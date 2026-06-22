//! Windsurf (Codeium) adapter.
//!
//! Windsurf is a VS Code-based editor whose Cascade agent stores chat in an
//! undocumented binary/SQLite store under `~/.codeium/windsurf/` and
//! `~/Library/Application Support/Windsurf/User/`. We do not parse that binary
//! store in this model. Instead this adapter targets a deterministic **exported
//! Cascade chat JSON-Lines** shape (one JSON object per line) and routes any
//! unrecognized-but-valid record to [`memscribe_core::EventKind::Unknown`] so the
//! stream stays lossless.
//!
//! Record shape (see `fixtures/windsurf/v1/`):
//! - a leading session header: `{"kind":"session_start","cwd":..,"git":{"sha","branch"},"toolVersion":..,"sessionId":..,"model":..}`
//! - message records: `{"id","parentId","role":"user"|"assistant","ts","sessionId","text","model","usage":{"input","output"},"toolCalls":[{"id","name","args"}],"toolResults":[{"id","ok","output"}],"edits":[{"path","oldText","newText","diff","added","removed"}]}`
//!
//! Mapping: `session_start` → `SessionStart`; `role:user` → `UserTurn`;
//! `role:assistant` → `AssistantTurn`; each `toolCalls[]` → `ToolCall`; each
//! `toolResults[]` → `ToolResult{ok}`; each `edits[]` → `FileEdit`.
//!
//! Hard rules honored: never panics (no unwrap/expect/indexing on parsed input);
//! deterministic (no clock/random/global state); `ctx.session_id` is set from the
//! first record carrying it; `ctx.project` is populated from the session-start
//! record; repeated records dedup via `ctx.first_seen(event_id)`.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, TranscriptHandle, Usage,
};
use std::path::PathBuf;

/// Adapter for Windsurf transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindsurfAdapter;

const SOURCE: SourceKind = SourceKind::Windsurf;

impl memscribe_core::TranscriptAdapter for WindsurfAdapter {
    fn source_kind(&self) -> SourceKind {
        SOURCE
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        // The real product stores chat in a binary/SQLite store; we do not parse
        // it here, but discovery still points at the on-disk locations so a
        // future exporter / probe has the canonical paths. Order is stable.
        let home = cfg.home_dir();
        let mut out = Vec::new();
        let candidates = [
            home.join(".codeium").join("windsurf"),
            home.join("Library")
                .join("Application Support")
                .join("Windsurf")
                .join("User"),
        ];
        for path in candidates {
            out.push(TranscriptHandle {
                path,
                source: SOURCE,
                session_hint: None,
                compressed: false,
            });
        }
        out
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        // Blank lines / invalid JSON: skip (blank) or fall through to a string
        // Unknown so nothing is lost.
        let Some(value) = util::parse_json_line(raw) else {
            let s = raw.as_str().map(str::trim).unwrap_or("");
            if s.is_empty() {
                return Ok(Vec::new());
            }
            let v = serde_json::Value::String(s.to_string());
            return Ok(vec![util::unknown_event(SOURCE, ctx, raw, v)]);
        };

        let kind = str_field(&value, "kind");
        let role = str_field(&value, "role");

        if kind.as_deref() == Some("session_start") {
            return Ok(parse_session_start(raw, ctx, value));
        }
        match role.as_deref() {
            Some("user") => Ok(parse_message(raw, ctx, value, false)),
            Some("assistant") => Ok(parse_message(raw, ctx, value, true)),
            // A valid JSON record we don't recognize: lossless Unknown.
            _ => Ok(vec![util::unknown_event(SOURCE, ctx, raw, value)]),
        }
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        match util::parse_json_line(sample) {
            Some(v)
                if str_field(&v, "kind").as_deref() == Some("session_start")
                    || str_field(&v, "role").is_some() =>
            {
                SchemaVariant::certain(SOURCE, "windsurf/cascade-export-v1")
            }
            _ => SchemaVariant::unknown(SOURCE),
        }
    }
}

/// Parse the session-start header: stamp `ctx.session_id` and `ctx.project`,
/// then emit a `SessionStart` event (deduped).
fn parse_session_start(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: serde_json::Value,
) -> Vec<CaptureEvent> {
    if let Some(sid) = str_field(&value, "sessionId") {
        if ctx.session_id.is_none() {
            ctx.session_id = Some(sid);
        }
    }

    let cwd = str_field(&value, "cwd")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let git = parse_git(value.get("git"));
    let model = str_field(&value, "model");
    let tool_version = str_field(&value, "toolVersion");

    // Bind the project for every event in this session.
    ctx.project = Some(ProjectRef {
        cwd: cwd.clone(),
        repo_root: None,
        git: git.clone(),
    });

    let event_id = event_id_for(&value, raw);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = ts_of(&value);
    vec![util::mk_event(
        SOURCE,
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

/// Parse a user/assistant message record into its turn event plus any nested
/// tool calls, tool results, and file edits (in a deterministic order).
fn parse_message(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: serde_json::Value,
    assistant: bool,
) -> Vec<CaptureEvent> {
    // Pick up a session id if the header was missing.
    if ctx.session_id.is_none() {
        if let Some(sid) = str_field(&value, "sessionId") {
            ctx.session_id = Some(sid);
        }
    }

    let turn_id = event_id_for(&value, raw);
    // Idempotency: a repeated record (same id) emits nothing.
    if !ctx.first_seen(&turn_id) {
        return Vec::new();
    }

    let parent_id = str_field(&value, "parentId");
    let ts = ts_of(&value);
    let text = str_field(&value, "text").unwrap_or_default();

    let mut out = Vec::new();

    let turn_kind = if assistant {
        let model = str_field(&value, "model");
        let usage = parse_usage(value.get("usage"));
        EventKind::AssistantTurn {
            text,
            thinking: None,
            model,
            usage,
            parts: Vec::new(),
        }
    } else {
        EventKind::UserTurn {
            text,
            parts: Vec::new(),
        }
    };
    out.push(util::mk_event(
        SOURCE,
        ctx,
        raw,
        turn_id.clone(),
        parent_id,
        ts,
        turn_kind,
    ));

    // Tool calls — record call name so a later result/edit can pair by call_id.
    if let Some(calls) = value.get("toolCalls").and_then(|v| v.as_array()) {
        for (i, call) in calls.iter().enumerate() {
            let call_id = str_field(call, "id").unwrap_or_else(|| format!("{turn_id}/call/{i}"));
            let name = str_field(call, "name").unwrap_or_default();
            let args = call.get("args").cloned().unwrap_or(serde_json::Value::Null);
            ctx.call_names.insert(call_id.clone(), name.clone());
            out.push(util::mk_event(
                SOURCE,
                ctx,
                raw,
                format!("{turn_id}#toolcall:{call_id}"),
                Some(turn_id.clone()),
                ts,
                EventKind::ToolCall {
                    call_id,
                    name,
                    args,
                },
            ));
        }
    }

    // Tool results — `ok` flag is recorded so edits can detect tool failures.
    if let Some(results) = value.get("toolResults").and_then(|v| v.as_array()) {
        for (i, res) in results.iter().enumerate() {
            let call_id = str_field(res, "id").unwrap_or_else(|| format!("{turn_id}/result/{i}"));
            let ok = res
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let output = res
                .get("output")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            ctx.call_ok.insert(call_id.clone(), ok);
            out.push(util::mk_event(
                SOURCE,
                ctx,
                raw,
                format!("{turn_id}#toolresult:{call_id}"),
                Some(turn_id.clone()),
                ts,
                EventKind::ToolResult {
                    call_id,
                    ok,
                    output,
                },
            ));
        }
    }

    // File edits — normalized to FileEdit{diff}.
    if let Some(edits) = value.get("edits").and_then(|v| v.as_array()) {
        for (i, edit) in edits.iter().enumerate() {
            let path = str_field(edit, "path").unwrap_or_default();
            let diff = Diff {
                path: PathBuf::from(path),
                old: str_field(edit, "oldText"),
                new: str_field(edit, "newText"),
                unified: str_field(edit, "diff"),
                added_lines: u32_field(edit, "added"),
                removed_lines: u32_field(edit, "removed"),
            };
            let call_id = str_field(edit, "id");
            out.push(util::mk_event(
                SOURCE,
                ctx,
                raw,
                format!("{turn_id}#edit:{i}"),
                Some(turn_id.clone()),
                ts,
                EventKind::FileEdit { call_id, diff },
            ));
        }
    }

    out
}

/// The event id: tool-native `id` when present, else a content hash of the bytes.
fn event_id_for(value: &serde_json::Value, raw: &RawRecord) -> String {
    str_field(value, "id").unwrap_or_else(|| content_id(&raw.bytes))
}

/// The record timestamp, via the shared `parse_ts` over the common keys.
fn ts_of(value: &serde_json::Value) -> memscribe_core::Timestamp {
    util::ts_from(value, &["ts", "timestamp", "time", "created_at"])
}

/// Read a string field, returning `None` when absent or not a string.
fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Read a non-negative integer field as `u32`, clamped, defaulting to 0.
fn u32_field(value: &serde_json::Value, key: &str) -> u32 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

/// Parse the optional `git` object into a `GitRef`.
fn parse_git(value: Option<&serde_json::Value>) -> Option<GitRef> {
    let g = value?;
    let sha = str_field(g, "sha")?;
    let branch = str_field(g, "branch");
    Some(GitRef { sha, branch })
}

/// Parse the optional `usage` object into a `Usage`.
fn parse_usage(value: Option<&serde_json::Value>) -> Option<Usage> {
    let u = value?;
    let input_tokens = u.get("input").and_then(serde_json::Value::as_u64);
    let output_tokens = u.get("output").and_then(serde_json::Value::as_u64);
    Some(Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens: None,
        cache_creation_tokens: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::{SourceLocation, TranscriptAdapter};
    use std::path::Path;

    fn raw(s: &str) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("cascade.jsonl", 0, 1))
    }

    fn parse_all(lines: &[&str]) -> Vec<CaptureEvent> {
        let a = WindsurfAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for l in lines {
            out.extend(a.parse(&raw(l), &mut ctx).expect("parse never errors"));
        }
        out
    }

    const SESSION_START: &str = r#"{"kind":"session_start","sessionId":"ws-1","cwd":"/home/dev/proj","git":{"sha":"abc123","branch":"main"},"toolVersion":"1.2.3","model":"cascade-base"}"#;

    #[test]
    fn session_start_sets_session_and_project() {
        let evs = parse_all(&[SESSION_START]);
        assert_eq!(evs.len(), 1);
        let e = &evs[0];
        assert_eq!(e.kind.tag(), "session_start");
        assert_eq!(e.session_id, "ws-1");
        assert_eq!(e.project.cwd, PathBuf::from("/home/dev/proj"));
        let git = e.project.git.as_ref().expect("git bound");
        assert_eq!(git.sha, "abc123");
        assert_eq!(git.branch.as_deref(), Some("main"));
        match &e.kind {
            EventKind::SessionStart {
                model,
                tool_version,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("cascade-base"));
                assert_eq!(tool_version.as_deref(), Some("1.2.3"));
            }
            other => panic!("expected session_start, got {other:?}"),
        }
    }

    #[test]
    fn normalized_sequence_user_then_assistant_with_tools_and_edit() {
        let user = r#"{"id":"u1","role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"ws-1","text":"Let's use Postgres instead of MySQL"}"#;
        let asst = r#"{"id":"a1","parentId":"u1","role":"assistant","ts":"2026-06-22T10:00:05Z","sessionId":"ws-1","text":"On it.","model":"cascade-base","usage":{"input":10,"output":4},"toolCalls":[{"id":"c1","name":"edit_file","args":{"path":"db.rs"}}],"toolResults":[{"id":"c1","ok":true,"output":"done"}],"edits":[{"id":"c1","path":"db.rs","oldText":"mysql","newText":"postgres","diff":"@@ -1 +1 @@","added":1,"removed":1}]}"#;
        let tags: Vec<&str> = parse_all(&[SESSION_START, user, asst])
            .iter()
            .map(|e| e.kind.tag())
            .collect();
        assert_eq!(
            tags,
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "tool_call",
                "tool_result",
                "file_edit",
            ]
        );
    }

    #[test]
    fn decision_then_edit_produces_user_turn_then_file_edit() {
        let user = r#"{"id":"u1","role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"ws-1","text":"Let's use Postgres instead of MySQL","edits":[{"path":"schema.sql","oldText":"a","newText":"b","added":1,"removed":1}]}"#;
        let evs = parse_all(&[user]);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].kind.tag(), "user_turn");
        assert_eq!(evs[1].kind.tag(), "file_edit");
        match &evs[0].kind {
            EventKind::UserTurn { text, .. } => {
                assert_eq!(text, "Let's use Postgres instead of MySQL");
            }
            other => panic!("expected user_turn, got {other:?}"),
        }
        match &evs[1].kind {
            EventKind::FileEdit { diff, .. } => {
                assert_eq!(diff.path, PathBuf::from("schema.sql"));
                assert_eq!(diff.added_lines, 1);
                assert_eq!(diff.removed_lines, 1);
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }

    #[test]
    fn assistant_usage_and_model_are_copied() {
        let asst = r#"{"id":"a1","role":"assistant","ts":"2026-06-22T10:00:05Z","sessionId":"ws-1","text":"hi","model":"cascade-pro","usage":{"input":100,"output":42}}"#;
        let evs = parse_all(&[asst]);
        match &evs[0].kind {
            EventKind::AssistantTurn { model, usage, .. } => {
                assert_eq!(model.as_deref(), Some("cascade-pro"));
                let u = usage.as_ref().expect("usage present");
                assert_eq!(u.input_tokens, Some(100));
                assert_eq!(u.output_tokens, Some(42));
            }
            other => panic!("expected assistant_turn, got {other:?}"),
        }
    }

    #[test]
    fn tool_failure_result_marks_not_ok() {
        let asst = r#"{"id":"a1","role":"assistant","ts":"2026-06-22T10:00:05Z","sessionId":"ws-1","text":"trying","toolResults":[{"id":"c1","ok":false,"output":"permission denied"}],"edits":[{"id":"c1","path":"locked.rs","oldText":"x","newText":"y","added":1,"removed":1}]}"#;
        let evs = parse_all(&[asst]);
        let tr = evs
            .iter()
            .find(|e| e.kind.tag() == "tool_result")
            .expect("tool_result present");
        match &tr.kind {
            EventKind::ToolResult { ok, .. } => assert!(!ok),
            other => panic!("expected tool_result, got {other:?}"),
        }
        // The edit is still captured (losslessness); episode-building downstream
        // decides not to mint an Episode for a failed edit.
        assert!(evs.iter().any(|e| e.kind.tag() == "file_edit"));
    }

    #[test]
    fn unrecognized_valid_record_becomes_unknown() {
        let weird = r#"{"kind":"telemetry","payload":{"latency_ms":12}}"#;
        let evs = parse_all(&[weird]);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind.tag(), "unknown");
    }

    #[test]
    fn garbage_never_panics_and_is_lossless() {
        // Invalid JSON, a bare scalar, and blank input.
        let a = WindsurfAdapter;
        let mut ctx = ParseCtx::new();
        let garbage = a.parse(&raw("{not json at all"), &mut ctx).unwrap();
        assert_eq!(garbage.len(), 1);
        assert_eq!(garbage[0].kind.tag(), "unknown");
        let scalar = a.parse(&raw("42"), &mut ctx).unwrap();
        assert_eq!(scalar.len(), 1);
        assert_eq!(scalar[0].kind.tag(), "unknown");
        let blank = a.parse(&raw("   "), &mut ctx).unwrap();
        assert!(blank.is_empty());
    }

    #[test]
    fn repeated_record_is_deduped() {
        let user = r#"{"id":"u1","role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"ws-1","text":"hello"}"#;
        let evs = parse_all(&[user, user]);
        assert_eq!(evs.len(), 1, "second identical record dedups to empty");
        assert_eq!(evs[0].kind.tag(), "user_turn");
    }

    #[test]
    fn seq_is_monotonic_and_deterministic() {
        let user = r#"{"id":"u1","role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"ws-1","text":"a"}"#;
        let asst = r#"{"id":"a1","role":"assistant","ts":"2026-06-22T10:00:01Z","sessionId":"ws-1","text":"b"}"#;
        let evs = parse_all(&[SESSION_START, user, asst]);
        let seqs: Vec<u64> = evs.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[test]
    fn no_id_falls_back_to_content_hash() {
        let rec =
            r#"{"role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"ws-1","text":"no id here"}"#;
        let evs = parse_all(&[rec]);
        assert_eq!(evs.len(), 1);
        assert!(!evs[0].event_id.is_empty());
        // Deterministic: same bytes → same id.
        let again = parse_all(&[rec]);
        assert_eq!(evs[0].event_id, again[0].event_id);
    }

    #[test]
    fn discover_points_at_real_product_paths() {
        let cfg = DiscoverCfg {
            home: Some(PathBuf::from("/home/dev")),
            ..DiscoverCfg::default()
        };
        let handles = WindsurfAdapter.discover(&cfg);
        assert_eq!(handles.len(), 2);
        assert!(handles.iter().all(|h| h.source == SourceKind::Windsurf));
        assert!(handles
            .iter()
            .any(|h| h.path == Path::new("/home/dev/.codeium/windsurf")));
    }

    #[test]
    fn schema_fingerprint_recognizes_cascade_export() {
        let fp = WindsurfAdapter.schema_fingerprint(&raw(SESSION_START));
        assert_eq!(fp.variant, "windsurf/cascade-export-v1");
        assert_eq!(fp.confidence, 100);
        let unknown = WindsurfAdapter.schema_fingerprint(&raw("{not json"));
        assert_eq!(unknown.confidence, 0);
    }
}
