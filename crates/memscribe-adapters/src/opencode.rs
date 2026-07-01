//! OpenCode (`github.com/sst/opencode`, now published from `anomalyco/opencode`)
//! adapter.
//!
//! OpenCode is **database-backed**: sessions/messages/parts live in a SQLite
//! database, `opencode.db`, managed with Drizzle ORM (WAL mode). This adapter
//! declares [`StoreReader::Native`] and reads that store **read-only**,
//! normalizing every message + its parts into the same stable
//! `{role, text, toolCalls, toolResults}` JSON record shape the pure
//! [`parse`](OpenCodeAdapter::parse) already understands.
//!
//! ## On-disk store (2026-07 research: verified directly against
//! `packages/core/src/global.ts`, `packages/core/src/database/database.ts`,
//! and `packages/core/src/session/sql.ts` / `packages/schema/src/v1/session.ts`
//! in the live `anomalyco/opencode` `dev` branch — not secondhand docs)
//!
//! Path: `{XDG_DATA_HOME}/opencode/opencode.db`. OpenCode resolves its data
//! directory via the `xdg-basedir` npm package, which does **not** special-case
//! macOS or Windows — every OS falls through to the same
//! `$XDG_DATA_HOME || $HOME/.local/share` logic. So unlike most Electron/VS
//! Code-family tools, the path is **uniform across macOS, Linux, and Windows**:
//! `~/.local/share/opencode/opencode.db` (or `%XDG_DATA_HOME%\opencode\opencode.db`
//! / `$HOME/.local/share/opencode/opencode.db` on Windows, since `xdg-basedir`
//! joins onto `os.homedir()` there too, not `%APPDATA%`). `OPENCODE_DB`, when
//! set to an absolute path, overrides the file entirely (`:memory:` is also a
//! legal value there — never dereferenced as a path).
//!
//! Non-default install channels (beta/dev builds) use `opencode-<channel>.db`
//! instead of `opencode.db`; this adapter only targets the default/stable
//! filename, matching what most real installs actually run.
//!
//! Schema (three tables, all `snake_case`, Drizzle-managed migrations):
//! - `session(id, project_id, workspace_id, parent_id, slug, directory, path,
//!   title, version, ..., time_created, time_updated)`.
//! - `message(id, session_id, time_created, time_updated, data JSON)` — `data`
//!   is the serialized `SessionV1.Info` union: a `User` message (`role:"user"`,
//!   `time.created`, `agent`, `model`) or `Assistant` message (`role:"assistant"`,
//!   `time.created`, `cost`, `tokens`, `error?`). **Neither variant carries the
//!   turn's text or tool calls directly** — those live in separate `part` rows.
//! - `part(id, message_id, session_id, time_created, time_updated, data JSON)` —
//!   `data` is the serialized `Part` union, discriminated on `type`: `text`
//!   (`text`), `tool` (`callID`, `tool`, `state: {status, input, output?,
//!   error?}`), `file`, `reasoning`, `subtask`, `step-start`/`step-finish`,
//!   `snapshot`, `patch`, `agent`, `retry`, `compaction`. `read_native` joins a
//!   message's parts back onto it (ordered by `part.id`, which is a
//!   monotonically-increasing `prt_<ksuid>` so id order is time order) to
//!   reconstruct the flat `{text, toolCalls, toolResults}` shape `parse_message`
//!   expects — a message row alone is metadata-only and would silently look
//!   empty without this join.
//!
//! OpenCode's own schema is under **heavy, near-daily migration churn** on the
//! `dev` branch (an in-flight rewrite is even moving message/part access off
//! the legacy `storage/db.ts` wrapper) — the table/column shapes here reflect
//! the durable, versioned `V1` message/part schema the project explicitly
//! preserves for backward compatibility, not the newer event-sourced
//! `session_message` log tables that coexist alongside it. Anything
//! unrecognized-but-valid routes to [`memscribe_core::EventKind::Unknown`] via
//! [`util::unknown_event`], so schema drift degrades gracefully instead of
//! silently dropping data. `read_native` opens SQLite read-only and never writes.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, Part,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, SourceLocation, StoreReader,
    TranscriptAdapter, TranscriptHandle, Usage,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const SOURCE: SourceKind = SourceKind::OpenCode;

/// Adapter for OpenCode (SST) transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenCodeAdapter;

impl TranscriptAdapter for OpenCodeAdapter {
    fn source_kind(&self) -> SourceKind {
        SOURCE
    }

    /// OpenCode keeps its conversation in a SQLite store, so the adapter reads
    /// the store itself via [`read_native`](OpenCodeAdapter::read_native).
    fn store_reader(&self) -> StoreReader {
        StoreReader::Native
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        // OPENCODE_DB (real env var — OpenCode's own Database.path() checks
        // this FIRST) overrides the file entirely when it's an absolute path.
        // ":memory:" is a legal value there too; skip it, there's nothing on
        // disk to discover.
        if let Some(db) = std::env::var("OPENCODE_DB").ok().filter(|v| !v.is_empty()) {
            if db != ":memory:" {
                let p = PathBuf::from(&db);
                if p.is_absolute() {
                    return if p.is_file() { vec![handle_for(p)] } else { Vec::new() };
                }
            }
        }

        // xdg-basedir does NOT special-case macOS/Windows: every OS resolves
        // via $XDG_DATA_HOME || $HOME/.local/share, so the path is uniform.
        let data_home = std::env::var("XDG_DATA_HOME")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| cfg.home_dir().join(".local/share"));
        let db = data_home.join("opencode").join("opencode.db");
        if db.is_file() {
            vec![handle_for(db)]
        } else {
            Vec::new()
        }
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
                SchemaVariant::certain(SOURCE, "opencode/session-v1")
            }
            Some(obj) if obj.get("role").is_some() => SchemaVariant::certain(SOURCE, "opencode/message-v1"),
            Some(_) => SchemaVariant::unknown(SOURCE),
            None => SchemaVariant::unknown(SOURCE),
        }
    }

    fn read_native(&self, handle: &TranscriptHandle) -> Result<Vec<RawRecord>, ParseError> {
        read_opencode_db(&handle.path)
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

/// Open OpenCode's `opencode.db` strictly read-only and emit one synthetic
/// `session_start` + one record per message (its parts joined back onto it,
/// ordered by `part.id` — a monotonically-increasing `prt_<ksuid>`, so id
/// order is time order), in deterministic (session, then message id) order.
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
        "SELECT id, title, time_created FROM session ORDER BY id",
    )?;
    let sessions: Vec<(String, Option<String>, Option<i64>)> = sess_stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .filter_map(std::result::Result::ok)
        .collect();

    for (session_id, title, started_at) in sessions {
        records.push(json!({
            "kind": "session_start",
            "sessionId": session_id,
            "ts": started_at,
            "title": title,
        }));

        let mut msg_stmt = conn.prepare(
            "SELECT id, data, time_created FROM message WHERE session_id = ?1 ORDER BY id",
        )?;
        let messages: Vec<(String, String, Option<i64>)> = msg_stmt
            .query_map([&session_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .filter_map(std::result::Result::ok)
            .collect();

        let mut part_stmt = conn.prepare(
            "SELECT id, message_id, data FROM part WHERE session_id = ?1 ORDER BY id",
        )?;
        let parts: Vec<(String, String, String)> = part_stmt
            .query_map([&session_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .filter_map(std::result::Result::ok)
            .collect();

        for (msg_id, data_json, ts) in messages {
            let Ok(data) = serde_json::from_str::<Value>(&data_json) else { continue };
            let Some(role) = data.get("role").and_then(Value::as_str) else { continue };

            let mut text_buf = String::new();
            let mut tool_calls = Vec::new();
            let mut tool_results = Vec::new();
            for (_, part_msg_id, part_json) in &parts {
                if part_msg_id != &msg_id {
                    continue;
                }
                let Ok(part) = serde_json::from_str::<Value>(part_json) else { continue };
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !text_buf.is_empty() {
                                text_buf.push('\n');
                            }
                            text_buf.push_str(t);
                        }
                    }
                    Some("tool") => {
                        let call_id = part
                            .get("callID")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let name = part.get("tool").and_then(Value::as_str).unwrap_or_default().to_string();
                        let state = part.get("state").cloned().unwrap_or(Value::Null);
                        let input = state.get("input").cloned().unwrap_or(Value::Null);
                        tool_calls.push(json!({ "id": call_id, "name": name, "args": input }));
                        match state.get("status").and_then(Value::as_str) {
                            Some("completed") => {
                                let output = state.get("output").cloned().unwrap_or(Value::Null);
                                tool_results.push(json!({ "id": call_id, "ok": true, "output": output }));
                            }
                            Some("error") => {
                                let output = state.get("error").cloned().unwrap_or(Value::Null);
                                tool_results.push(json!({ "id": call_id, "ok": false, "output": output }));
                            }
                            _ => {} // pending/running: no result yet
                        }
                    }
                    _ => {} // file/reasoning/subtask/step-*/snapshot/patch/agent/retry/compaction: not turn content
                }
            }

            let mut record = json!({
                "id": format!("{session_id}:{msg_id}"),
                "sessionId": session_id,
                "role": role,
                "text": text_buf,
                "ts": ts,
            });
            if let Some(obj) = record.as_object_mut() {
                if !tool_calls.is_empty() {
                    obj.insert("toolCalls".to_string(), Value::Array(tool_calls));
                }
                if !tool_results.is_empty() {
                    obj.insert("toolResults".to_string(), Value::Array(tool_results));
                }
            }
            records.push(record);
        }
    }
    Ok(records)
}

fn read_opencode_db(path: &Path) -> Result<Vec<RawRecord>, ParseError> {
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
    let ts = util::ts_from(&Value::Object(obj.clone()), &["ts", "timestamp"]);
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
            model: None,
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
        let adapter = OpenCodeAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            let r = RawRecord::from_line(l, SourceLocation::new("opencode.jsonl", 0, i as u64 + 1));
            out.extend(adapter.parse(&r, &mut ctx).expect("never errors"));
        }
        (out, ctx)
    }

    fn tags(evs: &[CaptureEvent]) -> Vec<&'static str> {
        evs.iter().map(|e| e.kind.tag()).collect()
    }

    #[test]
    fn discover_prefers_opencode_db_env_override_when_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let custom = tmp.path().join("custom.db");
        std::fs::write(&custom, b"").unwrap();

        std::env::set_var("OPENCODE_DB", &custom);
        let cfg = DiscoverCfg { home: Some(tmp.path().to_path_buf()), ..Default::default() };
        let handles = OpenCodeAdapter.discover(&cfg);
        std::env::remove_var("OPENCODE_DB");

        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].path, custom);
    }

    #[test]
    fn discover_ignores_memory_db_override() {
        std::env::set_var("OPENCODE_DB", ":memory:");
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"").unwrap();

        let cfg = DiscoverCfg { home: Some(tmp.path().to_path_buf()), ..Default::default() };
        let handles = OpenCodeAdapter.discover(&cfg);
        std::env::remove_var("OPENCODE_DB");

        // ":memory:" is not a real file — falls through to the default XDG path.
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].path, db);
    }

    #[test]
    fn discover_is_uniform_across_platforms_via_xdg_data_home() {
        // xdg-basedir does NOT special-case macOS/Windows; confirm the
        // adapter honors XDG_DATA_HOME the same way on every OS.
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("custom-xdg-data");
        let db = xdg.join("opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"").unwrap();

        std::env::set_var("XDG_DATA_HOME", &xdg);
        let cfg = DiscoverCfg { home: Some(tmp.path().to_path_buf()), ..Default::default() };
        let handles = OpenCodeAdapter.discover(&cfg);
        std::env::remove_var("XDG_DATA_HOME");

        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].path, db);
    }

    #[test]
    fn discover_falls_back_to_dot_local_share() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"").unwrap();

        std::env::remove_var("XDG_DATA_HOME");
        let cfg = DiscoverCfg { home: Some(tmp.path().to_path_buf()), ..Default::default() };
        let handles = OpenCodeAdapter.discover(&cfg);

        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].path, db);
    }

    #[test]
    fn real_schema_message_row_alone_is_metadata_only_parts_carry_content() {
        // Format-accurate reconstruction of the real message/part V1 schema
        // (message.data has NO text/tool fields — content lives in separate
        // part rows joined by message_id). read_native must do that join;
        // this test exercises the post-join flat shape it produces.
        let session_start = r#"{"kind":"session_start","sessionId":"ses_1","ts":1771700000000,"title":"Add Postgres support"}"#;
        let user = r#"{"id":"ses_1:msg_1","sessionId":"ses_1","role":"user","text":"Switch the config loader to Postgres.","ts":1771700000000}"#;
        let assistant = r#"{"id":"ses_1:msg_2","sessionId":"ses_1","role":"assistant","text":"Switching to Postgres.","ts":1771700005000,"toolCalls":[{"id":"call_1","name":"edit","args":{"path":"config.toml"}}],"toolResults":[{"id":"call_1","ok":true,"output":"applied 1 edit"}]}"#;

        let (events, ctx) = parse_all(&[session_start, user, assistant]);
        assert_eq!(ctx.session_id.as_deref(), Some("ses_1"));
        assert_eq!(
            tags(&events),
            ["session_start", "user_turn", "assistant_turn", "tool_call", "tool_result"]
        );
        assert!(matches!(&events[3].kind, EventKind::ToolCall { name, .. } if name == "edit"));
        assert!(matches!(&events[4].kind, EventKind::ToolResult { ok: true, .. }));
    }

    #[test]
    fn read_native_joins_text_and_tool_parts_onto_their_message() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, time_created INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);
             INSERT INTO session VALUES ('ses_1', 'Postgres migration', 1771700000000);
             INSERT INTO message VALUES ('msg_1', 'ses_1',
               '{\"id\":\"msg_1\",\"sessionID\":\"ses_1\",\"role\":\"user\",\"time\":{\"created\":1771700000000},\"agent\":\"build\",\"model\":{\"providerID\":\"anthropic\",\"modelID\":\"claude\"}}',
               1771700000000);
             INSERT INTO part VALUES ('prt_1', 'msg_1', 'ses_1',
               '{\"id\":\"prt_1\",\"sessionID\":\"ses_1\",\"messageID\":\"msg_1\",\"type\":\"text\",\"text\":\"Switch to Postgres\"}');
             INSERT INTO message VALUES ('msg_2', 'ses_1',
               '{\"id\":\"msg_2\",\"sessionID\":\"ses_1\",\"role\":\"assistant\",\"time\":{\"created\":1771700005000},\"parentID\":\"msg_1\",\"modelID\":\"claude\",\"providerID\":\"anthropic\",\"mode\":\"build\",\"agent\":\"build\",\"path\":{\"cwd\":\"/repo\",\"root\":\"/repo\"},\"cost\":0,\"tokens\":{\"input\":1,\"output\":1,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}',
               1771700005000);
             INSERT INTO part VALUES ('prt_2', 'msg_2', 'ses_1',
               '{\"id\":\"prt_2\",\"sessionID\":\"ses_1\",\"messageID\":\"msg_2\",\"type\":\"text\",\"text\":\"Switching now.\"}');
             INSERT INTO part VALUES ('prt_3', 'msg_2', 'ses_1',
               '{\"id\":\"prt_3\",\"sessionID\":\"ses_1\",\"messageID\":\"msg_2\",\"type\":\"tool\",\"callID\":\"call_1\",\"tool\":\"edit\",\"state\":{\"status\":\"completed\",\"input\":{\"path\":\"config.toml\"},\"output\":\"applied 1 edit\",\"title\":\"edit\",\"metadata\":{},\"time\":{\"start\":0,\"end\":1}}}');",
        )
        .unwrap();
        drop(conn);

        let handle = TranscriptHandle {
            path: db_path,
            source: SOURCE,
            session_hint: None,
            compressed: false,
        };
        let records = OpenCodeAdapter.read_native(&handle).expect("read ok");
        let mut ctx = ParseCtx::new();
        let mut events = Vec::new();
        for r in &records {
            events.extend(OpenCodeAdapter.parse(r, &mut ctx).expect("parse ok"));
        }
        assert_eq!(
            tags(&events),
            ["session_start", "user_turn", "assistant_turn", "tool_call", "tool_result"]
        );
        assert!(matches!(&events[1].kind, EventKind::UserTurn { text, .. } if text == "Switch to Postgres"));
        assert!(matches!(&events[2].kind, EventKind::AssistantTurn { text, .. } if text == "Switching now."));
        assert!(matches!(&events[4].kind, EventKind::ToolResult { ok: true, .. }));
    }

    #[test]
    fn message_row_with_no_matching_parts_yields_empty_text_not_a_crash() {
        // A message row with zero part rows (e.g. an in-flight/aborted turn)
        // must not panic and must not silently vanish — it still surfaces as
        // a turn, just with empty text, per the lossless-by-default contract.
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, time_created INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);
             INSERT INTO session VALUES ('ses_1', 'Empty turn', 1771700000000);
             INSERT INTO message VALUES ('msg_1', 'ses_1',
               '{\"id\":\"msg_1\",\"sessionID\":\"ses_1\",\"role\":\"user\",\"time\":{\"created\":1771700000000},\"agent\":\"build\",\"model\":{\"providerID\":\"a\",\"modelID\":\"b\"}}',
               1771700000000);",
        )
        .unwrap();
        drop(conn);

        let handle = TranscriptHandle { path: db_path, source: SOURCE, session_hint: None, compressed: false };
        let records = OpenCodeAdapter.read_native(&handle).expect("read ok");
        let mut ctx = ParseCtx::new();
        let mut events = Vec::new();
        for r in &records {
            events.extend(OpenCodeAdapter.parse(r, &mut ctx).expect("parse ok"));
        }
        assert_eq!(tags(&events), ["session_start", "user_turn"]);
        assert!(matches!(&events[1].kind, EventKind::UserTurn { text, .. } if text.is_empty()));
    }

    #[test]
    fn garbage_never_panics_and_is_lossless() {
        let (events, _) = parse_all(&["not json", "", r#"{"unrelated":true}"#]);
        assert_eq!(tags(&events), ["unknown", "unknown"]);
    }
}
