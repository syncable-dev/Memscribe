//! Hermes Agent (NousResearch, github.com/NousResearch/hermes-agent) adapter.
//!
//! Hermes is **database-backed**: conversations live in a SQLite database,
//! `state.db`, opened in WAL mode. This adapter declares [`StoreReader::Native`]
//! and reads that store **read-only**, normalizing every message into the same
//! stable `{role, text, toolCalls, toolResults}` JSON record shape the pure
//! [`parse`](HermesAdapter::parse) already understands (same shape Cursor/VS
//! Code's native readers produce).
//!
//! ## On-disk store (2026-07 research: `hermes_constants.py`, `hermes_state.py`,
//! and the project's own docs — see MemCortex adapter audit)
//!
//! `{HERMES_HOME}/state.db`, where `HERMES_HOME` defaults to:
//! - macOS/Linux: `$HOME/.hermes` (both fall through the SAME branch in
//!   Hermes's own source — there is no darwin-specific path).
//! - Windows: `%LOCALAPPDATA%\hermes` — **NOT** `~/.hermes`. This is a
//!   different base directory entirely (no dot-prefix), a real trap for a
//!   naive `home_dir().join(".hermes")` implementation.
//! - `HERMES_HOME` env var, when set, overrides all of the above on every OS.
//!
//! Schema (WAL journal mode, falls back to `DELETE` mode if the filesystem
//! doesn't support WAL):
//! - `sessions(id TEXT PK, source, user_id, session_key, chat_id, model,
//!   model_config JSON, system_prompt, parent_session_id, started_at, ended_at,
//!   message_count, token/cost counters, title, archived, ...)`.
//! - `messages(id INTEGER PK AUTOINCREMENT, session_id FK, role, content,
//!   tool_call_id, tool_calls JSON string, tool_name, timestamp, token_count,
//!   finish_reason, reasoning fields, active)` — a standard OpenAI-chat-style
//!   shape: `role` is `system|user|assistant|tool`; an `assistant` row's
//!   `tool_calls` is a JSON array of `{id, name, arguments}` calls it made; a
//!   SEPARATE `tool`-role row (linked via `tool_call_id`) carries that call's
//!   result in `content`. `read_native` joins these back together per-session
//!   so a tool call and its result land on the SAME synthesized record (the
//!   shape `parse_message` already expects), rather than surfacing the
//!   `tool`-role row as its own disconnected turn.
//! - `messages_fts`/`messages_fts_trigram` — full-text search indexes, not
//!   conversation content; not read here.
//!
//! A LEGACY pre-`state.db` format wrote one JSONL file per session under
//! `{HERMES_HOME}/sessions/*.jsonl` — explicitly dead in current Hermes
//! ("no longer written or read" per the project's own docs) and NOT targeted
//! by this adapter; `{HERMES_HOME}/sessions/sessions.json` is a live routing
//! index (messaging-platform session key → session id), not a transcript, and
//! `{HERMES_HOME}/sessions/saved/*.json` are user-triggered exports, not the
//! source of truth. All three are left alone.
//!
//! Anything unrecognized-but-valid routes to [`memscribe_core::EventKind::Unknown`]
//! via [`util::unknown_event`], so the stream stays lossless. The parser never
//! panics; `read_native` opens SQLite read-only and never writes.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, Part,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, SourceLocation, StoreReader,
    TranscriptAdapter, TranscriptHandle, Usage,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SOURCE: SourceKind = SourceKind::Hermes;

/// Adapter for Hermes Agent transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct HermesAdapter;

impl TranscriptAdapter for HermesAdapter {
    fn source_kind(&self) -> SourceKind {
        SOURCE
    }

    /// Hermes keeps its conversation in a SQLite store, so the adapter reads
    /// the store itself via [`read_native`](HermesAdapter::read_native).
    fn store_reader(&self) -> StoreReader {
        StoreReader::Native
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        // HERMES_HOME (real process env var — Hermes's own
        // get_hermes_home()/_get_platform_default_hermes_home() checks this
        // FIRST, before any platform default) wins over everything.
        if let Some(home) = std::env::var("HERMES_HOME").ok().filter(|v| !v.is_empty()) {
            let db = PathBuf::from(home).join("state.db");
            return if db.is_file() {
                vec![handle_for(db)]
            } else {
                Vec::new()
            };
        }

        let home = cfg.home_dir();
        // Windows is NOT `~/.hermes` — Hermes's own source
        // (_get_platform_default_hermes_home) resolves to
        // %LOCALAPPDATA%\hermes on win32, a different base directory
        // entirely (no dot-prefix). macOS and Linux share the same
        // `$HOME/.hermes` branch in Hermes's own code (not OS-specific).
        let candidates = [
            home.join(".hermes").join("state.db"),           // macOS/Linux
            home.join("AppData/Local/hermes/state.db"),       // Windows
        ];
        candidates
            .into_iter()
            .find(|p| p.is_file())
            .map(|db| vec![handle_for(db)])
            .unwrap_or_default()
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        let Some(value) = util::parse_json_line(raw) else {
            let s = raw.as_str().map(str::trim).unwrap_or("");
            if s.is_empty() {
                return Ok(Vec::new());
            }
            return Ok(vec![util::unknown_event(
                SOURCE,
                ctx,
                raw,
                Value::String(s.to_string()),
            )]);
        };
        let Some(obj) = value.as_object() else {
            return Ok(vec![util::unknown_event(SOURCE, ctx, raw, value)]);
        };

        if let Some(kind) = obj.get("kind").and_then(Value::as_str) {
            if kind == "session_start" {
                return Ok(parse_session_start(obj, ctx, raw));
            }
        }
        if obj.get("role").is_some() {
            return Ok(parse_message(obj, ctx, raw));
        }
        Ok(vec![util::unknown_event(SOURCE, ctx, raw, value)])
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        match util::parse_json_line(sample).as_ref().and_then(Value::as_object) {
            Some(obj) if obj.get("kind").and_then(Value::as_str) == Some("session_start") => {
                SchemaVariant::certain(SOURCE, "hermes/session-v1")
            }
            Some(obj) if obj.get("role").is_some() => SchemaVariant::certain(SOURCE, "hermes/chat-v1"),
            Some(_) => SchemaVariant::unknown(SOURCE),
            None => SchemaVariant::unknown(SOURCE),
        }
    }

    fn read_native(&self, handle: &TranscriptHandle) -> Result<Vec<RawRecord>, ParseError> {
        read_state_db(&handle.path)
    }
}

fn handle_for(path: PathBuf) -> TranscriptHandle {
    TranscriptHandle {
        path,
        source: SOURCE,
        session_hint: None,
        compressed: false,
    }
}

/// Open Hermes's `state.db` strictly read-only and emit one synthetic
/// `session_start` + one record per dialogue message (assistant tool calls
/// joined with their matching `tool`-role result rows), in deterministic
/// (session, then message id) order.
fn read_native(path: &Path) -> rusqlite::Result<Vec<Value>> {
    let uri = format!(
        "file:{}?mode=ro&immutable=1",
        path.to_string_lossy().replace('\\', "/")
    );
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;

    let mut records = Vec::new();
    let mut sess_stmt = conn.prepare(
        "SELECT id, model, started_at, title FROM sessions ORDER BY id",
    )?;
    let sessions: Vec<(String, Option<String>, Option<String>, Option<String>)> = sess_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .filter_map(std::result::Result::ok)
        .collect();

    for (session_id, model, started_at, title) in sessions {
        records.push(json!({
            "kind": "session_start",
            "sessionId": session_id,
            "model": model,
            "ts": started_at,
            "title": title,
        }));

        let mut msg_stmt = conn.prepare(
            "SELECT id, role, content, tool_call_id, tool_calls, timestamp \
             FROM messages WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows: Vec<(i64, String, Option<String>, Option<String>, Option<String>, Option<String>)> =
            msg_stmt
                .query_map([&session_id], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })?
                .filter_map(std::result::Result::ok)
                .collect();

        // First pass: index `tool`-role rows by the call id they answer, so an
        // assistant row's tool calls can carry their results inline — matching
        // the canonical flat shape `parse_message` already expects, instead of
        // surfacing the `tool`-role row as its own disconnected turn.
        let mut tool_results: HashMap<String, String> = HashMap::new();
        for (_, role, content, tool_call_id, _, _) in &rows {
            if role == "tool" {
                if let (Some(id), Some(content)) = (tool_call_id, content) {
                    tool_results.insert(id.clone(), content.clone());
                }
            }
        }

        for (msg_id, role, content, _tool_call_id, tool_calls_json, ts) in rows {
            if role == "tool" {
                continue; // folded into its assistant call above
            }
            let mapped_role = match role.as_str() {
                "user" => "user",
                "assistant" => "assistant",
                other => other, // system / anything else -> Unknown downstream
            };
            let mut record = json!({
                "id": format!("{session_id}:{msg_id}"),
                "sessionId": session_id,
                "role": mapped_role,
                "text": content.unwrap_or_default(),
                "ts": ts,
            });

            if let Some(calls_json) = tool_calls_json {
                if let Ok(calls) = serde_json::from_str::<Vec<Value>>(&calls_json) {
                    let mut tool_calls = Vec::new();
                    let mut results = Vec::new();
                    for call in calls {
                        let Some(call_obj) = call.as_object() else { continue };
                        let id = call_obj
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("{session_id}:{msg_id}:call"));
                        let name = call_obj
                            .get("name")
                            .or_else(|| call_obj.get("function").and_then(|f| f.get("name")))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let args = call_obj
                            .get("arguments")
                            .or_else(|| call_obj.get("args"))
                            .or_else(|| call_obj.get("function").and_then(|f| f.get("arguments")))
                            .cloned()
                            .unwrap_or(Value::Null);
                        if let Some(output) = tool_results.get(&id) {
                            results.push(json!({ "id": id, "ok": true, "output": output }));
                        }
                        tool_calls.push(json!({ "id": id, "name": name, "args": args }));
                    }
                    if let Some(obj) = record.as_object_mut() {
                        obj.insert("toolCalls".to_string(), Value::Array(tool_calls));
                        if !results.is_empty() {
                            obj.insert("toolResults".to_string(), Value::Array(results));
                        }
                    }
                }
            }
            records.push(record);
        }
    }
    Ok(records)
}

fn read_state_db(path: &Path) -> Result<Vec<RawRecord>, ParseError> {
    let values = read_native(path).map_err(|e| ParseError::Io(e.to_string()))?;
    let path_str = path.to_string_lossy().into_owned();
    Ok(values
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            let line = serde_json::to_string(&v).unwrap_or_default();
            RawRecord::from_line(&line, SourceLocation::new(path_str.clone(), 0, i as u64 + 1))
        })
        .collect())
}

fn parse_session_start(
    obj: &serde_json::Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
    if let Some(sid) = obj.get("sessionId").and_then(Value::as_str) {
        ctx.session_id = Some(sid.to_string());
    }
    let event_id = obj
        .get("sessionId")
        .and_then(Value::as_str)
        .map(|s| format!("{s}:start"))
        .unwrap_or_else(|| content_id(&raw.bytes));
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    ctx.project = Some(ProjectRef::from_cwd("."));
    let ts = util::ts_from(&Value::Object(obj.clone()), &["ts", "timestamp", "started_at"]);
    vec![util::mk_event(
        SOURCE,
        ctx,
        raw,
        event_id,
        None,
        ts,
        EventKind::SessionStart {
            cwd: PathBuf::from("."),
            git: None::<GitRef>,
            model: obj.get("model").and_then(Value::as_str).map(str::to_string),
            tool_version: None,
        },
    )]
}

fn parse_message(
    obj: &serde_json::Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
    let role = obj.get("role").and_then(Value::as_str).unwrap_or("");
    let event_id = obj
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| content_id(&raw.bytes));
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = util::ts_from(&Value::Object(obj.clone()), &["ts", "timestamp"]);
    let text = obj.get("text").and_then(Value::as_str).unwrap_or("").to_string();

    let mut events = Vec::new();
    let kind = match role {
        "user" => EventKind::UserTurn {
            text: text.clone(),
            parts: vec![Part::Text { text }],
        },
        "assistant" => EventKind::AssistantTurn {
            text: text.clone(),
            thinking: None,
            model: None,
            usage: None::<Usage>,
            parts: vec![Part::Text { text }],
        },
        _ => return vec![util::unknown_event(SOURCE, ctx, raw, Value::Object(obj.clone()))],
    };
    events.push(util::mk_event(SOURCE, ctx, raw, event_id.clone(), None, ts, kind));

    if let Some(calls) = obj.get("toolCalls").and_then(Value::as_array) {
        let results = obj.get("toolResults").and_then(Value::as_array);
        for (i, call) in calls.iter().enumerate() {
            let Some(call_obj) = call.as_object() else { continue };
            let call_id = call_obj
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{event_id}:tool:{i}"));
            let name = call_obj.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let args = call_obj.get("args").cloned().unwrap_or(Value::Null);
            let call_ev_id = format!("{call_id}:call");
            if ctx.first_seen(&call_ev_id) {
                events.push(util::mk_event(
                    SOURCE,
                    ctx,
                    raw,
                    call_ev_id,
                    Some(event_id.clone()),
                    ts,
                    EventKind::ToolCall { call_id: call_id.clone(), name, args },
                ));
            }

            if let Some(result) = results.and_then(|rs| {
                rs.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(call_id.as_str()))
            }) {
                let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(true);
                let output = result.get("output").cloned().unwrap_or(Value::Null);
                ctx.call_ok.insert(call_id.clone(), ok);
                let result_ev_id = format!("{call_id}:result");
                if ctx.first_seen(&result_ev_id) {
                    events.push(util::mk_event(
                        SOURCE,
                        ctx,
                        raw,
                        result_ev_id,
                        Some(event_id.clone()),
                        ts,
                        EventKind::ToolResult { call_id, ok, output },
                    ));
                }
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn parse_all(lines: &[&str]) -> (Vec<CaptureEvent>, ParseCtx) {
        let adapter = HermesAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            let r = RawRecord::from_line(l, SourceLocation::new("hermes.jsonl", 0, i as u64 + 1));
            out.extend(adapter.parse(&r, &mut ctx).expect("never errors"));
        }
        (out, ctx)
    }

    fn tags(evs: &[CaptureEvent]) -> Vec<&'static str> {
        evs.iter().map(|e| e.kind.tag()).collect()
    }

    #[test]
    fn discover_prefers_hermes_home_env_over_default() {
        let tmp = tempfile::tempdir().unwrap();
        let custom = tmp.path().join("custom-hermes-home");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("state.db"), b"").unwrap();

        std::env::set_var("HERMES_HOME", &custom);
        let cfg = DiscoverCfg { home: Some(tmp.path().to_path_buf()), ..Default::default() };
        let handles = HermesAdapter.discover(&cfg);
        std::env::remove_var("HERMES_HOME");

        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].path, custom.join("state.db"));
    }

    #[test]
    fn discover_finds_windows_localappdata_not_dot_hermes() {
        // Hermes's own source resolves Windows to %LOCALAPPDATA%\hermes, NOT
        // ~/.hermes — a real trap for a naive home_dir().join(".hermes").
        let tmp = tempfile::tempdir().unwrap();
        let win_db = tmp.path().join("AppData/Local/hermes/state.db");
        std::fs::create_dir_all(win_db.parent().unwrap()).unwrap();
        std::fs::write(&win_db, b"").unwrap();

        let cfg = DiscoverCfg { home: Some(tmp.path().to_path_buf()), ..Default::default() };
        let handles = HermesAdapter.discover(&cfg);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].path, win_db);
    }

    #[test]
    fn discover_finds_macos_linux_dot_hermes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".hermes/state.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"").unwrap();

        let cfg = DiscoverCfg { home: Some(tmp.path().to_path_buf()), ..Default::default() };
        let handles = HermesAdapter.discover(&cfg);
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].path, db);
    }

    #[test]
    fn real_schema_session_then_user_then_assistant_with_tool_call_and_result() {
        // Format-accurate reconstruction of the real sessions/messages schema
        // (chat-completions-style: assistant.tool_calls JSON array, a separate
        // tool-role row carrying the result via tool_call_id) — read_native
        // must fold the tool-role row's content into the assistant's
        // toolResults, not surface it as its own disconnected turn.
        let session_start = r#"{"kind":"session_start","sessionId":"s1","model":"hermes-3-70b","ts":"2026-06-22T10:00:00Z"}"#;
        let user = r#"{"id":"s1:1","sessionId":"s1","role":"user","text":"Switch the config loader to Postgres.","ts":"2026-06-22T10:00:00Z"}"#;
        let assistant = r#"{"id":"s1:2","sessionId":"s1","role":"assistant","text":"Switching to Postgres.","ts":"2026-06-22T10:00:05Z","toolCalls":[{"id":"call_1","name":"edit_file","args":{"path":"config.toml"}}],"toolResults":[{"id":"call_1","ok":true,"output":"applied 1 edit"}]}"#;

        let (events, ctx) = parse_all(&[session_start, user, assistant]);
        assert_eq!(ctx.session_id.as_deref(), Some("s1"));
        assert_eq!(
            tags(&events),
            ["session_start", "user_turn", "assistant_turn", "tool_call", "tool_result"]
        );
        assert!(matches!(&events[3].kind, EventKind::ToolCall { name, .. } if name == "edit_file"));
        assert!(matches!(&events[4].kind, EventKind::ToolResult { ok: true, .. }));
    }

    #[test]
    fn read_native_joins_tool_role_result_into_assistant_call() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, started_at TEXT, title TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT,
               role TEXT, content TEXT, tool_call_id TEXT, tool_calls TEXT, timestamp TEXT);
             INSERT INTO sessions VALUES ('s1', 'hermes-3-70b', '2026-06-22T10:00:00Z', 'Postgres migration');
             INSERT INTO messages (session_id, role, content, timestamp)
               VALUES ('s1', 'user', 'Switch to Postgres', '2026-06-22T10:00:00Z');
             INSERT INTO messages (session_id, role, content, tool_calls, timestamp)
               VALUES ('s1', 'assistant', 'Switching now.', '[{\"id\":\"call_1\",\"name\":\"edit_file\",\"arguments\":{\"path\":\"config.toml\"}}]', '2026-06-22T10:00:05Z');
             INSERT INTO messages (session_id, role, content, tool_call_id, timestamp)
               VALUES ('s1', 'tool', 'applied 1 edit', 'call_1', '2026-06-22T10:00:06Z');",
        )
        .unwrap();
        drop(conn);

        let handle = TranscriptHandle {
            path: db_path,
            source: SOURCE,
            session_hint: None,
            compressed: false,
        };
        let records = HermesAdapter.read_native(&handle).expect("read ok");
        let mut ctx = ParseCtx::new();
        let mut events = Vec::new();
        for r in &records {
            events.extend(HermesAdapter.parse(r, &mut ctx).expect("parse ok"));
        }
        assert_eq!(
            tags(&events),
            ["session_start", "user_turn", "assistant_turn", "tool_call", "tool_result"]
        );
        // The tool-role row must NOT surface as its own separate turn.
        assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::UserTurn { text, .. } if text == "applied 1 edit")));
        assert!(matches!(&events[4].kind, EventKind::ToolResult { ok: true, .. }));
    }

    #[test]
    fn garbage_never_panics_and_is_lossless() {
        let (events, _) = parse_all(&["not json", "", r#"{"unrelated":true}"#]);
        assert_eq!(tags(&events), ["unknown", "unknown"]);
    }
}
