//! Cursor adapter.
//!
//! Cursor is a VS Code fork; live chat state lives in the per-workspace
//! `state.vscdb` SQLite store under
//! `~/Library/Application Support/Cursor/User/workspaceStorage/<hash>/` (and,
//! on newer builds, under `~/.cursor/`). That binary store is undocumented, so
//! the first deterministic model targets an **exported JSON-lines** transcript
//! with a stable `{role, text, ...}` shape; a SQLite reader can be layered into
//! `memscribe-io` later. This parser pattern-matches the fields it needs and
//! routes anything unrecognized to [`EventKind::Unknown`] so the stream stays
//! lossless across Cursor-version churn.
//!
//! Record shape (one JSON object per line):
//! - leading `{"kind":"session_start","cwd":..,"git":{"sha","branch"},
//!   "toolVersion":..,"sessionId":..}` → [`EventKind::SessionStart`] and seeds
//!   `ctx.project` / `ctx.session_id`.
//! - message records
//!   `{"id","parentId","role":"user"|"assistant","ts","sessionId","text",
//!   "model","usage":{"input","output"},"toolCalls":[..],"toolResults":[..],
//!   "edits":[..]}`. One record expands to multiple events, in a stable order:
//!   the turn (`UserTurn` / `AssistantTurn`), then each `ToolCall`, each
//!   `ToolResult`, then each `FileEdit`.
//!
//! `event_id` = the record's native `id`, else a `blake3(content)` fallback.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, Part,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, TranscriptAdapter, TranscriptHandle, Usage,
};
use serde_json::Value;
use std::path::PathBuf;

const SRC: SourceKind = SourceKind::Cursor;

/// Adapter for Cursor transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct CursorAdapter;

impl TranscriptAdapter for CursorAdapter {
    fn source_kind(&self) -> SourceKind {
        SRC
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        let mut out = Vec::new();
        let home = cfg.home_dir();
        // Point at the real product locations. We do not parse the binary store
        // in this model, but discovery should surface where it lives so the
        // runtime can wire a SQLite reader without re-deriving these paths.
        let roots = [
            home.join("Library/Application Support/Cursor/User/workspaceStorage"),
            home.join(".cursor"),
        ];
        for root in roots {
            for entry in walkdir::WalkDir::new(&root)
                .max_depth(3)
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                let is_store = name == "state.vscdb"
                    || name.ends_with(".jsonl")
                    || name.ends_with(".cursorchat");
                if is_store && path.is_file() {
                    let session_hint = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .map(str::to_string);
                    out.push(TranscriptHandle {
                        path: path.to_path_buf(),
                        source: SRC,
                        session_hint,
                        compressed: false,
                    });
                }
            }
        }
        // Deterministic order regardless of filesystem iteration order.
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        // Blank lines yield nothing; invalid JSON is preserved verbatim as an
        // Unknown so the stream is still lossless (never an error here).
        let value = match util::parse_json_line(raw) {
            Some(v) => v,
            None => {
                let s = raw.as_str().map(str::trim).unwrap_or("");
                if s.is_empty() {
                    return Ok(Vec::new());
                }
                return Ok(vec![util::unknown_event(
                    SRC,
                    ctx,
                    raw,
                    Value::String(s.to_string()),
                )]);
            }
        };

        // We only know how to parse JSON objects; anything else is Unknown.
        let obj = match value.as_object() {
            Some(o) => o,
            None => return Ok(vec![util::unknown_event(SRC, ctx, raw, value)]),
        };

        // Seed session id from any record that carries one (records are parsed
        // in file order, so the first one wins for the whole stream).
        if ctx.session_id.is_none() {
            if let Some(sid) = str_field(obj, "sessionId") {
                ctx.session_id = Some(sid.to_string());
            }
        }

        // Dispatch on the record discriminator. A `kind` of `session_start`
        // (and a couple of tolerant aliases) means the session header; a `role`
        // means a dialogue turn. Everything else is Unknown.
        if let Some(kind) = str_field(obj, "kind") {
            match kind {
                "session_start" | "session-start" | "sessionStart" => {
                    return Ok(parse_session_start(obj, ctx, raw));
                }
                "session_end" | "session-end" | "sessionEnd" => {
                    return Ok(parse_session_end(obj, ctx, raw));
                }
                _ => {}
            }
        }

        if str_field(obj, "role").is_some() {
            return Ok(parse_message(obj, ctx, raw));
        }

        Ok(vec![util::unknown_event(SRC, ctx, raw, value)])
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        match util::parse_json_line(sample)
            .as_ref()
            .and_then(Value::as_object)
        {
            Some(obj)
                if obj.contains_key("role")
                    || matches!(str_field(obj, "kind"), Some("session_start")) =>
            {
                SchemaVariant::certain(SRC, "cursor/export-v1")
            }
            _ => SchemaVariant::unknown(SRC),
        }
    }
}

/// Parse a `session_start` header: seeds `ctx.project` and emits `SessionStart`.
fn parse_session_start(
    obj: &serde_json::Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
    let cwd_str = str_field(obj, "cwd").unwrap_or(".");
    let cwd = PathBuf::from(cwd_str);
    let git = parse_git(obj.get("git"));
    let model = str_field(obj, "model").map(str::to_string);
    let tool_version = str_field(obj, "toolVersion")
        .or_else(|| str_field(obj, "tool_version"))
        .map(str::to_string);

    // Bind the project for every subsequent event in this session.
    ctx.project = Some(ProjectRef {
        cwd: cwd.clone(),
        repo_root: str_field(obj, "repoRoot")
            .or_else(|| str_field(obj, "repo_root"))
            .map(PathBuf::from),
        git: git.clone(),
    });

    let event_id = event_id_for(obj, raw);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = ts_for(obj);
    vec![util::mk_event(
        SRC,
        ctx,
        raw,
        event_id,
        None,
        ts,
        EventKind::SessionStart {
            cwd,
            git,
            model,
            tool_version,
        },
    )]
}

/// Parse a `session_end` header into a `SessionEnd` event.
fn parse_session_end(
    obj: &serde_json::Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
    let event_id = event_id_for(obj, raw);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = ts_for(obj);
    let reason = str_field(obj, "reason").map(str::to_string);
    vec![util::mk_event(
        SRC,
        ctx,
        raw,
        event_id,
        parent_field(obj),
        ts,
        EventKind::SessionEnd { reason },
    )]
}

/// Parse a dialogue record into the turn event plus any embedded tool calls,
/// tool results, and file edits — in a stable, deterministic order.
fn parse_message(
    obj: &serde_json::Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
    let base_id = event_id_for(obj, raw);
    // Idempotency: a repeated record (same id) yields nothing.
    if !ctx.first_seen(&base_id) {
        return Vec::new();
    }

    let ts = ts_for(obj);
    let parent = parent_field(obj);
    let role = str_field(obj, "role").unwrap_or("");
    let text = str_field(obj, "text").unwrap_or("").to_string();

    let mut events = Vec::new();

    // 1) The turn itself.
    let turn_kind = match role {
        "user" => EventKind::UserTurn {
            text,
            parts: text_parts(obj),
        },
        "assistant" => EventKind::AssistantTurn {
            text,
            thinking: str_field(obj, "thinking").map(str::to_string),
            model: str_field(obj, "model").map(str::to_string),
            usage: parse_usage(obj.get("usage")),
            parts: text_parts(obj),
        },
        _ => {
            // A role we don't recognize → Unknown, but still keep ordering.
            EventKind::Unknown {
                raw_type: role.to_string(),
                raw: Value::Object(obj.clone()),
            }
        }
    };
    events.push(util::mk_event(
        SRC,
        ctx,
        raw,
        base_id.clone(),
        parent.clone(),
        ts,
        turn_kind,
    ));

    // 2) Tool calls. Each gets a synthetic, deterministic id derived from the
    //    turn id + the call id so it never collides with the turn or siblings.
    if let Some(calls) = obj.get("toolCalls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            let call_obj = match call.as_object() {
                Some(o) => o,
                None => continue,
            };
            let call_id = str_field(call_obj, "id")
                .map(str::to_string)
                .unwrap_or_else(|| format!("{base_id}:call:{i}"));
            let name = str_field(call_obj, "name").unwrap_or("").to_string();
            let args = call_obj.get("args").cloned().unwrap_or(Value::Null);
            // Remember the name so a later result can be paired by call_id.
            ctx.call_names.insert(call_id.clone(), name.clone());
            let ev_id = format!("{base_id}#toolcall:{call_id}");
            if !ctx.first_seen(&ev_id) {
                continue;
            }
            events.push(util::mk_event(
                SRC,
                ctx,
                raw,
                ev_id,
                Some(base_id.clone()),
                ts,
                EventKind::ToolCall {
                    call_id,
                    name,
                    args,
                },
            ));
        }
    }

    // 3) Tool results.
    if let Some(results) = obj.get("toolResults").and_then(Value::as_array) {
        for (i, res) in results.iter().enumerate() {
            let res_obj = match res.as_object() {
                Some(o) => o,
                None => continue,
            };
            let call_id = str_field(res_obj, "id")
                .map(str::to_string)
                .unwrap_or_else(|| format!("{base_id}:result:{i}"));
            let ok = bool_field(res_obj, "ok").unwrap_or(true);
            ctx.call_ok.insert(call_id.clone(), ok);
            let output = res_obj.get("output").cloned().unwrap_or(Value::Null);
            let ev_id = format!("{base_id}#toolresult:{call_id}");
            if !ctx.first_seen(&ev_id) {
                continue;
            }
            events.push(util::mk_event(
                SRC,
                ctx,
                raw,
                ev_id,
                Some(base_id.clone()),
                ts,
                EventKind::ToolResult {
                    call_id,
                    ok,
                    output,
                },
            ));
        }
    }

    // 4) File edits.
    if let Some(edits) = obj.get("edits").and_then(Value::as_array) {
        for (i, edit) in edits.iter().enumerate() {
            let edit_obj = match edit.as_object() {
                Some(o) => o,
                None => continue,
            };
            let path = str_field(edit_obj, "path").unwrap_or("").to_string();
            let diff = Diff {
                path: PathBuf::from(path),
                old: str_field(edit_obj, "oldText").map(str::to_string),
                new: str_field(edit_obj, "newText").map(str::to_string),
                unified: str_field(edit_obj, "diff").map(str::to_string),
                added_lines: u32_field(edit_obj, "added").unwrap_or(0),
                removed_lines: u32_field(edit_obj, "removed").unwrap_or(0),
            };
            let call_id = str_field(edit_obj, "callId")
                .or_else(|| str_field(edit_obj, "call_id"))
                .map(str::to_string);
            let ev_id = format!("{base_id}#edit:{i}");
            if !ctx.first_seen(&ev_id) {
                continue;
            }
            events.push(util::mk_event(
                SRC,
                ctx,
                raw,
                ev_id,
                Some(base_id.clone()),
                ts,
                EventKind::FileEdit { call_id, diff },
            ));
        }
    }

    events
}

/// Build text/thinking [`Part`]s from a message (best-effort, never fails).
fn text_parts(obj: &serde_json::Map<String, Value>) -> Vec<Part> {
    let mut parts = Vec::new();
    if let Some(t) = str_field(obj, "text") {
        if !t.is_empty() {
            parts.push(Part::Text {
                text: t.to_string(),
            });
        }
    }
    if let Some(th) = str_field(obj, "thinking") {
        if !th.is_empty() {
            parts.push(Part::Thinking {
                text: th.to_string(),
            });
        }
    }
    parts
}

/// Parse `usage:{input,output}` (also tolerant of token-suffixed keys).
fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let obj = value?.as_object()?;
    let input = u64_field(obj, "input").or_else(|| u64_field(obj, "input_tokens"));
    let output = u64_field(obj, "output").or_else(|| u64_field(obj, "output_tokens"));
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: None,
        cache_creation_tokens: None,
    })
}

/// Parse a `{sha, branch}` git ref, if present.
fn parse_git(value: Option<&Value>) -> Option<GitRef> {
    let obj = value?.as_object()?;
    let sha = str_field(obj, "sha")?.to_string();
    Some(GitRef {
        sha,
        branch: str_field(obj, "branch").map(str::to_string),
    })
}

// ---- small, total field accessors (no panics, no indexing) ----

fn str_field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

fn bool_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    obj.get(key).and_then(Value::as_bool)
}

fn u64_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<u64> {
    obj.get(key).and_then(Value::as_u64)
}

fn u32_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<u32> {
    obj.get(key)
        .and_then(Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn parent_field(obj: &serde_json::Map<String, Value>) -> Option<String> {
    str_field(obj, "parentId")
        .or_else(|| str_field(obj, "parent_id"))
        .map(str::to_string)
}

/// The event id for a record: native `id`, else a stable content hash.
fn event_id_for(obj: &serde_json::Map<String, Value>, raw: &RawRecord) -> String {
    str_field(obj, "id")
        .map(str::to_string)
        .unwrap_or_else(|| content_id(&raw.bytes))
}

/// Timestamp from any of the common keys, falling back to the epoch.
fn ts_for(obj: &serde_json::Map<String, Value>) -> memscribe_core::Timestamp {
    util::ts_from(
        &Value::Object(obj.clone()),
        &["ts", "timestamp", "time", "created_at"],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(s: &str, line: u64) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("cursor.jsonl", 0, line))
    }

    /// Parse a whole JSONL string through one shared context (file order),
    /// returning the flattened event-kind tags.
    fn parse_all(jsonl: &str) -> (Vec<CaptureEvent>, ParseCtx) {
        let adapter = CursorAdapter;
        let mut ctx = ParseCtx::new();
        let mut events = Vec::new();
        for (i, line) in jsonl.lines().enumerate() {
            let r = raw(line, i as u64 + 1);
            let evs = adapter.parse(&r, &mut ctx).expect("parse never errors");
            events.extend(evs);
        }
        (events, ctx)
    }

    fn tags(events: &[CaptureEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.kind.tag()).collect()
    }

    #[test]
    fn session_start_then_decision_then_edit() {
        let jsonl = r#"{"kind":"session_start","sessionId":"s1","cwd":"/work/app","git":{"sha":"abc123","branch":"main"},"toolVersion":"0.42.0"}
{"id":"m1","role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"s1","text":"Let's use Postgres instead of MySQL"}
{"id":"m2","parentId":"m1","role":"assistant","ts":"2026-06-22T10:00:05Z","sessionId":"s1","text":"Switching to Postgres.","model":"cursor-fast","usage":{"input":12,"output":7},"edits":[{"path":"db/config.toml","oldText":"engine=mysql","newText":"engine=postgres","diff":"@@\n-engine=mysql\n+engine=postgres","added":1,"removed":1}]}"#;
        let (events, ctx) = parse_all(jsonl);
        assert_eq!(
            tags(&events),
            vec!["session_start", "user_turn", "assistant_turn", "file_edit"]
        );
        // Session + project were learned from the header.
        assert_eq!(ctx.session_id.as_deref(), Some("s1"));
        assert_eq!(events[1].session_id, "s1");
        assert_eq!(events[1].project.cwd, PathBuf::from("/work/app"));
        // seq is monotonic from file order.
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        // The decision turn is a UserTurn carrying the text verbatim.
        match &events[1].kind {
            EventKind::UserTurn { text, .. } => {
                assert_eq!(text, "Let's use Postgres instead of MySQL");
            }
            other => panic!("expected user_turn, got {other:?}"),
        }
        // The FileEdit carries old/new/unified and line counts.
        match &events[3].kind {
            EventKind::FileEdit { diff, .. } => {
                assert_eq!(diff.path, PathBuf::from("db/config.toml"));
                assert_eq!(diff.old.as_deref(), Some("engine=mysql"));
                assert_eq!(diff.new.as_deref(), Some("engine=postgres"));
                assert_eq!(diff.added_lines, 1);
                assert_eq!(diff.removed_lines, 1);
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }

    #[test]
    fn assistant_usage_and_model_captured() {
        let jsonl = r#"{"id":"a1","role":"assistant","sessionId":"s","text":"hi","model":"cursor-pro","usage":{"input":5,"output":9}}"#;
        let (events, _) = parse_all(jsonl);
        match &events[0].kind {
            EventKind::AssistantTurn {
                model, usage, text, ..
            } => {
                assert_eq!(text, "hi");
                assert_eq!(model.as_deref(), Some("cursor-pro"));
                let u = usage.as_ref().expect("usage");
                assert_eq!(u.input_tokens, Some(5));
                assert_eq!(u.output_tokens, Some(9));
            }
            other => panic!("expected assistant_turn, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_then_result_pairing() {
        let jsonl = r#"{"id":"t1","role":"assistant","sessionId":"s","text":"running","toolCalls":[{"id":"c1","name":"shell","args":{"cmd":"ls"}}],"toolResults":[{"id":"c1","ok":true,"output":"a\nb"}]}"#;
        let (events, ctx) = parse_all(jsonl);
        assert_eq!(
            tags(&events),
            vec!["assistant_turn", "tool_call", "tool_result"]
        );
        assert_eq!(ctx.call_names.get("c1").map(String::as_str), Some("shell"));
        assert_eq!(ctx.call_ok.get("c1").copied(), Some(true));
        match &events[2].kind {
            EventKind::ToolResult { call_id, ok, .. } => {
                assert_eq!(call_id, "c1");
                assert!(*ok);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn failed_tool_result_is_marked_not_ok() {
        // Mirrors the tool_failure fixture: edit's result failed.
        let jsonl = r#"{"id":"f1","role":"assistant","sessionId":"s","text":"trying","toolCalls":[{"id":"e1","name":"edit","args":{"path":"x.rs"}}],"toolResults":[{"id":"e1","ok":false,"output":"permission denied"}],"edits":[{"path":"x.rs","oldText":"a","newText":"b","added":1,"removed":1}]}"#;
        let (events, ctx) = parse_all(jsonl);
        assert_eq!(
            tags(&events),
            vec!["assistant_turn", "tool_call", "tool_result", "file_edit"]
        );
        // The failed result is observable so downstream can suppress the Episode.
        assert_eq!(ctx.call_ok.get("e1").copied(), Some(false));
        match &events[2].kind {
            EventKind::ToolResult { ok, .. } => assert!(!*ok),
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn dedup_repeated_record_is_idempotent() {
        let line = r#"{"id":"dup","role":"user","sessionId":"s","text":"hello"}"#;
        let jsonl = format!("{line}\n{line}");
        let (events, _) = parse_all(&jsonl);
        // The second identical record yields nothing.
        assert_eq!(tags(&events), vec!["user_turn"]);
    }

    #[test]
    fn unrecognized_record_routes_to_unknown() {
        let jsonl = r#"{"kind":"telemetry_ping","payload":{"x":1}}
{"id":"w1","role":"wizard","sessionId":"s","text":"???"}"#;
        let (events, _) = parse_all(jsonl);
        // A record with neither a known kind nor a role → Unknown; a record with
        // an unknown role also degrades to Unknown rather than panicking.
        assert_eq!(tags(&events), vec!["unknown", "unknown"]);
    }

    #[test]
    fn garbage_never_panics() {
        let adapter = CursorAdapter;
        let mut ctx = ParseCtx::new();
        for bad in [
            "",
            "   ",
            "not json at all",
            "{",
            "[1,2,3]",
            "42",
            "true",
            "null",
            r#"{"role":42}"#,
            r#"{"kind":"session_start","git":"oops","cwd":12}"#,
            r#"{"id":"x","role":"user","toolCalls":"not-an-array","edits":{"nope":1}}"#,
        ] {
            let r = raw(bad, 1);
            let evs = adapter.parse(&r, &mut ctx).expect("never errors");
            // Blank lines produce nothing; everything else is lossless (>=1).
            if bad.trim().is_empty() {
                assert!(evs.is_empty());
            } else {
                assert!(!evs.is_empty(), "lossless for {bad:?}");
            }
        }
    }

    #[test]
    fn invalid_json_is_preserved_as_unknown() {
        let (events, _) = parse_all("this is not json");
        assert_eq!(tags(&events), vec!["unknown"]);
        match &events[0].kind {
            EventKind::Unknown { raw, .. } => {
                assert_eq!(raw, &Value::String("this is not json".to_string()));
            }
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn schema_fingerprint_detects_export() {
        let adapter = CursorAdapter;
        let hdr = raw(r#"{"kind":"session_start","cwd":"/x"}"#, 1);
        let msg = raw(r#"{"id":"m","role":"user","text":"hi"}"#, 2);
        let junk = raw(r#"{"kind":"telemetry"}"#, 3);
        assert_eq!(adapter.schema_fingerprint(&hdr).confidence, 100);
        assert_eq!(adapter.schema_fingerprint(&msg).confidence, 100);
        assert_eq!(adapter.schema_fingerprint(&junk).confidence, 0);
    }

    #[test]
    fn session_id_set_before_header_is_learned_from_record() {
        // Even without a header, the first record carrying sessionId seeds ctx.
        let jsonl = r#"{"id":"m1","role":"user","sessionId":"late","text":"hi"}"#;
        let (events, ctx) = parse_all(jsonl);
        assert_eq!(ctx.session_id.as_deref(), Some("late"));
        assert_eq!(events[0].session_id, "late");
    }

    // ---- on-disk fixture conformance ----
    //
    // The fixtures under `fixtures/cursor/v1/` ARE this tool's real record
    // shape and feed the Phase-2 conformance suite. These tests parse them
    // through the live adapter to guarantee the two never drift apart.

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/cursor/v1")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    #[test]
    fn fixture_happy_path_decision_then_edits() {
        let (events, ctx) = parse_all(&fixture("happy_path_decision_then_edits.jsonl"));
        assert_eq!(ctx.session_id.as_deref(), Some("cur-sess-001"));
        // header, decision (user), assistant+call+result+edit, assistant+call+result+edit, end.
        assert_eq!(
            tags(&events),
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "tool_call",
                "tool_result",
                "file_edit",
                "assistant_turn",
                "tool_call",
                "tool_result",
                "file_edit",
                "session_end",
            ]
        );
        // The decision turn is the user's, and a FileEdit follows.
        match &events[1].kind {
            EventKind::UserTurn { text, .. } => assert!(text.contains("Postgres")),
            other => panic!("expected user_turn, got {other:?}"),
        }
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::FileEdit { diff, .. }
                if diff.path == std::path::Path::new("config/database.toml")
        )));
        // Project binding came from the session_start header.
        assert_eq!(
            events[1].project.cwd,
            PathBuf::from("/Users/dev/projects/orders-api")
        );
        // Every tool result in this fixture succeeded.
        assert!(ctx.call_ok.values().all(|ok| *ok));
    }

    #[test]
    fn fixture_rejected_alternative_parses() {
        let (events, _) = parse_all(&fixture("rejected_alternative.jsonl"));
        // Contains a user decision to reject Redux followed by an edit.
        assert!(events.iter().any(
            |e| matches!(&e.kind, EventKind::UserTurn { text, .. } if text.contains("reject"))
        ));
        assert!(events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::FileEdit { .. })));
    }

    #[test]
    fn fixture_ban_parses_decision_and_edit() {
        let (events, _) = parse_all(&fixture("ban.jsonl"));
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::UserTurn { text, .. } if text.contains("never")
        )));
        assert!(events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::FileEdit { .. })));
    }

    #[test]
    fn fixture_tool_failure_edit_has_failed_result() {
        // The edit's tool result FAILED → downstream must NOT mint an Episode.
        // At the event level: there IS a FileEdit, but it is LINKED by call_id to
        // a ToolResult with ok=false, so the segmenter drops it (no spurious
        // episode, §8.2).
        let (events, ctx) = parse_all(&fixture("tool_failure.jsonl"));
        // The edit event still exists (losslessness), keyed to call-edit-4.
        let edit = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::FileEdit { call_id, diff } => Some((call_id.clone(), diff.clone())),
                _ => None,
            })
            .expect("an edit event");
        assert_eq!(edit.1.path, PathBuf::from("deploy.sh"));
        // The edit is tied to the failing call so the segmenter can drop it.
        assert_eq!(edit.0.as_deref(), Some("call-edit-4"));
        // The failing result is observable by call_id → the gate for "no Episode".
        assert_eq!(ctx.call_ok.get("call-edit-4").copied(), Some(false));
        // And the ToolResult event itself is marked not-ok.
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::ToolResult { call_id, ok: false, .. } if call_id == "call-edit-4"
        )));
    }

    #[test]
    fn fixture_tool_failure_yields_no_episode_via_segmenter() {
        // End-to-end through the segmenter: the failed edit must NOT mint an
        // Episode, and the happy path must still mint two.
        use memscribe_core::gate::CommitmentGate;
        use memscribe_core::segmenter::{DefaultSegmenter, Segmenter};

        let gate = CommitmentGate::default();
        let seg = DefaultSegmenter;

        let (fail_events, _) = parse_all(&fixture("tool_failure.jsonl"));
        let fail_seg = seg.segment(&fail_events, &gate);
        assert_eq!(
            fail_seg.episodes.len(),
            0,
            "a failed edit must produce no episode"
        );

        let (ok_events, _) = parse_all(&fixture("happy_path_decision_then_edits.jsonl"));
        let ok_seg = seg.segment(&ok_events, &gate);
        assert_eq!(
            ok_seg.episodes.len(),
            2,
            "the happy path must still produce two episodes"
        );
    }

    #[test]
    fn all_fixtures_lossless_and_never_error() {
        for name in [
            "happy_path_decision_then_edits.jsonl",
            "rejected_alternative.jsonl",
            "ban.jsonl",
            "tool_failure.jsonl",
        ] {
            let (events, _) = parse_all(&fixture(name));
            // No record silently vanished: a non-empty fixture yields events,
            // and none degraded to Unknown (the shapes are all recognized).
            assert!(!events.is_empty(), "{name} produced no events");
            assert!(
                events.iter().all(|e| e.kind.tag() != "unknown"),
                "{name} produced an Unknown event"
            );
        }
    }
}
