//! VS Code adapter (Copilot Chat / chat sessions).
//!
//! VS Code stores chat sessions under
//! `<user>/workspaceStorage/<hash>/chatSessions/*.json` (and
//! `chatEditingSessions` for edits). That on-disk store is an undocumented,
//! version-churning JSON blob, so this adapter parses two shapes:
//!
//! 1. A stable, **exported** chat JSON-lines shape (one record per line) that a
//!    companion exporter writes — a leading `{kind:session_start, cwd, git,
//!    toolVersion}` followed by message records `{id, parentId, role, ts,
//!    sessionId, text, model, usage, toolCalls, toolResults, edits}`.
//! 2. The **native** `chatSessions` JSON shape, where a single object carries
//!    `{version, requesterUsername, responderUsername, requests:[{message,
//!    response}]}`; each request maps to a `UserTurn` and its response to an
//!    `AssistantTurn`.
//!
//! Anything unrecognized-but-valid routes to [`memscribe_core::EventKind::Unknown`]
//! via [`util::unknown_event`], so the stream stays lossless across VS Code
//! version churn. The parser is fully deterministic and never panics.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, TranscriptAdapter, TranscriptHandle, Usage,
};
use serde_json::Value;
use std::path::PathBuf;

const SOURCE: SourceKind = SourceKind::VsCode;

/// Adapter for VS Code chat-session transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct VsCodeAdapter;

impl TranscriptAdapter for VsCodeAdapter {
    fn source_kind(&self) -> SourceKind {
        SOURCE
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        // Point at the real product path; we don't parse the binary store here.
        // `Application Support/Code/User/workspaceStorage/<hash>/chatSessions/*.json`
        let home = cfg.home_dir();
        let base = home
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
            .join("workspaceStorage");
        let mut handles = Vec::new();
        // Walk workspaceStorage/<hash>/chatSessions/*.json deterministically.
        let mut hashes: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    hashes.push(p);
                }
            }
        }
        hashes.sort();
        for ws in hashes {
            let sessions_dir = ws.join("chatSessions");
            let mut files: Vec<PathBuf> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("json") {
                        files.push(p);
                    }
                }
            }
            files.sort();
            for f in files {
                let session_hint = f.file_stem().and_then(|s| s.to_str()).map(str::to_string);
                handles.push(TranscriptHandle {
                    path: f,
                    source: SOURCE,
                    session_hint,
                    compressed: false,
                });
            }
        }
        handles
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        let Some(value) = util::parse_json_line(raw) else {
            // Blank line → nothing; non-JSON but non-blank → lossless Unknown.
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

        // The native chatSessions shape: a single object with a `requests` array.
        if value.get("requests").and_then(Value::as_array).is_some() {
            return Ok(parse_native_session(raw, ctx, &value));
        }

        // Otherwise treat it as one exported JSON-lines record.
        Ok(parse_exported_record(raw, ctx, value))
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        let Some(value) = util::parse_json_line(sample) else {
            return SchemaVariant::unknown(SOURCE);
        };
        if value.get("requests").and_then(Value::as_array).is_some() {
            return SchemaVariant::certain(SOURCE, "vscode/chat-sessions-native");
        }
        if value.get("kind").and_then(Value::as_str) == Some("session_start")
            || value.get("role").and_then(Value::as_str).is_some()
        {
            return SchemaVariant::certain(SOURCE, "vscode/exported-jsonl-v1");
        }
        SchemaVariant::unknown(SOURCE)
    }
}

/// Parse one record of the exported JSON-lines shape into zero or more events.
fn parse_exported_record(raw: &RawRecord, ctx: &mut ParseCtx, value: Value) -> Vec<CaptureEvent> {
    let kind = value.get("kind").and_then(Value::as_str);

    // Record-level dedup / idempotency: a repeated record yields nothing.
    let record_id = record_event_id(raw, &value);
    if !ctx.first_seen(&record_id) {
        return Vec::new();
    }

    match kind {
        Some("session_start") => {
            apply_session_start(ctx, &value);
            let ts = util::ts_from(&value, &["ts", "timestamp", "time"]);
            let git = parse_git(value.get("git"));
            let cwd = string_field(&value, "cwd").unwrap_or_else(|| ".".to_string());
            let model = string_field(&value, "model");
            let tool_version = string_field(&value, "toolVersion");
            vec![util::mk_event(
                SOURCE,
                ctx,
                raw,
                record_id,
                None,
                ts,
                EventKind::SessionStart {
                    cwd: PathBuf::from(cwd),
                    git,
                    model,
                    tool_version,
                },
            )]
        }
        Some("session_end") => {
            adopt_session(ctx, &value);
            let ts = util::ts_from(&value, &["ts", "timestamp", "time"]);
            let reason = string_field(&value, "reason");
            vec![util::mk_event(
                SOURCE,
                ctx,
                raw,
                record_id,
                None,
                ts,
                EventKind::SessionEnd { reason },
            )]
        }
        _ => parse_message_record(raw, ctx, &value, record_id),
    }
}

/// Parse one message record (`role: user|assistant`, with optional toolCalls,
/// toolResults, and edits) into an ordered list of events.
fn parse_message_record(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: &Value,
    record_id: String,
) -> Vec<CaptureEvent> {
    adopt_session(ctx, value);
    let ts = util::ts_from(value, &["ts", "timestamp", "time"]);
    let parent_id = string_field(value, "parentId");
    let role = value.get("role").and_then(Value::as_str);
    let text = string_field(value, "text").unwrap_or_default();

    let mut out = Vec::new();

    match role {
        Some("user") => out.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            record_id.clone(),
            parent_id.clone(),
            ts,
            EventKind::UserTurn {
                text,
                parts: Vec::new(),
            },
        )),
        Some("assistant") => out.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            record_id.clone(),
            parent_id.clone(),
            ts,
            EventKind::AssistantTurn {
                text,
                thinking: None,
                model: string_field(value, "model"),
                usage: parse_usage(value.get("usage")),
                parts: Vec::new(),
            },
        )),
        // A record with edits/tool data but no recognized role is still
        // valuable; if it carries no actionable role and nothing else, fall
        // through to the sub-records below and, if none, emit Unknown.
        _ => {
            let has_children = value
                .get("toolCalls")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
                || value
                    .get("toolResults")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
                || value
                    .get("edits")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty());
            if !has_children {
                return vec![util::unknown_event(SOURCE, ctx, raw, value.clone())];
            }
        }
    }

    // Tool calls.
    if let Some(calls) = value.get("toolCalls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            let call_id =
                string_field(call, "id").unwrap_or_else(|| format!("{record_id}:call:{i}"));
            let name = string_field(call, "name").unwrap_or_default();
            let args = call.get("args").cloned().unwrap_or(Value::Null);
            ctx.call_names.insert(call_id.clone(), name.clone());
            out.push(util::mk_event(
                SOURCE,
                ctx,
                raw,
                derive_id(&record_id, "toolcall", i, &call_id),
                Some(record_id.clone()),
                ts,
                EventKind::ToolCall {
                    call_id,
                    name,
                    args,
                },
            ));
        }
    }

    // Tool results.
    if let Some(results) = value.get("toolResults").and_then(Value::as_array) {
        for (i, res) in results.iter().enumerate() {
            let call_id =
                string_field(res, "id").unwrap_or_else(|| format!("{record_id}:result:{i}"));
            let ok = res.get("ok").and_then(Value::as_bool).unwrap_or(true);
            let output = res.get("output").cloned().unwrap_or(Value::Null);
            ctx.call_ok.insert(call_id.clone(), ok);
            out.push(util::mk_event(
                SOURCE,
                ctx,
                raw,
                derive_id(&record_id, "toolresult", i, &call_id),
                Some(record_id.clone()),
                ts,
                EventKind::ToolResult {
                    call_id,
                    ok,
                    output,
                },
            ));
        }
    }

    // File edits.
    if let Some(edits) = value.get("edits").and_then(Value::as_array) {
        for (i, edit) in edits.iter().enumerate() {
            let diff = parse_edit(edit);
            let edit_id = string_field(edit, "id")
                .or_else(|| diff.path.to_str().map(str::to_string))
                .unwrap_or_default();
            // Link the edit to its originating tool call when the export carries
            // one (`callId`/`call_id`). This is what lets the segmenter drop an
            // edit whose paired ToolResult failed (ok=false) — "a tool failure →
            // no spurious episode" (§8.2). Absent the field, the edit stands on
            // its own (the happy path), matching the prior behavior.
            let call_id = string_field(edit, "callId").or_else(|| string_field(edit, "call_id"));
            out.push(util::mk_event(
                SOURCE,
                ctx,
                raw,
                derive_id(&record_id, "edit", i, &edit_id),
                Some(record_id.clone()),
                ts,
                EventKind::FileEdit { call_id, diff },
            ));
        }
    }

    out
}

/// Parse the native `chatSessions` shape: `requests[].message` → `UserTurn`,
/// `requests[].response[]` → `AssistantTurn`.
fn parse_native_session(raw: &RawRecord, ctx: &mut ParseCtx, value: &Value) -> Vec<CaptureEvent> {
    // Record-level idempotency on the whole session object.
    let record_id = record_event_id(raw, value);
    if !ctx.first_seen(&record_id) {
        return Vec::new();
    }
    adopt_session(ctx, value);
    if ctx.session_id.is_none() {
        // Native files have no `sessionId`; derive a stable one from content.
        ctx.session_id = Some(format!("vscode-{}", &record_id[..record_id.len().min(16)]));
    }

    let ts = util::ts_from(value, &["ts", "timestamp", "time"]);
    let responder = string_field(value, "responderUsername");
    let mut out = Vec::new();

    let Some(requests) = value.get("requests").and_then(Value::as_array) else {
        return vec![util::unknown_event(SOURCE, ctx, raw, value.clone())];
    };

    for (i, req) in requests.iter().enumerate() {
        // User turn from `message`.
        let user_text = req
            .get("message")
            .map(flatten_native_text)
            .unwrap_or_default();
        out.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            format!("{record_id}:req:{i}:user"),
            None,
            ts,
            EventKind::UserTurn {
                text: user_text,
                parts: Vec::new(),
            },
        ));

        // Assistant turn from `response` (an array of parts).
        let resp_text = flatten_native_response(req.get("response"));
        out.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            format!("{record_id}:req:{i}:asst"),
            Some(format!("{record_id}:req:{i}:user")),
            ts,
            EventKind::AssistantTurn {
                text: resp_text,
                thinking: None,
                model: responder.clone(),
                usage: None,
                parts: Vec::new(),
            },
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The deterministic record-level event id: the native `id` field, else a
/// `blake3` of the raw bytes (per the format spec: `event_id = id else content_id`).
fn record_event_id(raw: &RawRecord, value: &Value) -> String {
    string_field(value, "id").unwrap_or_else(|| content_id(&raw.bytes))
}

/// Derive a stable, collision-free child event id under a parent record id.
fn derive_id(record_id: &str, kind: &str, index: usize, native: &str) -> String {
    format!("{record_id}:{kind}:{index}:{native}")
}

/// Read a string field, ignoring empty/non-string values.
fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Set `ctx.session_id` from `sessionId` if not already set.
fn adopt_session(ctx: &mut ParseCtx, value: &Value) {
    if ctx.session_id.is_none() {
        if let Some(sid) = string_field(value, "sessionId") {
            ctx.session_id = Some(sid);
        }
    }
}

/// Apply a session-start record to the context: session id + project binding.
fn apply_session_start(ctx: &mut ParseCtx, value: &Value) {
    adopt_session(ctx, value);
    let cwd = string_field(value, "cwd").unwrap_or_else(|| ".".to_string());
    let git = parse_git(value.get("git"));
    ctx.project = Some(ProjectRef {
        cwd: PathBuf::from(cwd),
        repo_root: None,
        git,
    });
}

/// Parse a `{sha, branch}` git object.
fn parse_git(value: Option<&Value>) -> Option<GitRef> {
    let obj = value?;
    let sha = string_field(obj, "sha")?;
    let branch = string_field(obj, "branch");
    Some(GitRef { sha, branch })
}

/// Parse a `{input, output}` usage object into [`Usage`].
fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let obj = value?;
    let input_tokens = obj.get("input").and_then(Value::as_u64);
    let output_tokens = obj.get("output").and_then(Value::as_u64);
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens: None,
        cache_creation_tokens: None,
    })
}

/// Parse an `{path, oldText, newText, diff, added, removed}` edit into a [`Diff`].
fn parse_edit(edit: &Value) -> Diff {
    let path = string_field(edit, "path").unwrap_or_default();
    Diff {
        path: PathBuf::from(path),
        old: string_field(edit, "oldText"),
        new: string_field(edit, "newText"),
        unified: string_field(edit, "diff"),
        added_lines: edit
            .get("added")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u64::from(u32::MAX)) as u32,
        removed_lines: edit
            .get("removed")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u64::from(u32::MAX)) as u32,
    }
}

/// Flatten a native `message` object (`{text, parts:[{kind:text,text}]}`).
fn flatten_native_text(message: &Value) -> String {
    if let Some(t) = string_field(message, "text") {
        return t;
    }
    flatten_text_parts(message.get("parts"))
}

/// Flatten a native `response` (an array of `{kind:text,text}` parts).
fn flatten_native_response(response: Option<&Value>) -> String {
    match response {
        Some(Value::Array(_)) => flatten_text_parts(response),
        Some(Value::String(s)) => s.clone(),
        Some(obj @ Value::Object(_)) => flatten_native_text(obj),
        _ => String::new(),
    }
}

/// Concatenate the `text` of every `{kind:"text", text}` part in an array.
fn flatten_text_parts(parts: Option<&Value>) -> String {
    let Some(arr) = parts.and_then(Value::as_array) else {
        return String::new();
    };
    let mut chunks: Vec<String> = Vec::new();
    for part in arr {
        if part.get("kind").and_then(Value::as_str) == Some("text") {
            if let Some(t) = string_field(part, "text") {
                chunks.push(t);
            }
        }
    }
    chunks.join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(line: &str) -> RawRecord {
        RawRecord::from_line(line, SourceLocation::new("vscode.jsonl", 0, 1))
    }

    /// Run a slice of JSONL lines through the adapter, threading one context.
    fn run(lines: &[&str]) -> Vec<CaptureEvent> {
        let adapter = VsCodeAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for line in lines {
            let evs = adapter.parse(&raw(line), &mut ctx).expect("never errors");
            out.extend(evs);
        }
        out
    }

    fn tags(events: &[CaptureEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.kind.tag()).collect()
    }

    const SESSION_START: &str = r#"{"kind":"session_start","sessionId":"s1","cwd":"/work","git":{"sha":"abc","branch":"main"},"toolVersion":"1.92.0","model":"gpt-4o"}"#;

    #[test]
    fn session_start_sets_session_and_project() {
        let evs = run(&[SESSION_START]);
        assert_eq!(tags(&evs), vec!["session_start"]);
        assert_eq!(evs[0].session_id, "s1");
        assert_eq!(evs[0].project.cwd, PathBuf::from("/work"));
        let git = evs[0].project.git.as_ref().expect("git set from start");
        assert_eq!(git.sha, "abc");
        assert_eq!(git.branch.as_deref(), Some("main"));
        match &evs[0].kind {
            EventKind::SessionStart {
                model,
                tool_version,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("gpt-4o"));
                assert_eq!(tool_version.as_deref(), Some("1.92.0"));
            }
            other => panic!("expected session_start, got {other:?}"),
        }
    }

    #[test]
    fn normalized_sequence_decision_then_edits() {
        let user = r#"{"id":"m1","role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"s1","text":"Let's use Postgres instead of MySQL."}"#;
        let asst = r#"{"id":"m2","parentId":"m1","role":"assistant","ts":"2026-06-22T10:00:05Z","sessionId":"s1","text":"Switching now.","model":"gpt-4o","usage":{"input":10,"output":3},"edits":[{"path":"src/db.ts","oldText":"mysql","newText":"postgres","added":1,"removed":1}]}"#;
        let evs = run(&[SESSION_START, user, asst]);
        // session_start, user_turn, assistant_turn, file_edit
        assert_eq!(
            tags(&evs),
            vec!["session_start", "user_turn", "assistant_turn", "file_edit"]
        );
        // The decision turn is a UserTurn carrying the decision text.
        match &evs[1].kind {
            EventKind::UserTurn { text, .. } => {
                assert!(text.contains("Postgres"));
            }
            other => panic!("expected user_turn, got {other:?}"),
        }
        // The edit is a FileEdit with the diff fields mapped.
        match &evs[3].kind {
            EventKind::FileEdit { diff, .. } => {
                assert_eq!(diff.path, PathBuf::from("src/db.ts"));
                assert_eq!(diff.old.as_deref(), Some("mysql"));
                assert_eq!(diff.new.as_deref(), Some("postgres"));
                assert_eq!(diff.added_lines, 1);
                assert_eq!(diff.removed_lines, 1);
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
        // The edit's parent links back to the assistant turn record id.
        assert_eq!(evs[3].parent_id.as_deref(), Some("m2"));
    }

    #[test]
    fn tool_call_then_result_failure() {
        let asst = r#"{"id":"t2","role":"assistant","ts":"2026-06-22T13:00:07Z","sessionId":"s4","text":"applying","toolCalls":[{"id":"c1","name":"applyEdit","args":{"path":"x"}}],"edits":[{"path":"x","callId":"c1","oldText":"a","newText":"b","added":1,"removed":1}]}"#;
        let res = r#"{"id":"t3","role":"assistant","ts":"2026-06-22T13:00:09Z","sessionId":"s4","text":"","toolResults":[{"id":"c1","ok":false,"output":"FAILED"}]}"#;
        let evs = run(&[asst, res]);
        // assistant_turn, tool_call, file_edit, assistant_turn, tool_result
        assert_eq!(
            tags(&evs),
            vec![
                "assistant_turn",
                "tool_call",
                "file_edit",
                "assistant_turn",
                "tool_result"
            ]
        );
        // The edit is linked to the failing call by call_id, so the segmenter
        // drops it (no spurious episode for a failed edit).
        match &evs[2].kind {
            EventKind::FileEdit { call_id, .. } => assert_eq!(call_id.as_deref(), Some("c1")),
            other => panic!("expected file_edit, got {other:?}"),
        }
        // The failed result must carry ok=false (so no Episode is produced
        // downstream for the failed edit).
        match &evs[4].kind {
            EventKind::ToolResult { ok, call_id, .. } => {
                assert!(!ok);
                assert_eq!(call_id, "c1");
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    /// On-disk fixture conformance: the `tool_failure` fixture must, end-to-end
    /// through the segmenter, mint NO episode; the happy path must still mint two.
    fn vscode_fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/vscode/v1")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    fn run_file(jsonl: &str) -> Vec<CaptureEvent> {
        let adapter = VsCodeAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for line in jsonl.lines() {
            let evs = adapter.parse(&raw(line), &mut ctx).expect("never errors");
            out.extend(evs);
        }
        out
    }

    #[test]
    fn fixture_tool_failure_yields_no_episode_via_segmenter() {
        use memscribe_core::gate::CommitmentGate;
        use memscribe_core::segmenter::{DefaultSegmenter, Segmenter};

        let gate = CommitmentGate::default();
        let seg = DefaultSegmenter;

        let fail_events = run_file(&vscode_fixture("tool_failure.jsonl"));
        // The failed edit is linked to a failing ToolResult by call_id.
        assert!(fail_events.iter().any(|e| matches!(
            &e.kind,
            EventKind::FileEdit { call_id, .. } if call_id.as_deref() == Some("call-edit-1")
        )));
        let fail_seg = seg.segment(&fail_events, &gate);
        assert_eq!(
            fail_seg.episodes.len(),
            0,
            "a failed edit must produce no episode"
        );

        let ok_events = run_file(&vscode_fixture("happy_path_decision_then_edits.jsonl"));
        let ok_seg = seg.segment(&ok_events, &gate);
        assert_eq!(
            ok_seg.episodes.len(),
            2,
            "the happy path must still produce two episodes"
        );
    }

    #[test]
    fn never_panics_on_garbage() {
        // Invalid JSON, empty, and structurally-weird-but-valid inputs.
        let garbage = run(&[
            "not json at all {{{",
            "",
            "   ",
            "42",
            "true",
            r#"{"role":12345}"#,
            r#"{"kind":"session_start"}"#,
            r#"{"requests":"not-an-array"}"#,
            r#"{"id":"x","edits":[{}]}"#,
        ]);
        // Nothing panicked; every non-blank record produced at least an event.
        // Blank lines produce nothing, so the count is < the input count but > 0.
        assert!(!garbage.is_empty());
        // A non-JSON line is preserved as Unknown (lossless).
        assert!(garbage.iter().any(|e| e.kind.tag() == "unknown"));
    }

    #[test]
    fn unrecognized_valid_record_routes_to_unknown() {
        let evs = run(&[r#"{"id":"weird","kind":"telemetry","payload":{"a":1}}"#]);
        assert_eq!(tags(&evs), vec!["unknown"]);
        match &evs[0].kind {
            EventKind::Unknown { raw_type, raw } => {
                assert_eq!(raw_type, "unknown");
                assert!(raw.get("payload").is_some());
            }
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn dedup_repeated_record_is_idempotent() {
        let user =
            r#"{"id":"m1","role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"s1","text":"hi"}"#;
        // Same record twice → only one event.
        let evs = run(&[SESSION_START, user, user]);
        assert_eq!(tags(&evs), vec!["session_start", "user_turn"]);
        // Sequence numbers are still monotonic and gap-free for what was kept.
        assert_eq!(evs[0].seq, 0);
        assert_eq!(evs[1].seq, 1);
    }

    #[test]
    fn idempotent_record_with_children_dedups_whole_record() {
        let asst = r#"{"id":"m2","role":"assistant","ts":"2026-06-22T10:00:05Z","sessionId":"s1","text":"x","edits":[{"path":"a.ts","oldText":"1","newText":"2","added":1,"removed":0}]}"#;
        let evs = run(&[asst, asst]);
        // First time: assistant_turn + file_edit. Second time: nothing.
        assert_eq!(tags(&evs), vec!["assistant_turn", "file_edit"]);
    }

    #[test]
    fn child_event_ids_do_not_collide_with_turn() {
        let asst = r#"{"id":"m9","role":"assistant","ts":"2026-06-22T10:00:05Z","sessionId":"s1","text":"x","toolCalls":[{"id":"m9","name":"t","args":{}}]}"#;
        // The tool-call's native id collides with the record id; derivation must
        // keep the events distinct so both survive.
        let evs = run(&[asst]);
        assert_eq!(tags(&evs), vec!["assistant_turn", "tool_call"]);
        assert_ne!(evs[0].event_id, evs[1].event_id);
    }

    #[test]
    fn epoch_ms_timestamp_parses() {
        let user = r#"{"id":"m1","role":"user","ts":1750000000000,"sessionId":"s1","text":"hi"}"#;
        let evs = run(&[user]);
        assert_eq!(tags(&evs), vec!["user_turn"]);
        // 1_750_000_000_000 ms = 2025-06-15ish — well after the epoch.
        assert!(evs[0].timestamp.unix_timestamp() > 1_700_000_000);
    }

    #[test]
    fn native_chatsession_shape_maps_requests() {
        let native = r#"{"version":3,"requesterUsername":"dev","responderUsername":"Copilot","requests":[{"message":{"text":"Add a health check","parts":[{"kind":"text","text":"Add a health check"}]},"response":[{"kind":"text","text":"Adding GET /healthz."},{"kind":"text","text":" Done."}]}]}"#;
        let evs = run(&[native]);
        assert_eq!(tags(&evs), vec!["user_turn", "assistant_turn"]);
        match &evs[0].kind {
            EventKind::UserTurn { text, .. } => assert_eq!(text, "Add a health check"),
            other => panic!("expected user_turn, got {other:?}"),
        }
        match &evs[1].kind {
            EventKind::AssistantTurn { text, model, .. } => {
                assert_eq!(text, "Adding GET /healthz. Done.");
                assert_eq!(model.as_deref(), Some("Copilot"));
            }
            other => panic!("expected assistant_turn, got {other:?}"),
        }
    }

    #[test]
    fn schema_fingerprint_distinguishes_shapes() {
        let adapter = VsCodeAdapter;
        let exported = adapter.schema_fingerprint(&raw(SESSION_START));
        assert_eq!(exported.variant, "vscode/exported-jsonl-v1");
        assert_eq!(exported.confidence, 100);

        let native = adapter.schema_fingerprint(&raw(r#"{"requests":[]}"#));
        assert_eq!(native.variant, "vscode/chat-sessions-native");

        let unknown = adapter.schema_fingerprint(&raw("garbage"));
        assert_eq!(unknown.confidence, 0);
    }

    #[test]
    fn session_id_falls_back_to_unknown_without_start() {
        // A bare message with no sessionId and no prior session_start.
        let user = r#"{"id":"m1","role":"user","ts":"2026-06-22T10:00:00Z","text":"hi"}"#;
        let evs = run(&[user]);
        assert_eq!(evs[0].session_id, "unknown");
    }

    #[test]
    fn determinism_same_input_same_output() {
        let lines = [
            SESSION_START,
            r#"{"id":"m1","role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"s1","text":"a"}"#,
            r#"{"id":"m2","parentId":"m1","role":"assistant","ts":"2026-06-22T10:00:05Z","sessionId":"s1","text":"b","edits":[{"path":"p","oldText":"x","newText":"y","added":2,"removed":1}]}"#,
        ];
        let a = run(&lines);
        let b = run(&lines);
        assert_eq!(a, b);
    }
}
