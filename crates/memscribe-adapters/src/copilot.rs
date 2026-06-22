//! GitHub Copilot adapter.
//!
//! Covers the GitHub Copilot CLI / Copilot chat export shape (distinct from the
//! VS Code Copilot Chat `workspaceStorage` handled by the `vscode` adapter).
//! Copilot's live store is an undocumented SQLite/`workspaceStorage` blob, so for
//! the initial deterministic model this adapter parses an **exported chat
//! JSON-lines** shape and routes anything unrecognized to
//! [`memscribe_core::EventKind::Unknown`] (losslessness).
//!
//! ## Exported record shape (one JSON object per line)
//! - A leading control record `{kind:"session_start", cwd, git:{sha,branch},
//!   toolVersion, model?, sessionId, ts}` binds the session/project.
//! - `{kind:"session_end", sessionId, reason?, ts}` closes it.
//! - Message records `{id, parentId, role:"user"|"assistant", ts, sessionId,
//!   text, model?, usage:{input,output}?, toolCalls:[{id,name,args}]?,
//!   toolResults:[{id,ok,output}]?, edits:[{path,oldText,newText,diff,added,
//!   removed}]?}`.
//!
//! ## Mapping
//! - `session_start` → [`EventKind::SessionStart`]; `session_end` →
//!   [`EventKind::SessionEnd`].
//! - `role:"user"` → [`EventKind::UserTurn`]; `role:"assistant"` →
//!   [`EventKind::AssistantTurn`] (`text`, `model`, `usage`, `parts`).
//! - `toolCalls[]` → [`EventKind::ToolCall`]; `toolResults[]` →
//!   [`EventKind::ToolResult`] (`ok`); `edits[]` → [`EventKind::FileEdit`]
//!   (`oldText`→`old`, `newText`→`new`, `diff`→`unified`, `added`/`removed`).
//!
//! `discover()` points at the real product paths (the binary store is not parsed
//! in this model). The parser is deterministic, never panics, and dedups repeated
//! records by their native id via [`ParseCtx::first_seen`].

use crate::util;
use memscribe_core::{
    CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, ProjectRef,
    RawRecord, SchemaVariant, SourceKind, TranscriptAdapter, TranscriptHandle, Usage,
};
use serde_json::Value;
use std::path::PathBuf;

/// Adapter for GitHub Copilot transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct CopilotAdapter;

impl TranscriptAdapter for CopilotAdapter {
    fn source_kind(&self) -> SourceKind {
        SourceKind::Copilot
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        discover_handles(cfg)
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        // Parse the line; blank lines yield nothing, invalid JSON is preserved as
        // an Unknown (lossless) rather than failing the stream.
        let Some(value) = util::parse_json_line(raw) else {
            // Distinguish a blank line (skip) from invalid-but-present JSON.
            if raw.as_str().map(str::trim).unwrap_or("").is_empty() {
                return Ok(Vec::new());
            }
            return util::stub_parse(SourceKind::Copilot, raw, ctx);
        };

        Ok(parse_value(raw, ctx, value))
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        fingerprint(sample)
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// The Copilot product paths we point discovery at. We do not parse the binary
/// store in this model, but we surface its location so the runtime/UX can show
/// where Copilot history lives.
fn discover_handles(cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
    let home = cfg.home_dir();
    let mut handles = Vec::new();

    // GitHub Copilot CLI config directory.
    let cli_dir = home.join(".config").join("github-copilot");
    // VS Code Copilot Chat workspace storage (handled in detail by the vscode
    // adapter, but Copilot history physically lives here too).
    let vscode_dir = home
        .join(".config")
        .join("Code")
        .join("User")
        .join("workspaceStorage");

    for dir in [cli_dir, vscode_dir] {
        let session_hint = dir.file_name().and_then(|s| s.to_str()).map(str::to_string);
        handles.push(TranscriptHandle {
            path: dir,
            source: SourceKind::Copilot,
            session_hint,
            compressed: false,
        });
    }

    handles
}

// ---------------------------------------------------------------------------
// Fingerprinting
// ---------------------------------------------------------------------------

fn fingerprint(sample: &RawRecord) -> SchemaVariant {
    let Some(value) = util::parse_json_line(sample) else {
        return SchemaVariant::unknown(SourceKind::Copilot);
    };

    // A control record names the variant with high confidence.
    if value.get("kind").and_then(Value::as_str) == Some("session_start") {
        return SchemaVariant::certain(SourceKind::Copilot, "copilot/export-v1");
    }

    // A message record (role + id) is a reasonable but not definitive signal.
    let has_role = value.get("role").and_then(Value::as_str).is_some();
    let has_id = value.get("id").and_then(Value::as_str).is_some();
    if has_role && has_id {
        return SchemaVariant {
            source: SourceKind::Copilot,
            variant: "copilot/export-v1".to_string(),
            confidence: 80,
        };
    }

    SchemaVariant::unknown(SourceKind::Copilot)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse one already-decoded JSON record into zero or more events.
fn parse_value(raw: &RawRecord, ctx: &mut ParseCtx, value: Value) -> Vec<CaptureEvent> {
    match value.get("kind").and_then(Value::as_str) {
        Some("session_start") => parse_session_start(raw, ctx, &value),
        Some("session_end") => parse_session_end(raw, ctx, &value),
        // A control record we do not recognize → Unknown (lossless).
        Some(_) => vec![util::unknown_event(SourceKind::Copilot, ctx, raw, value)],
        None => match value.get("role").and_then(Value::as_str) {
            Some("user") | Some("assistant") => parse_message(raw, ctx, value),
            // Not a control record and not a known message → Unknown.
            _ => vec![util::unknown_event(SourceKind::Copilot, ctx, raw, value)],
        },
    }
}

fn parse_session_start(raw: &RawRecord, ctx: &mut ParseCtx, value: &Value) -> Vec<CaptureEvent> {
    // Bind the session id as soon as we learn it.
    if ctx.session_id.is_none() {
        if let Some(sid) = value.get("sessionId").and_then(Value::as_str) {
            ctx.session_id = Some(sid.to_string());
        }
    }

    let cwd = value
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let git = parse_git(value.get("git"));
    let model = str_field(value, "model");
    let tool_version = str_field(value, "toolVersion");

    // Populate the project binding from the session-start record.
    if ctx.project.is_none() {
        ctx.project = Some(ProjectRef {
            cwd: cwd.clone(),
            repo_root: None,
            git: git.clone(),
        });
    }

    let event_id = event_id_for(value, &raw.bytes);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = ts_for(value);

    vec![util::mk_event(
        SourceKind::Copilot,
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

fn parse_session_end(raw: &RawRecord, ctx: &mut ParseCtx, value: &Value) -> Vec<CaptureEvent> {
    if ctx.session_id.is_none() {
        if let Some(sid) = value.get("sessionId").and_then(Value::as_str) {
            ctx.session_id = Some(sid.to_string());
        }
    }
    let event_id = event_id_for(value, &raw.bytes);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = ts_for(value);
    let reason = str_field(value, "reason");

    vec![util::mk_event(
        SourceKind::Copilot,
        ctx,
        raw,
        event_id,
        None,
        ts,
        EventKind::SessionEnd { reason },
    )]
}

/// Parse a message record into a turn event, plus any embedded tool calls, tool
/// results, and file edits (each a distinct event with a derived id).
fn parse_message(raw: &RawRecord, ctx: &mut ParseCtx, value: Value) -> Vec<CaptureEvent> {
    if ctx.session_id.is_none() {
        if let Some(sid) = value.get("sessionId").and_then(Value::as_str) {
            ctx.session_id = Some(sid.to_string());
        }
    }

    let msg_id = event_id_for(&value, &raw.bytes);
    // Idempotency: a repeated message record (same id) is dropped wholesale,
    // including all of its derived sub-events.
    if !ctx.first_seen(&msg_id) {
        return Vec::new();
    }

    let parent_id = str_field(&value, "parentId");
    let ts = ts_for(&value);
    let text = str_field(&value, "text").unwrap_or_default();
    let role = value.get("role").and_then(Value::as_str).unwrap_or("");

    let mut events = Vec::new();

    // 1. The turn itself.
    let kind = if role == "assistant" {
        let model = str_field(&value, "model");
        let usage = parse_usage(value.get("usage"));
        EventKind::AssistantTurn {
            text,
            thinking: str_field(&value, "thinking"),
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
    events.push(util::mk_event(
        SourceKind::Copilot,
        ctx,
        raw,
        msg_id.clone(),
        parent_id.clone(),
        ts,
        kind,
    ));

    // 2. Tool calls embedded in the turn.
    if let Some(calls) = value.get("toolCalls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            if let Some(ev) = tool_call_event(raw, ctx, &msg_id, ts, call, i) {
                events.push(ev);
            }
        }
    }

    // 3. Tool results embedded in the turn. We record the success flag in the
    //    context so that any sibling FileEdit can be paired with its outcome.
    if let Some(results) = value.get("toolResults").and_then(Value::as_array) {
        for (i, result) in results.iter().enumerate() {
            if let Some(ev) = tool_result_event(raw, ctx, &msg_id, ts, result, i) {
                events.push(ev);
            }
        }
    }

    // 4. File edits embedded in the turn.
    if let Some(edits) = value.get("edits").and_then(Value::as_array) {
        for (i, edit) in edits.iter().enumerate() {
            if let Some(ev) = file_edit_event(raw, ctx, &msg_id, ts, edit, i) {
                events.push(ev);
            }
        }
    }

    events
}

fn tool_call_event(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    msg_id: &str,
    ts: memscribe_core::Timestamp,
    call: &Value,
    idx: usize,
) -> Option<CaptureEvent> {
    let call_id = str_field(call, "id").unwrap_or_else(|| format!("{msg_id}:call:{idx}"));
    let name = str_field(call, "name").unwrap_or_default();
    let args = call.get("args").cloned().unwrap_or(Value::Null);

    // Remember the tool name for this call id (call/result pairing).
    ctx.call_names.insert(call_id.clone(), name.clone());

    let event_id = format!("{msg_id}:call:{call_id}");
    if !ctx.first_seen(&event_id) {
        return None;
    }
    Some(util::mk_event(
        SourceKind::Copilot,
        ctx,
        raw,
        event_id,
        Some(msg_id.to_string()),
        ts,
        EventKind::ToolCall {
            call_id,
            name,
            args,
        },
    ))
}

fn tool_result_event(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    msg_id: &str,
    ts: memscribe_core::Timestamp,
    result: &Value,
    idx: usize,
) -> Option<CaptureEvent> {
    let call_id = str_field(result, "id").unwrap_or_else(|| format!("{msg_id}:result:{idx}"));
    // `ok` defaults to true when absent (a present result with no flag is success).
    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(true);
    let output = result.get("output").cloned().unwrap_or(Value::Null);

    // Record the outcome so a sibling FileEdit can be paired with it downstream.
    ctx.call_ok.insert(call_id.clone(), ok);

    let event_id = format!("{msg_id}:result:{call_id}");
    if !ctx.first_seen(&event_id) {
        return None;
    }
    Some(util::mk_event(
        SourceKind::Copilot,
        ctx,
        raw,
        event_id,
        Some(msg_id.to_string()),
        ts,
        EventKind::ToolResult {
            call_id,
            ok,
            output,
        },
    ))
}

fn file_edit_event(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    msg_id: &str,
    ts: memscribe_core::Timestamp,
    edit: &Value,
    idx: usize,
) -> Option<CaptureEvent> {
    let path = str_field(edit, "path")?;
    // An edit may name the originating tool call id so downstream can join its
    // ToolResult.ok (a failed edit must not become an Episode).
    let call_id = str_field(edit, "callId").or_else(|| str_field(edit, "call_id"));

    let diff = Diff {
        path: PathBuf::from(&path),
        old: str_field(edit, "oldText"),
        new: str_field(edit, "newText"),
        unified: str_field(edit, "diff"),
        added_lines: u32_field(edit, "added"),
        removed_lines: u32_field(edit, "removed"),
    };

    let event_id = format!("{msg_id}:edit:{idx}:{path}");
    if !ctx.first_seen(&event_id) {
        return None;
    }
    Some(util::mk_event(
        SourceKind::Copilot,
        ctx,
        raw,
        event_id,
        Some(msg_id.to_string()),
        ts,
        EventKind::FileEdit { call_id, diff },
    ))
}

// ---------------------------------------------------------------------------
// Field helpers (all total — never panic on missing/odd input)
// ---------------------------------------------------------------------------

/// The native event id, else a stable content hash of the raw bytes.
fn event_id_for(value: &Value, bytes: &[u8]) -> String {
    str_field(value, "id").unwrap_or_else(|| memscribe_core::content_id(bytes))
}

/// Pull a string field, treating empty/non-string as absent.
fn str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Pull a non-negative count as `u32`, clamping out-of-range/odd values to 0.
fn u32_field(value: &Value, key: &str) -> u32 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// The record timestamp via the shared helper, tolerant of RFC3339 and epoch.
fn ts_for(value: &Value) -> memscribe_core::Timestamp {
    util::ts_from(value, &["ts", "timestamp", "time", "created_at"])
}

fn parse_git(value: Option<&Value>) -> Option<GitRef> {
    let v = value?;
    let sha = str_field(v, "sha")?;
    Some(GitRef {
        sha,
        branch: str_field(v, "branch"),
    })
}

fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let v = value?;
    let input_tokens = v.get("input").and_then(Value::as_u64);
    let output_tokens = v.get("output").and_then(Value::as_u64);
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens: v.get("cacheRead").and_then(Value::as_u64),
        cache_creation_tokens: v.get("cacheCreation").and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(s: &str) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("copilot.jsonl", 0, 1))
    }

    fn parse_all(lines: &[&str]) -> Vec<CaptureEvent> {
        let adapter = CopilotAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for line in lines {
            out.extend(adapter.parse(&raw(line), &mut ctx).expect("never errs"));
        }
        out
    }

    const SESSION_START: &str = r#"{"kind":"session_start","cwd":"/Users/dev/projects/orbit","git":{"sha":"abc123","branch":"main"},"toolVersion":"copilot-cli 1.4.0","model":"gpt-4o","sessionId":"copilot-001","ts":"2026-06-22T10:00:00Z"}"#;

    #[test]
    fn session_start_binds_session_and_project() {
        let adapter = CopilotAdapter;
        let mut ctx = ParseCtx::new();
        let evs = adapter.parse(&raw(SESSION_START), &mut ctx).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind.tag(), "session_start");
        assert_eq!(evs[0].session_id, "copilot-001");
        assert_eq!(ctx.session_id.as_deref(), Some("copilot-001"));
        let proj = ctx.project.as_ref().expect("project bound");
        assert_eq!(proj.cwd, PathBuf::from("/Users/dev/projects/orbit"));
        assert_eq!(proj.git.as_ref().map(|g| g.sha.as_str()), Some("abc123"));
        match &evs[0].kind {
            EventKind::SessionStart {
                model,
                tool_version,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("gpt-4o"));
                assert_eq!(tool_version.as_deref(), Some("copilot-cli 1.4.0"));
            }
            other => panic!("expected session_start, got {other:?}"),
        }
    }

    #[test]
    fn normalized_event_sequence_for_decision_then_edit() {
        let lines = [
            SESSION_START,
            r#"{"id":"m1","parentId":null,"role":"user","sessionId":"copilot-001","ts":"2026-06-22T10:00:05Z","text":"Let's use Postgres instead of MySQL."}"#,
            r#"{"id":"m2","parentId":"m1","role":"assistant","sessionId":"copilot-001","ts":"2026-06-22T10:00:09Z","text":"Switching to Postgres.","model":"gpt-4o","usage":{"input":1000,"output":200},"edits":[{"path":"src/db.rs","oldText":"mysql","newText":"postgres","diff":"@@ -1 +1 @@\n-mysql\n+postgres","added":1,"removed":1}]}"#,
            r#"{"kind":"session_end","sessionId":"copilot-001","reason":"done","ts":"2026-06-22T10:01:00Z"}"#,
        ];
        let evs = parse_all(&lines);
        let tags: Vec<&str> = evs.iter().map(|e| e.kind.tag()).collect();
        assert_eq!(
            tags,
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "file_edit",
                "session_end"
            ]
        );
    }

    #[test]
    fn decision_turn_then_file_edit() {
        let lines = [
            SESSION_START,
            r#"{"id":"m1","role":"user","sessionId":"copilot-001","ts":"2026-06-22T10:00:05Z","text":"Use Redis for the cache."}"#,
            r#"{"id":"m2","parentId":"m1","role":"assistant","sessionId":"copilot-001","ts":"2026-06-22T10:00:09Z","text":"Done.","edits":[{"path":"src/cache.rs","oldText":"a","newText":"b","diff":"d","added":3,"removed":2}]}"#,
        ];
        let evs = parse_all(&lines);
        // user_turn precedes file_edit.
        let user_idx = evs
            .iter()
            .position(|e| e.kind.tag() == "user_turn")
            .unwrap();
        let edit_idx = evs
            .iter()
            .position(|e| e.kind.tag() == "file_edit")
            .unwrap();
        assert!(user_idx < edit_idx);
        match &evs[edit_idx].kind {
            EventKind::FileEdit { diff, .. } => {
                assert_eq!(diff.path, PathBuf::from("src/cache.rs"));
                assert_eq!(diff.old.as_deref(), Some("a"));
                assert_eq!(diff.new.as_deref(), Some("b"));
                assert_eq!(diff.unified.as_deref(), Some("d"));
                assert_eq!(diff.added_lines, 3);
                assert_eq!(diff.removed_lines, 2);
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_and_result_with_ok_flag() {
        let lines = [
            SESSION_START,
            r#"{"id":"m2","role":"assistant","sessionId":"copilot-001","ts":"2026-06-22T10:00:09Z","text":"Running.","toolCalls":[{"id":"c1","name":"apply_patch","args":{"path":"x.rs"}}],"toolResults":[{"id":"c1","ok":false,"output":"patch rejected"}]}"#,
        ];
        let evs = parse_all(&lines);
        let tags: Vec<&str> = evs.iter().map(|e| e.kind.tag()).collect();
        assert_eq!(
            tags,
            vec![
                "session_start",
                "assistant_turn",
                "tool_call",
                "tool_result"
            ]
        );
        match &evs[3].kind {
            EventKind::ToolResult { call_id, ok, .. } => {
                assert_eq!(call_id, "c1");
                assert!(!ok, "failed tool result must carry ok=false");
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn assistant_usage_and_model_captured() {
        let lines = [
            SESSION_START,
            r#"{"id":"m2","role":"assistant","sessionId":"copilot-001","ts":"2026-06-22T10:00:09Z","text":"Hi.","model":"gpt-4o-mini","usage":{"input":42,"output":7}}"#,
        ];
        let evs = parse_all(&lines);
        match &evs[1].kind {
            EventKind::AssistantTurn { model, usage, .. } => {
                assert_eq!(model.as_deref(), Some("gpt-4o-mini"));
                let u = usage.as_ref().expect("usage present");
                assert_eq!(u.input_tokens, Some(42));
                assert_eq!(u.output_tokens, Some(7));
            }
            other => panic!("expected assistant_turn, got {other:?}"),
        }
    }

    #[test]
    fn no_panic_on_garbage_input() {
        let adapter = CopilotAdapter;
        let mut ctx = ParseCtx::new();
        for junk in [
            "not json at all",
            "{",
            "[]",
            "12345",
            "null",
            "true",
            r#"{"role":"user"}"#,           // missing id/text
            r#"{"kind":"weird_control"}"#,  // unknown control
            r#"{"id":"e","role":"alien"}"#, // unknown role
            r#"{"id":"x","role":"assistant","text":null,"edits":"not-an-array"}"#,
            r#"{"kind":"session_start"}"#, // missing all fields
        ] {
            let res = adapter.parse(&raw(junk), &mut ctx);
            assert!(res.is_ok(), "parse must never error on: {junk}");
        }
    }

    #[test]
    fn unrecognized_records_route_to_unknown() {
        let evs = parse_all(&[
            r#"{"kind":"telemetry_ping","seq":3}"#,
            r#"{"id":"z","role":"system","text":"boot"}"#,
        ]);
        assert_eq!(evs.len(), 2);
        assert!(evs.iter().all(|e| e.kind.tag() == "unknown"));
    }

    #[test]
    fn blank_lines_skipped() {
        let evs = parse_all(&["", "   ", "\t"]);
        assert!(evs.is_empty());
    }

    #[test]
    fn dedup_repeated_record_is_idempotent() {
        let user = r#"{"id":"m1","role":"user","sessionId":"copilot-001","ts":"2026-06-22T10:00:05Z","text":"hello"}"#;
        let adapter = CopilotAdapter;
        let mut ctx = ParseCtx::new();
        let first = adapter.parse(&raw(user), &mut ctx).unwrap();
        assert_eq!(first.len(), 1);
        // Re-ingesting the same record (same native id) yields nothing.
        let second = adapter.parse(&raw(user), &mut ctx).unwrap();
        assert!(second.is_empty(), "repeated record must dedup to empty");
    }

    #[test]
    fn dedup_drops_derived_subevents_too() {
        let msg = r#"{"id":"m2","role":"assistant","sessionId":"copilot-001","text":"x","edits":[{"path":"a.rs","oldText":"o","newText":"n","added":1,"removed":0}]}"#;
        let adapter = CopilotAdapter;
        let mut ctx = ParseCtx::new();
        let first = adapter.parse(&raw(msg), &mut ctx).unwrap();
        assert_eq!(first.len(), 2); // assistant_turn + file_edit
        let second = adapter.parse(&raw(msg), &mut ctx).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn seq_is_monotonic_across_subevents() {
        let evs = parse_all(&[
            SESSION_START,
            r#"{"id":"m2","role":"assistant","sessionId":"copilot-001","text":"x","toolCalls":[{"id":"c1","name":"edit","args":{}}],"edits":[{"path":"a.rs","oldText":"o","newText":"n","added":1,"removed":0}]}"#,
        ]);
        let seqs: Vec<u64> = evs.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3]);
    }

    #[test]
    fn ban_turn_is_a_user_turn_with_verbatim_text() {
        let evs = parse_all(&[
            SESSION_START,
            r#"{"id":"m1","role":"user","sessionId":"copilot-001","ts":"2026-06-22T10:00:05Z","text":"We will never add a dependency on left-pad."}"#,
        ]);
        match &evs[1].kind {
            EventKind::UserTurn { text, .. } => {
                assert_eq!(text, "We will never add a dependency on left-pad.");
            }
            other => panic!("expected user_turn, got {other:?}"),
        }
    }

    #[test]
    fn discover_points_at_product_paths() {
        let cfg = DiscoverCfg {
            home: Some(PathBuf::from("/home/dev")),
            ..Default::default()
        };
        let handles = CopilotAdapter.discover(&cfg);
        assert!(!handles.is_empty());
        assert!(handles.iter().all(|h| h.source == SourceKind::Copilot));
        assert!(handles.iter().any(|h| h.path.ends_with("github-copilot")));
    }

    #[test]
    fn fingerprint_recognizes_session_start_and_messages() {
        let fp = CopilotAdapter.schema_fingerprint(&raw(SESSION_START));
        assert_eq!(fp.source, SourceKind::Copilot);
        assert_eq!(fp.confidence, 100);
        assert_eq!(fp.variant, "copilot/export-v1");

        let msg = raw(r#"{"id":"m1","role":"user","text":"hi"}"#);
        let fp2 = CopilotAdapter.schema_fingerprint(&msg);
        assert_eq!(fp2.confidence, 80);

        let junk = raw("not json");
        let fp3 = CopilotAdapter.schema_fingerprint(&junk);
        assert_eq!(fp3.confidence, 0);
    }

    #[test]
    fn invariants_hold_on_happy_path() {
        let evs = parse_all(&[
            SESSION_START,
            r#"{"id":"m1","role":"user","sessionId":"copilot-001","ts":"2026-06-22T10:00:05Z","text":"Use Postgres."}"#,
            r#"{"id":"m2","parentId":"m1","role":"assistant","sessionId":"copilot-001","ts":"2026-06-22T10:00:09Z","text":"ok","edits":[{"path":"a.rs","oldText":"o","newText":"n","added":1,"removed":1}]}"#,
        ]);
        // Monotonic seq within the session.
        let mut last = None;
        for e in &evs {
            if let Some(p) = last {
                assert!(e.seq > p);
            }
            last = Some(e.seq);
        }
        // Unique event ids.
        let mut seen = std::collections::HashSet::new();
        for e in &evs {
            assert!(seen.insert(e.event_id.clone()), "dup id {}", e.event_id);
        }
    }

    // --- Fixture-parity guards (mirror fixtures/copilot/v1/*.jsonl verbatim) ---

    #[test]
    fn fixture_tool_failure_edit_has_failed_result() {
        // The assistant record from fixtures/copilot/v1/tool_failure.jsonl: the
        // edit's tool result failed, so the FileEdit must coexist with a
        // ToolResult{ok:false} — that is the signal downstream uses to suppress
        // a spurious Episode.
        let line = r#"{"id":"msg-2","parentId":"msg-1","role":"assistant","ts":"2026-06-22T13:00:13Z","sessionId":"copilot-thread-004","text":"I'll apply the migration patch.","model":"gpt-4o","usage":{"input":720,"output":90},"toolCalls":[{"id":"call-z9","name":"apply_patch","args":{"path":"migrations/0007_email_not_null.sql"}}],"toolResults":[{"id":"call-z9","ok":false,"output":"error: patch did not apply cleanly: hunk #1 FAILED at line 3"}],"edits":[{"path":"migrations/0007_email_not_null.sql","callId":"call-z9","oldText":"email TEXT","newText":"email TEXT NOT NULL","diff":"@@ -3 +3 @@\n-email TEXT\n+email TEXT NOT NULL","added":1,"removed":1}]}"#;
        let evs = parse_all(&[line]);
        let tags: Vec<&str> = evs.iter().map(|e| e.kind.tag()).collect();
        assert_eq!(
            tags,
            vec!["assistant_turn", "tool_call", "tool_result", "file_edit"]
        );
        // The failed result carries ok=false.
        let failed = evs
            .iter()
            .find(|e| e.kind.tag() == "tool_result")
            .expect("tool_result present");
        match &failed.kind {
            EventKind::ToolResult { ok, call_id, .. } => {
                assert!(!ok, "tool result must be ok=false");
                assert_eq!(call_id, "call-z9");
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
        // The FileEdit links back to the failing call id for downstream pairing.
        let edit = evs
            .iter()
            .find(|e| e.kind.tag() == "file_edit")
            .expect("file_edit present");
        match &edit.kind {
            EventKind::FileEdit { call_id, .. } => {
                assert_eq!(call_id.as_deref(), Some("call-z9"));
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }

    #[test]
    fn fixture_rejected_alternative_edit_succeeds() {
        // The assistant record from fixtures/copilot/v1/rejected_alternative.jsonl:
        // the edit's tool result succeeded (ok=true).
        let line = r#"{"id":"msg-2","parentId":"msg-1","role":"assistant","ts":"2026-06-22T11:00:14Z","sessionId":"copilot-thread-002","text":"Understood. I'll wire up the Stripe SDK and drop the PayPal client.","model":"gpt-4o","usage":{"input":980,"output":210},"toolCalls":[{"id":"call-a1","name":"apply_patch","args":{"path":"src/payments/provider.rs"}}],"toolResults":[{"id":"call-a1","ok":true,"output":"patch applied (1 file changed)"}],"edits":[{"path":"src/payments/provider.rs","callId":"call-a1","oldText":"use paypal_sdk::Client;","newText":"use stripe::Client;","diff":"@@ -1 +1 @@\n-use paypal_sdk::Client;\n+use stripe::Client;","added":1,"removed":1}]}"#;
        let evs = parse_all(&[line]);
        let ok = evs.iter().any(|e| {
            matches!(&e.kind, EventKind::ToolResult { ok: true, call_id, .. } if call_id == "call-a1")
        });
        assert!(ok, "rejected_alternative edit result should be ok=true");
    }
}
