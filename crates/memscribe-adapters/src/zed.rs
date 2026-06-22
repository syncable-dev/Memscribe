//! Zed adapter.
//!
//! Zed's assistant/agent stores threads under its application support directory
//! (`~/Library/Application Support/Zed/threads/` on macOS, `~/.local/share/zed/`
//! on Linux) in an undocumented SQLite/JSON store. We do not parse that binary
//! store in this model; instead this adapter targets an **exported JSON-lines**
//! thread shape and routes anything unrecognized to [`EventKind::Unknown`] so the
//! stream stays lossless across Zed's frequent format churn.
//!
//! ## Exported record shape (one JSON object per line)
//!
//! A leading session header:
//! ```json
//! {"kind":"session_start","cwd":"…","git":{"sha":"…","branch":"…"},
//!  "toolVersion":"zed 0.182.0","sessionId":"…","ts":"2026-06-22T10:00:00Z"}
//! ```
//! followed by message records:
//! ```json
//! {"id":"…","parentId":"…","role":"user|assistant","ts":"…","sessionId":"…",
//!  "text":"…","model":"…","usage":{"input":N,"output":N},
//!  "toolCalls":[{"id":"…","name":"…","args":{…}}],
//!  "toolResults":[{"id":"…","ok":true,"output":…}],
//!  "edits":[{"path":"…","oldText":"…","newText":"…","diff":"…",
//!            "added":N,"removed":N}]}
//! ```
//! and an optional `{"kind":"session_end","reason":"…"}` trailer.
//!
//! ## Mapping
//! - `kind:session_start` → [`EventKind::SessionStart`] (also binds
//!   `ctx.session_id` and `ctx.project`).
//! - `role:user` → [`EventKind::UserTurn`].
//! - `role:assistant` → [`EventKind::AssistantTurn`] (`text`, `model`, `usage`).
//! - `toolCalls[]` → [`EventKind::ToolCall`].
//! - `toolResults[]` → [`EventKind::ToolResult`] (`ok`), and the `ok` flag is
//!   remembered so downstream can suppress episodes for failed edits.
//! - `edits[]` → [`EventKind::FileEdit`] with a normalized [`Diff`].
//!
//! ## Invariants
//! Never panics (no `unwrap`/`expect`/indexing on parsed input); fully
//! deterministic (no clock/random/global state); deduplicates by record id via
//! [`ParseCtx::first_seen`]; any valid-but-unrecognized record becomes
//! [`EventKind::Unknown`].

use crate::util;
use memscribe_core::{
    CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, ProjectRef,
    RawRecord, SchemaVariant, SourceKind, TranscriptAdapter, TranscriptHandle, Usage,
};
use std::path::PathBuf;

/// Adapter for Zed transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct ZedAdapter;

impl TranscriptAdapter for ZedAdapter {
    fn source_kind(&self) -> SourceKind {
        SourceKind::Zed
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        let home = cfg.home_dir();
        // Zed's real on-disk thread stores. We point at them so the runtime can
        // surface where Zed history lives even though this model parses exported
        // JSONL rather than the binary store.
        let roots = [
            home.join("Library/Application Support/Zed/threads"),
            home.join(".local/share/zed/threads"),
            home.join(".local/share/zed"),
        ];
        let mut handles = Vec::new();
        for root in roots {
            if !root.is_dir() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&root)
                .max_depth(4)
                .into_iter()
                .filter_map(Result::ok)
            {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let ext_ok = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| matches!(e, "jsonl" | "json" | "ndjson"))
                    .unwrap_or(false);
                if !ext_ok {
                    continue;
                }
                let session_hint = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string);
                handles.push(TranscriptHandle {
                    path: path.to_path_buf(),
                    source: SourceKind::Zed,
                    session_hint,
                    compressed: false,
                });
            }
        }
        // Deterministic ordering across platforms / filesystem iteration order.
        handles.sort_by(|a, b| a.path.cmp(&b.path));
        handles
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        let Some(value) = util::parse_json_line(raw) else {
            // Blank line → nothing; non-JSON garbage → lossless Unknown.
            let s = raw.as_str().map(str::trim).unwrap_or("");
            if s.is_empty() {
                return Ok(Vec::new());
            }
            let v = serde_json::Value::String(s.to_string());
            return Ok(vec![util::unknown_event(SourceKind::Zed, ctx, raw, v)]);
        };

        // `kind`-tagged control records (session lifecycle).
        if let Some(kind) = value.get("kind").and_then(|v| v.as_str()) {
            match kind {
                "session_start" => return Ok(parse_session_start(raw, ctx, &value)),
                "session_end" => return Ok(parse_session_end(raw, ctx, &value)),
                _ => return Ok(vec![util::unknown_event(SourceKind::Zed, ctx, raw, value)]),
            }
        }

        // Otherwise it should be a `role`-tagged message record.
        if value.get("role").and_then(|v| v.as_str()).is_some() {
            return Ok(parse_message(raw, ctx, &value));
        }

        // Valid JSON we don't recognize → Unknown (losslessness).
        Ok(vec![util::unknown_event(SourceKind::Zed, ctx, raw, value)])
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        let Some(value) = util::parse_json_line(sample) else {
            return SchemaVariant::unknown(SourceKind::Zed);
        };
        let looks_like_zed = value.get("kind").and_then(|v| v.as_str()) == Some("session_start")
            || (value.get("role").is_some()
                && (value.get("toolCalls").is_some()
                    || value.get("toolResults").is_some()
                    || value.get("edits").is_some()
                    || value.get("sessionId").is_some()));
        if looks_like_zed {
            SchemaVariant::certain(SourceKind::Zed, "zed/export-v1")
        } else {
            SchemaVariant::unknown(SourceKind::Zed)
        }
    }
}

/// Parse a `kind:session_start` header, binding session + project on `ctx`.
fn parse_session_start(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: &serde_json::Value,
) -> Vec<CaptureEvent> {
    if let Some(sid) = value.get("sessionId").and_then(|v| v.as_str()) {
        ctx.session_id = Some(sid.to_string());
    }
    let cwd = value
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let git = parse_git(value.get("git"));
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let tool_version = value
        .get("toolVersion")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Bind the project for every subsequent event in this session.
    ctx.project = Some(ProjectRef {
        cwd: cwd.clone(),
        repo_root: None,
        git: git.clone(),
    });

    let event_id = event_id_for(value, raw);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = util::ts_from(value, &["ts", "timestamp", "time"]);
    vec![util::mk_event(
        SourceKind::Zed,
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

/// Parse a `kind:session_end` trailer.
fn parse_session_end(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: &serde_json::Value,
) -> Vec<CaptureEvent> {
    if let Some(sid) = value.get("sessionId").and_then(|v| v.as_str()) {
        if ctx.session_id.is_none() {
            ctx.session_id = Some(sid.to_string());
        }
    }
    let event_id = event_id_for(value, raw);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = util::ts_from(value, &["ts", "timestamp", "time"]);
    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    vec![util::mk_event(
        SourceKind::Zed,
        ctx,
        raw,
        event_id,
        None,
        ts,
        EventKind::SessionEnd { reason },
    )]
}

/// Parse a `role`-tagged message record into its turn plus any embedded
/// tool calls, tool results, and file edits (one record fans out to many events).
fn parse_message(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: &serde_json::Value,
) -> Vec<CaptureEvent> {
    if let Some(sid) = value.get("sessionId").and_then(|v| v.as_str()) {
        if ctx.session_id.is_none() {
            ctx.session_id = Some(sid.to_string());
        }
    }

    let record_id = event_id_for(value, raw);
    // Idempotency: a repeated record (same id) yields nothing.
    if !ctx.first_seen(&record_id) {
        return Vec::new();
    }

    let ts = util::ts_from(value, &["ts", "timestamp", "time", "created_at"]);
    let parent_id = value
        .get("parentId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let role = value.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let text = value
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut events = Vec::new();

    // 1) The turn itself.
    let turn_kind = match role {
        "user" => EventKind::UserTurn {
            text,
            parts: Vec::new(),
        },
        "assistant" => {
            let model = value
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let thinking = value
                .get("thinking")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let usage = parse_usage(value.get("usage"));
            EventKind::AssistantTurn {
                text,
                thinking,
                model,
                usage,
                parts: Vec::new(),
            }
        }
        _ => {
            // Unknown role → lossless Unknown for the whole record.
            return vec![util::unknown_event(
                SourceKind::Zed,
                ctx,
                raw,
                value.clone(),
            )];
        }
    };
    events.push(util::mk_event(
        SourceKind::Zed,
        ctx,
        raw,
        record_id.clone(),
        parent_id,
        ts,
        turn_kind,
    ));

    // 2) Tool calls embedded in the turn.
    if let Some(calls) = value.get("toolCalls").and_then(|v| v.as_array()) {
        for (i, call) in calls.iter().enumerate() {
            let call_id = call
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{record_id}#call{i}"));
            let name = call
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = call.get("args").cloned().unwrap_or(serde_json::Value::Null);
            // Remember the call name for pairing with results/edits.
            ctx.call_names.insert(call_id.clone(), name.clone());
            let child_id = format!("{record_id}:call:{call_id}");
            if !ctx.first_seen(&child_id) {
                continue;
            }
            events.push(util::mk_event(
                SourceKind::Zed,
                ctx,
                raw,
                child_id,
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

    // 3) Tool results embedded in the turn.
    if let Some(results) = value.get("toolResults").and_then(|v| v.as_array()) {
        for (i, result) in results.iter().enumerate() {
            let call_id = result
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{record_id}#res{i}"));
            // `ok` defaults to true when omitted; an explicit `false` marks failure.
            let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
            let output = result
                .get("output")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            // Remember success/failure so downstream can suppress failed-edit episodes.
            ctx.call_ok.insert(call_id.clone(), ok);
            let child_id = format!("{record_id}:result:{call_id}");
            if !ctx.first_seen(&child_id) {
                continue;
            }
            events.push(util::mk_event(
                SourceKind::Zed,
                ctx,
                raw,
                child_id,
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

    // 4) File edits embedded in the turn.
    if let Some(edits) = value.get("edits").and_then(|v| v.as_array()) {
        for (i, edit) in edits.iter().enumerate() {
            let path = edit
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let old = edit
                .get("oldText")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let new = edit
                .get("newText")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let unified = edit
                .get("diff")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let added_lines = edit.get("added").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let removed_lines = edit.get("removed").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            // Correlate the edit to a tool call in the same record, if exactly one
            // exists (so downstream can join the edit to its result's `ok` flag).
            let call_id = edit
                .get("callId")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| sole_tool_call_id(value));
            let child_id = format!("{record_id}:edit:{i}");
            if !ctx.first_seen(&child_id) {
                continue;
            }
            events.push(util::mk_event(
                SourceKind::Zed,
                ctx,
                raw,
                child_id,
                Some(record_id.clone()),
                ts,
                EventKind::FileEdit {
                    call_id,
                    diff: Diff {
                        path: PathBuf::from(path),
                        old,
                        new,
                        unified,
                        added_lines,
                        removed_lines,
                    },
                },
            ));
        }
    }

    events
}

/// Resolve the `event_id`: tool-native `id` when present, else a content hash.
fn event_id_for(value: &serde_json::Value, raw: &RawRecord) -> String {
    value
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| memscribe_core::content_id(&raw.bytes))
}

/// Parse an optional `git` object into a [`GitRef`]. A missing/blank sha yields
/// `None` rather than an empty ref.
fn parse_git(value: Option<&serde_json::Value>) -> Option<GitRef> {
    let g = value?;
    let sha = g.get("sha").and_then(|v| v.as_str())?;
    if sha.is_empty() {
        return None;
    }
    let branch = g
        .get("branch")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(GitRef {
        sha: sha.to_string(),
        branch,
    })
}

/// Parse an optional `usage` object. Returns `None` when no fields are present.
fn parse_usage(value: Option<&serde_json::Value>) -> Option<Usage> {
    let u = value?;
    let input_tokens = u
        .get("input")
        .or_else(|| u.get("input_tokens"))
        .and_then(|v| v.as_u64());
    let output_tokens = u
        .get("output")
        .or_else(|| u.get("output_tokens"))
        .and_then(|v| v.as_u64());
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

/// If a record carries exactly one tool call, return its id — used to correlate
/// a sibling file edit to that call. Returns `None` for zero or many calls.
fn sole_tool_call_id(value: &serde_json::Value) -> Option<String> {
    let calls = value.get("toolCalls").and_then(|v| v.as_array())?;
    if calls.len() != 1 {
        return None;
    }
    calls
        .first()
        .and_then(|c| c.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(s: &str) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("zed.jsonl", 0, 1))
    }

    /// Parse a whole JSONL string through one shared ctx, mirroring runtime use.
    fn parse_all(lines: &str) -> (Vec<CaptureEvent>, ParseCtx) {
        let adapter = ZedAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for line in lines.lines() {
            let evs = adapter.parse(&raw(line), &mut ctx).expect("never errors");
            out.extend(evs);
        }
        (out, ctx)
    }

    fn tags(evs: &[CaptureEvent]) -> Vec<&'static str> {
        evs.iter().map(|e| e.kind.tag()).collect()
    }

    #[test]
    fn session_start_binds_session_and_project() {
        let line = r#"{"kind":"session_start","cwd":"/w/orbit","git":{"sha":"abc","branch":"main"},"toolVersion":"zed 0.1","sessionId":"s1","ts":"2026-06-22T10:00:00Z"}"#;
        let (evs, ctx) = parse_all(line);
        assert_eq!(tags(&evs), vec!["session_start"]);
        assert_eq!(ctx.session_id.as_deref(), Some("s1"));
        assert_eq!(evs[0].session_id, "s1");
        match &evs[0].kind {
            EventKind::SessionStart {
                cwd,
                git,
                tool_version,
                ..
            } => {
                assert_eq!(cwd.as_path(), std::path::Path::new("/w/orbit"));
                assert_eq!(git.as_ref().map(|g| g.sha.as_str()), Some("abc"));
                assert_eq!(tool_version.as_deref(), Some("zed 0.1"));
            }
            other => panic!("expected SessionStart, got {other:?}"),
        }
        // Project propagated from session start.
        assert_eq!(evs[0].project.cwd, std::path::Path::new("/w/orbit"));
    }

    #[test]
    fn decision_turn_then_edit_sequence() {
        let lines = concat!(
            r#"{"kind":"session_start","cwd":"/w","git":{"sha":"a"},"sessionId":"s","ts":"2026-06-22T10:00:00Z"}"#,
            "\n",
            r#"{"id":"u1","role":"user","ts":"2026-06-22T10:00:05Z","sessionId":"s","text":"Let's use Postgres instead of MySQL"}"#,
            "\n",
            r#"{"id":"a1","parentId":"u1","role":"assistant","ts":"2026-06-22T10:00:09Z","sessionId":"s","text":"ok","model":"m","usage":{"input":10,"output":3},"edits":[{"path":"src/db.rs","oldText":"mysql","newText":"postgres","diff":"d","added":1,"removed":1}]}"#,
        );
        let (evs, _) = parse_all(lines);
        // A decision (UserTurn) followed by a FileEdit must appear in order.
        assert_eq!(
            tags(&evs),
            vec!["session_start", "user_turn", "assistant_turn", "file_edit"]
        );
        // The user decision text is preserved verbatim.
        match &evs[1].kind {
            EventKind::UserTurn { text, .. } => assert!(text.contains("Postgres")),
            other => panic!("expected UserTurn, got {other:?}"),
        }
        // The edit normalizes old/new/unified + line counts.
        match &evs[3].kind {
            EventKind::FileEdit { diff, .. } => {
                assert_eq!(diff.path, PathBuf::from("src/db.rs"));
                assert_eq!(diff.old.as_deref(), Some("mysql"));
                assert_eq!(diff.new.as_deref(), Some("postgres"));
                assert_eq!(diff.unified.as_deref(), Some("d"));
                assert_eq!(diff.added_lines, 1);
                assert_eq!(diff.removed_lines, 1);
            }
            other => panic!("expected FileEdit, got {other:?}"),
        }
        // Seq is monotonic across the fanned-out events.
        let seqs: Vec<u64> = evs.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3]);
    }

    #[test]
    fn tool_call_and_result_ok_recorded() {
        let lines = concat!(
            r#"{"id":"a","role":"assistant","sessionId":"s","text":"calling","toolCalls":[{"id":"c1","name":"read_file","args":{"path":"x"}}]}"#,
            "\n",
            r#"{"id":"b","role":"assistant","sessionId":"s","text":"got it","toolResults":[{"id":"c1","ok":true,"output":"data"}]}"#,
        );
        let (evs, ctx) = parse_all(lines);
        assert_eq!(
            tags(&evs),
            vec![
                "assistant_turn",
                "tool_call",
                "assistant_turn",
                "tool_result"
            ]
        );
        assert_eq!(
            ctx.call_names.get("c1").map(String::as_str),
            Some("read_file")
        );
        assert_eq!(ctx.call_ok.get("c1"), Some(&true));
    }

    #[test]
    fn failed_tool_result_marks_call_not_ok() {
        // An edit whose tool result failed: the edit is captured but the result's
        // ok:false is recorded so downstream can suppress the episode.
        let lines = concat!(
            r#"{"id":"a","role":"assistant","sessionId":"s","text":"editing","toolCalls":[{"id":"c9","name":"edit_file","args":{}}],"edits":[{"path":"src/c.rs","oldText":"x","newText":"y","added":1,"removed":1}]}"#,
            "\n",
            r#"{"id":"b","role":"assistant","sessionId":"s","text":"failed","toolResults":[{"id":"c9","ok":false,"output":"locked"}]}"#,
        );
        let (evs, ctx) = parse_all(lines);
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
        // The edit was correlated to the sole tool call in its record.
        match &evs[2].kind {
            EventKind::FileEdit { call_id, .. } => {
                assert_eq!(call_id.as_deref(), Some("c9"));
            }
            other => panic!("expected FileEdit, got {other:?}"),
        }
        // The failure is recorded against the call id → downstream drops the episode.
        assert_eq!(ctx.call_ok.get("c9"), Some(&false));
        match &evs[4].kind {
            EventKind::ToolResult { ok, .. } => assert!(!ok),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn dedup_repeated_record_is_idempotent() {
        let line = r#"{"id":"u1","role":"user","sessionId":"s","text":"hi"}"#;
        let adapter = ZedAdapter;
        let mut ctx = ParseCtx::new();
        let first = adapter.parse(&raw(line), &mut ctx).unwrap();
        let second = adapter.parse(&raw(line), &mut ctx).unwrap();
        assert_eq!(tags(&first), vec!["user_turn"]);
        assert!(second.is_empty(), "repeated record must yield nothing");
    }

    #[test]
    fn unknown_record_kind_is_lossless() {
        let line = r#"{"kind":"telemetry_ping","payload":42}"#;
        let (evs, _) = parse_all(line);
        assert_eq!(tags(&evs), vec!["unknown"]);
        match &evs[0].kind {
            EventKind::Unknown { raw, .. } => {
                assert_eq!(raw.get("payload").and_then(|v| v.as_i64()), Some(42));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn unknown_role_is_lossless() {
        let line = r#"{"id":"x","role":"system","sessionId":"s","text":"boot"}"#;
        let (evs, _) = parse_all(line);
        assert_eq!(tags(&evs), vec!["unknown"]);
    }

    #[test]
    fn garbage_input_never_panics_and_is_lossless() {
        let adapter = ZedAdapter;
        let mut ctx = ParseCtx::new();
        // Non-JSON line → Unknown, no panic.
        let g = adapter.parse(&raw("}{ not json at all"), &mut ctx).unwrap();
        assert_eq!(tags(&g), vec!["unknown"]);
        // Blank line → nothing.
        let blank = adapter.parse(&raw("   "), &mut ctx).unwrap();
        assert!(blank.is_empty());
        // Truncated / weird JSON shapes must not panic.
        for s in [
            "{",
            "[1,2,3]",
            "null",
            "12345",
            r#"{"role":"assistant"}"#,
            r#"{"role":"assistant","edits":[{}]}"#,
            r#"{"kind":"session_start"}"#,
            r#"{"role":"assistant","toolCalls":"not-an-array"}"#,
            r#"{"role":"assistant","usage":"oops"}"#,
        ] {
            let _ = adapter.parse(&raw(s), &mut ctx).unwrap();
        }
    }

    #[test]
    fn session_end_emits_session_end() {
        let line = r#"{"kind":"session_end","sessionId":"s","reason":"user_closed","ts":"2026-06-22T10:01:30Z"}"#;
        let (evs, _) = parse_all(line);
        assert_eq!(tags(&evs), vec!["session_end"]);
        match &evs[0].kind {
            EventKind::SessionEnd { reason } => assert_eq!(reason.as_deref(), Some("user_closed")),
            other => panic!("expected SessionEnd, got {other:?}"),
        }
    }

    #[test]
    fn no_id_falls_back_to_content_hash() {
        let line = r#"{"role":"user","sessionId":"s","text":"anon"}"#;
        let (evs, _) = parse_all(line);
        assert_eq!(tags(&evs), vec!["user_turn"]);
        // 64-hex blake3 content id (no native id present).
        assert_eq!(evs[0].event_id.len(), 64);
        assert!(evs[0].event_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprint_recognizes_zed_export() {
        let adapter = ZedAdapter;
        let start = raw(r#"{"kind":"session_start","sessionId":"s","cwd":"/w"}"#);
        assert_eq!(adapter.schema_fingerprint(&start).confidence, 100);
        let msg = raw(r#"{"id":"a","role":"assistant","sessionId":"s","edits":[]}"#);
        assert_eq!(adapter.schema_fingerprint(&msg).confidence, 100);
        let foreign = raw(r#"{"type":"summary","text":"x"}"#);
        assert_eq!(adapter.schema_fingerprint(&foreign).confidence, 0);
    }

    #[test]
    fn full_happy_path_fixture_shape_parses() {
        // Mirrors fixtures/zed/v1/happy_path_decision_then_edits.jsonl in shape.
        let lines = concat!(
            r#"{"kind":"session_start","cwd":"/w","git":{"sha":"a","branch":"main"},"toolVersion":"zed 0.1","sessionId":"t1","ts":"2026-06-22T10:00:00Z"}"#,
            "\n",
            r#"{"id":"m1","parentId":null,"role":"user","ts":"2026-06-22T10:00:05Z","sessionId":"t1","text":"Let's use Postgres instead of MySQL."}"#,
            "\n",
            r#"{"id":"m2","parentId":"m1","role":"assistant","ts":"2026-06-22T10:00:09Z","sessionId":"t1","text":"ok","model":"m","usage":{"input":1,"output":1},"edits":[{"path":"a.rs","oldText":"x","newText":"y","diff":"d","added":1,"removed":1},{"path":"b.rs","oldText":"p","newText":"q","diff":"d2","added":2,"removed":1}]}"#,
            "\n",
            r#"{"kind":"session_end","sessionId":"t1","ts":"2026-06-22T10:01:30Z","reason":"user_closed"}"#,
        );
        let (evs, _) = parse_all(lines);
        assert_eq!(
            tags(&evs),
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "file_edit",
                "file_edit",
                "session_end"
            ]
        );
        // Every event carries the bound session + project.
        assert!(evs.iter().all(|e| e.session_id == "t1"));
        assert!(evs
            .iter()
            .all(|e| e.project.cwd == std::path::Path::new("/w")));
    }
}
