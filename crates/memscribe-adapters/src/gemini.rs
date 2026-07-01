//! Gemini CLI adapter.
//!
//! Transcripts: `~/.gemini/tmp/<hash>/chats/session-*.jsonl`, append-only JSONL;
//! also a legacy single-blob `.json` history. Each non-control line is a message
//! record: `{role: user|gemini|model, text|content|parts, timestamp, thoughts,
//! tokens:{input,output}|tokenCount, toolCalls:[{name, args, resultDisplay}]}`.
//! Control records: `{"$set":{...}}` (session/cwd metadata) and
//! `{"$rewindTo": <id|index>}` (logical truncation).
//!
//! Mapping (whitepaper §5 + Appendix A):
//! - `role:user` → [`EventKind::UserTurn`].
//! - `role:gemini|model` → [`EventKind::AssistantTurn`] with `thinking` from
//!   `thoughts`, `usage` from `tokens`, and structured `parts`.
//! - nested `toolCalls[]` → a [`EventKind::ToolCall`], and when `resultDisplay`
//!   is present a [`EventKind::ToolResult`]; a `FileDiff`-shaped `resultDisplay`
//!   additionally yields a [`EventKind::FileEdit`].
//! - `{"$rewindTo"}` → [`EventKind::Rewind`].
//! - `{"$set"}` that carries a cwd/project → [`EventKind::SessionStart`], else
//!   [`EventKind::Unknown`].
//!
//! Quirks: tolerate the legacy single-blob `.json` and the `$set`/`$rewindTo`
//! control records; prefer `chats/*.jsonl` over `logs.json`. The parser never
//! panics, is fully deterministic, and routes anything unrecognized to
//! [`EventKind::Unknown`] so the stream stays lossless.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, Part,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, TranscriptAdapter, TranscriptHandle, Usage,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const SOURCE: SourceKind = SourceKind::Gemini;

/// Adapter for Google Gemini CLI transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiAdapter;

impl TranscriptAdapter for GeminiAdapter {
    fn source_kind(&self) -> SourceKind {
        SOURCE
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        discover_transcripts(cfg)
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        let Some(value) = util::parse_json_line(raw) else {
            // Blank line or invalid JSON: nothing to emit (blank) or an Unknown
            // for non-empty-but-unparseable bytes — keep losslessness.
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
        Ok(parse_value(raw, ctx, value))
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        match util::parse_json_line(sample) {
            Some(v) if v.get("$set").is_some() || v.get("$rewindTo").is_some() => {
                SchemaVariant::certain(SOURCE, "gemini/control")
            }
            Some(v) if v.get("role").is_some() => SchemaVariant::certain(SOURCE, "gemini/chat-v1"),
            Some(_) => SchemaVariant::unknown(SOURCE),
            None => SchemaVariant::unknown(SOURCE),
        }
    }
}

/// Discover Gemini transcripts under `<home>/.gemini/tmp/<hash>/`.
///
/// Prefers `chats/session-*.jsonl` (and any `chats/*.jsonl`) over the legacy
/// `logs.json`; only when a project directory has no JSONL chat does it fall
/// back to a `logs.json` / `*.json` blob. Output is sorted for determinism.
fn discover_transcripts(cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
    let root = cfg.home_dir().join(".gemini").join("tmp");
    if !root.is_dir() {
        return Vec::new();
    }

    let mut jsonl: Vec<PathBuf> = Vec::new();
    let mut blob: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match path.extension().and_then(|e| e.to_str()) {
            Some("jsonl") => jsonl.push(path.to_path_buf()),
            Some("json") => blob.push(path.to_path_buf()),
            _ => {}
        }
    }

    // Project hash = the directory directly under `tmp/`. If any `.jsonl` chat
    // exists for a project, drop that project's `.json` blobs (prefer chats).
    let projects_with_jsonl: std::collections::HashSet<PathBuf> =
        jsonl.iter().filter_map(|p| project_dir(&root, p)).collect();
    blob.retain(|p| match project_dir(&root, p) {
        Some(proj) => !projects_with_jsonl.contains(&proj),
        None => true,
    });

    let mut handles: Vec<TranscriptHandle> = jsonl
        .into_iter()
        .chain(blob)
        .map(|path| TranscriptHandle {
            session_hint: session_hint_of(&path),
            path,
            source: SOURCE,
            compressed: false,
        })
        .collect();
    handles.sort_by(|a, b| a.path.cmp(&b.path));
    handles
}

/// The project-hash directory directly beneath `tmp/` for a transcript path.
fn project_dir(root: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(root).ok()?;
    let first = rel.components().next()?;
    Some(root.join(first.as_os_str()))
}

/// Derive a session hint from a `session-<id>.jsonl` filename.
fn session_hint_of(path: &Path) -> Option<String> {
    let stem = path.file_stem().and_then(|s| s.to_str())?;
    Some(stem.strip_prefix("session-").unwrap_or(stem).to_string())
}

/// Parse one already-decoded JSON record into zero or more events.
fn parse_value(raw: &RawRecord, ctx: &mut ParseCtx, value: Value) -> Vec<CaptureEvent> {
    if value.get("$rewindTo").is_some() {
        return parse_rewind(raw, ctx, value);
    }
    if value.get("$set").is_some() {
        return parse_set(raw, ctx, value);
    }
    if is_dialogue_turn(&value) {
        return parse_message(raw, ctx, value);
    }
    vec![util::unknown_event(SOURCE, ctx, raw, value)]
}

/// True when `value` is a dialogue-turn record worth routing into
/// [`parse_message`]. Current gemini-cli discriminates messages with a `type`
/// field (`user`|`gemini`|`info`|`error`|`warning`, per
/// `chatRecordingTypes.ts`/`chatRecordingService.ts` — verified against
/// gemini-cli's own source, 2026-07) — `role` never appears in the real
/// schema at all. Before this fix every real message record fell through to
/// `Unknown` (matched neither the doc comment's claimed `role` key nor any
/// dispatch branch), so a real install's chat history was never parsed into
/// structured turns. `role` is still accepted for any legacy/exported data
/// that used it, and only `user`/`gemini`(+legacy `model`/`assistant`)
/// dialogue values route here — `info`/`error`/`warning` are session
/// chrome, not turns, and correctly stay `Unknown` (lossless).
fn is_dialogue_turn(value: &Value) -> bool {
    first_str(value, &["type", "role"])
        .as_deref()
        .is_some_and(|t| matches!(t, "user" | "gemini" | "model" | "assistant"))
}

/// `{"$rewindTo": <id|index>}` → [`EventKind::Rewind`].
fn parse_rewind(raw: &RawRecord, ctx: &mut ParseCtx, value: Value) -> Vec<CaptureEvent> {
    let target = value.get("$rewindTo");
    let to_event = match target {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let event_id = record_id(&value, &raw.bytes);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = util::ts_from(&value, TS_KEYS);
    vec![util::mk_event(
        SOURCE,
        ctx,
        raw,
        event_id,
        None,
        ts,
        EventKind::Rewind { to_event },
    )]
}

/// `{"$set": {...}}` → [`EventKind::SessionStart`] when it carries a cwd/project,
/// otherwise [`EventKind::Unknown`] (lossless).
fn parse_set(raw: &RawRecord, ctx: &mut ParseCtx, value: Value) -> Vec<CaptureEvent> {
    let set = value.get("$set");
    // Learn the session id if present anywhere in the $set payload.
    if let Some(sid) = set
        .and_then(|s| first_str(s, &["sessionId", "session_id", "id"]))
        .or_else(|| first_str(&value, &["sessionId", "session_id"]))
    {
        if ctx.session_id.is_none() {
            ctx.session_id = Some(sid);
        }
    }

    let cwd = set.and_then(|s| first_str(s, &["cwd", "projectRoot", "project_root", "workspace"]));
    let event_id = record_id(&value, &raw.bytes);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }
    let ts = util::ts_from(&value, TS_KEYS);

    let Some(cwd) = cwd else {
        // A `$set` with no project binding is metadata we don't model yet.
        return vec![util::unknown_event(SOURCE, ctx, raw, value)];
    };

    let git = git_ref_from(set.unwrap_or(&value));
    let model = set.and_then(|s| first_str(s, &["model", "modelName", "model_name"]));
    let tool_version = set.and_then(|s| first_str(s, &["version", "cliVersion", "cli_version"]));

    // Stamp the project binding so every later event inherits it.
    let repo_root = set
        .and_then(|s| first_str(s, &["repoRoot", "repo_root"]))
        .map(PathBuf::from);
    ctx.project = Some(ProjectRef {
        cwd: PathBuf::from(&cwd),
        repo_root,
        git: git.clone(),
    });

    vec![util::mk_event(
        SOURCE,
        ctx,
        raw,
        event_id,
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

/// A message record (`role: user|gemini|model`) → one turn plus any nested
/// `toolCalls[]` as ToolCall/ToolResult/FileEdit events.
fn parse_message(raw: &RawRecord, ctx: &mut ParseCtx, value: Value) -> Vec<CaptureEvent> {
    // See `is_dialogue_turn`: real records key on `type`, not `role`.
    let role = first_str(&value, &["type", "role"]).unwrap_or_default();
    let role = role.as_str();
    let event_id = record_id(&value, &raw.bytes);
    if !ctx.first_seen(&event_id) {
        // Idempotency: a repeated record produces nothing on re-ingest.
        return Vec::new();
    }
    let ts = util::ts_from(&value, TS_KEYS);
    let text = flatten_text(&value);

    let mut out = Vec::new();
    let kind = match role {
        "user" => EventKind::UserTurn {
            text,
            parts: message_parts(&value),
        },
        "gemini" | "model" | "assistant" => {
            let thinking = first_str(&value, &["thoughts", "thinking", "reasoning"]);
            EventKind::AssistantTurn {
                text,
                thinking,
                model: first_str(&value, &["model", "modelName", "model_name"]),
                usage: usage_from(&value),
                parts: message_parts(&value),
            }
        }
        _ => {
            // An unrecognized role is still a valid record: keep it verbatim.
            return vec![util::unknown_event(SOURCE, ctx, raw, value)];
        }
    };
    out.push(util::mk_event(
        SOURCE,
        ctx,
        raw,
        event_id.clone(),
        None,
        ts,
        kind,
    ));

    // Nested tool calls become their own events, parented to the turn.
    if let Some(calls) = value.get("toolCalls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            out.extend(parse_tool_call(raw, ctx, &event_id, ts, i, call));
        }
    }
    out
}

/// One nested `toolCalls[]` entry → a ToolCall and, when `resultDisplay` is
/// present, a ToolResult (+ a FileEdit for a `FileDiff`-shaped result).
fn parse_tool_call(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    turn_id: &str,
    ts: memscribe_core::Timestamp,
    index: usize,
    call: &Value,
) -> Vec<CaptureEvent> {
    let name = first_str(call, &["name", "tool", "toolName"]).unwrap_or_default();
    let args = call
        .get("args")
        .or_else(|| call.get("arguments"))
        .or_else(|| call.get("input"))
        .cloned()
        .unwrap_or(Value::Null);
    // A deterministic, stable call id: native id if present, else turn+index.
    let call_id = first_str(call, &["callId", "call_id", "id"])
        .unwrap_or_else(|| format!("{turn_id}:tool:{index}"));

    let mut out = Vec::new();
    out.push(util::mk_event(
        SOURCE,
        ctx,
        raw,
        format!("{call_id}:call"),
        Some(turn_id.to_string()),
        ts,
        EventKind::ToolCall {
            call_id: call_id.clone(),
            name,
            args,
        },
    ));

    let Some(result) = call.get("resultDisplay").or_else(|| call.get("result")) else {
        return out;
    };

    let ok = result_ok(call, result);
    ctx.call_ok.insert(call_id.clone(), ok);
    out.push(util::mk_event(
        SOURCE,
        ctx,
        raw,
        format!("{call_id}:result"),
        Some(turn_id.to_string()),
        ts,
        EventKind::ToolResult {
            call_id: call_id.clone(),
            ok,
            output: result.clone(),
        },
    ));

    if let Some(diff) = file_diff_from(result) {
        out.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            format!("{call_id}:edit"),
            Some(turn_id.to_string()),
            ts,
            EventKind::FileEdit {
                call_id: Some(call_id),
                diff,
            },
        ));
    }
    out
}

/// Timestamp keys Gemini may use, in priority order.
const TS_KEYS: &[&str] = &["timestamp", "time", "ts", "createdAt", "created_at"];

/// A record's native id, falling back to a `blake3` content hash.
fn record_id(value: &Value, bytes: &[u8]) -> String {
    first_str(value, &["id", "messageId", "message_id", "uuid"])
        .unwrap_or_else(|| content_id(bytes))
}

/// The first string-valued key from `keys` present on `value` (non-empty).
fn first_str(value: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = value.get(*k).and_then(Value::as_str) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Flatten a message's textual content from `text`, `content`, or `parts[]`.
fn flatten_text(value: &Value) -> String {
    if let Some(s) = value.get("text").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(s) = value.get("content").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(parts) = value
        .get("parts")
        .or_else(|| value.get("content"))
        .and_then(Value::as_array)
    {
        let mut buf = String::new();
        for p in parts {
            if let Some(s) = p.as_str() {
                buf.push_str(s);
            } else if let Some(s) = p.get("text").and_then(Value::as_str) {
                buf.push_str(s);
            }
        }
        return buf;
    }
    String::new()
}

/// Structured parts, preserving anything we don't recognize as [`Part::Other`].
/// Real gemini-cli records carry these under `content` (a `PartListUnion`
/// array, per `chatRecordingTypes.ts`) — `parts` never appears in the real
/// schema. Before this fix, structured parts were silently ALWAYS empty for
/// real data: `flatten_text` happened to still recover the raw text via its
/// own `content` fallback, which is why this gap didn't show up as missing
/// text, only as an always-empty `parts: Vec<Part>` on every real turn.
fn message_parts(value: &Value) -> Vec<Part> {
    let Some(parts) = value
        .get("parts")
        .or_else(|| value.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    parts
        .iter()
        .map(|p| {
            if let Some(s) = p.as_str() {
                Part::Text {
                    text: s.to_string(),
                }
            } else if let Some(s) = p.get("text").and_then(Value::as_str) {
                Part::Text {
                    text: s.to_string(),
                }
            } else if let Some(s) = p.get("thought").and_then(Value::as_str) {
                Part::Thinking {
                    text: s.to_string(),
                }
            } else {
                Part::Other { raw: p.clone() }
            }
        })
        .collect()
}

/// Token usage from `tokens:{input,output,...}` or a flat `tokenCount`.
fn usage_from(value: &Value) -> Option<Usage> {
    if let Some(tokens) = value.get("tokens").filter(|v| v.is_object()) {
        let usage = Usage {
            input_tokens: u64_at(tokens, &["input", "inputTokens", "prompt", "promptTokens"]),
            output_tokens: u64_at(
                tokens,
                &["output", "outputTokens", "completion", "completionTokens"],
            ),
            cache_read_tokens: u64_at(tokens, &["cacheRead", "cached", "cachedContentTokens"]),
            cache_creation_tokens: u64_at(tokens, &["cacheCreation", "cacheWrite"]),
        };
        if usage != Usage::default() {
            return Some(usage);
        }
    }
    if let Some(total) = u64_at(value, &["tokenCount", "totalTokens"]) {
        return Some(Usage {
            output_tokens: Some(total),
            ..Usage::default()
        });
    }
    None
}

/// First unsigned-integer value among `keys` on `value`.
fn u64_at(value: &Value, keys: &[&str]) -> Option<u64> {
    for k in keys {
        if let Some(n) = value.get(*k).and_then(Value::as_u64) {
            return Some(n);
        }
    }
    None
}

/// A git ref from `commit`/`sha` (+ optional `branch`) within a `$set` payload.
fn git_ref_from(value: &Value) -> Option<GitRef> {
    let sha = first_str(value, &["commit", "sha", "head", "gitCommit"])?;
    Some(GitRef {
        sha,
        branch: first_str(value, &["branch", "gitBranch"]),
    })
}

/// Whether a tool result is a success. A `FileDiff`-shaped result is a success
/// by construction; otherwise an explicit `error`/`success`/`ok`/`status` field
/// decides, defaulting to success when none is present.
fn result_ok(call: &Value, result: &Value) -> bool {
    for v in [call, result] {
        if let Some(b) = v.get("success").and_then(Value::as_bool) {
            return b;
        }
        if let Some(b) = v.get("ok").and_then(Value::as_bool) {
            return b;
        }
        if let Some(b) = v.get("error").and_then(Value::as_bool) {
            return !b;
        }
        if let Some(s) = v.get("status").and_then(Value::as_str) {
            let s = s.to_ascii_lowercase();
            if s == "error" || s == "failed" || s == "failure" || s == "rejected" {
                return false;
            }
            if s == "success" || s == "ok" || s == "completed" {
                return true;
            }
        }
        // A non-empty `error` string/object means failure.
        match v.get("error") {
            Some(Value::String(s)) if !s.is_empty() => return false,
            Some(Value::Object(o)) if !o.is_empty() => return false,
            _ => {}
        }
    }
    true
}

/// A normalized [`Diff`] from a `FileDiff`-shaped `resultDisplay`, if it looks
/// like one (`fileName`/`filePath` plus diff content). Returns `None` for
/// non-edit results.
fn file_diff_from(result: &Value) -> Option<Diff> {
    let obj = result.as_object()?;
    let path = first_str(result, &["fileName", "filePath", "file", "path"])?;
    let has_edit_shape = obj.contains_key("originalContent")
        || obj.contains_key("newContent")
        || obj.contains_key("fileDiff")
        || obj.contains_key("diff")
        || obj.contains_key("diffStat");
    if !has_edit_shape {
        return None;
    }

    let old = first_str(result, &["originalContent", "oldContent", "old"]);
    let new = first_str(result, &["newContent", "new"]);
    let unified = first_str(result, &["fileDiff", "diff", "unified"]);

    let (added, removed) = diff_stat(result);
    Some(Diff {
        path: PathBuf::from(path),
        old,
        new,
        unified,
        added_lines: added,
        removed_lines: removed,
    })
}

/// Added/removed line counts from `diffStat:{added,removed}` or the
/// `model_added_lines`/`model_removed_lines` shape.
fn diff_stat(result: &Value) -> (u32, u32) {
    if let Some(stat) = result.get("diffStat").filter(|v| v.is_object()) {
        let added = u64_at(stat, &["added", "additions", "model_added_lines"]).unwrap_or(0);
        let removed = u64_at(stat, &["removed", "deletions", "model_removed_lines"]).unwrap_or(0);
        return (clamp_u32(added), clamp_u32(removed));
    }
    let added = u64_at(result, &["model_added_lines", "added"]).unwrap_or(0);
    let removed = u64_at(result, &["model_removed_lines", "removed"]).unwrap_or(0);
    (clamp_u32(added), clamp_u32(removed))
}

/// Saturate a `u64` line count into the model's `u32` field.
fn clamp_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(line: &str) -> RawRecord {
        RawRecord::from_line(line, SourceLocation::new("session-x.jsonl", 0, 1))
    }

    fn parse_all(lines: &[&str]) -> Vec<CaptureEvent> {
        let adapter = GeminiAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            let r =
                RawRecord::from_line(l, SourceLocation::new("session-x.jsonl", 0, i as u64 + 1));
            out.extend(adapter.parse(&r, &mut ctx).expect("never errors"));
        }
        out
    }

    fn tags(events: &[CaptureEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.kind.tag()).collect()
    }

    #[test]
    fn set_with_cwd_is_session_start_and_binds_project() {
        let line = r#"{"$set":{"sessionId":"sess-1","cwd":"/home/u/app","model":"gemini-2.5-pro","branch":"main","commit":"abc123"}}"#;
        let evs = parse_all(&[line]);
        assert_eq!(tags(&evs), ["session_start"]);
        assert_eq!(evs[0].session_id, "sess-1");
        assert_eq!(evs[0].project.cwd, PathBuf::from("/home/u/app"));
        match &evs[0].kind {
            EventKind::SessionStart {
                cwd, git, model, ..
            } => {
                assert_eq!(cwd, &PathBuf::from("/home/u/app"));
                assert_eq!(model.as_deref(), Some("gemini-2.5-pro"));
                let git = git.as_ref().expect("git ref");
                assert_eq!(git.sha, "abc123");
                assert_eq!(git.branch.as_deref(), Some("main"));
            }
            other => panic!("expected SessionStart, got {other:?}"),
        }
    }

    #[test]
    fn set_without_project_is_unknown_not_session_start() {
        let evs = parse_all(&[r#"{"$set":{"theme":"dark"}}"#]);
        assert_eq!(tags(&evs), ["unknown"]);
    }

    #[test]
    fn user_then_assistant_with_edit_yields_decision_then_file_edit() {
        // A user decision turn, then an assistant turn whose tool call edits a
        // file — the canonical happy path: UserTurn then (eventually) FileEdit.
        let user = r#"{"id":"m1","role":"user","text":"Let's use Postgres instead of MySQL.","timestamp":"2026-06-22T10:00:00Z"}"#;
        let asst = r#"{"id":"m2","role":"model","text":"Switching the driver.","thoughts":"swap the dep","tokens":{"input":12,"output":34},"timestamp":"2026-06-22T10:00:01Z","toolCalls":[{"name":"write_file","args":{"path":"db.rs"},"resultDisplay":{"fileName":"db.rs","originalContent":"mysql","newContent":"postgres","fileDiff":"@@ -1 +1 @@\n-mysql\n+postgres","diffStat":{"added":1,"removed":1}}}]}"#;
        let evs = parse_all(&[user, asst]);
        assert_eq!(
            tags(&evs),
            [
                "user_turn",
                "assistant_turn",
                "tool_call",
                "tool_result",
                "file_edit",
            ]
        );

        // UserTurn first.
        match &evs[0].kind {
            EventKind::UserTurn { text, .. } => {
                assert!(text.contains("instead of MySQL"));
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
        // AssistantTurn carries thinking + usage.
        match &evs[1].kind {
            EventKind::AssistantTurn {
                thinking, usage, ..
            } => {
                assert_eq!(thinking.as_deref(), Some("swap the dep"));
                let u = usage.as_ref().expect("usage");
                assert_eq!(u.input_tokens, Some(12));
                assert_eq!(u.output_tokens, Some(34));
            }
            other => panic!("expected AssistantTurn, got {other:?}"),
        }
        // FileEdit normalized from the FileDiff.
        match &evs[4].kind {
            EventKind::FileEdit { call_id, diff } => {
                assert!(call_id.is_some());
                assert_eq!(diff.path, PathBuf::from("db.rs"));
                assert_eq!(diff.old.as_deref(), Some("mysql"));
                assert_eq!(diff.new.as_deref(), Some("postgres"));
                assert_eq!(diff.added_lines, 1);
                assert_eq!(diff.removed_lines, 1);
                assert!(diff.unified.as_deref().unwrap().contains("+postgres"));
            }
            other => panic!("expected FileEdit, got {other:?}"),
        }
    }

    #[test]
    fn failed_tool_result_reports_ok_false() {
        // An edit whose tool result failed must surface ToolResult.ok=false so
        // the downstream segmenter suppresses the episode. The FileEdit still
        // carries the same call_id the failed result keys on.
        let line = r#"{"id":"m9","role":"model","text":"trying","toolCalls":[{"callId":"c7","name":"write_file","args":{"path":"x.rs"},"resultDisplay":{"fileName":"x.rs","newContent":"...","fileDiff":"@@","error":"permission denied"}}]}"#;
        let evs = parse_all(&[line]);
        assert_eq!(
            tags(&evs),
            ["assistant_turn", "tool_call", "tool_result", "file_edit"]
        );
        let result = evs
            .iter()
            .find(|e| matches!(e.kind, EventKind::ToolResult { .. }))
            .unwrap();
        let edit = evs
            .iter()
            .find(|e| matches!(e.kind, EventKind::FileEdit { .. }))
            .unwrap();
        match (&result.kind, &edit.kind) {
            (
                EventKind::ToolResult {
                    call_id: rid, ok, ..
                },
                EventKind::FileEdit {
                    call_id: Some(eid), ..
                },
            ) => {
                assert!(!ok, "failed result must be ok=false");
                assert_eq!(rid, eid, "edit and failed result must share call_id");
            }
            other => panic!("unexpected kinds: {other:?}"),
        }
    }

    #[test]
    fn rewind_control_record_maps_to_rewind() {
        let evs = parse_all(&[r#"{"$rewindTo":"m1"}"#]);
        assert_eq!(tags(&evs), ["rewind"]);
        match &evs[0].kind {
            EventKind::Rewind { to_event } => assert_eq!(to_event, "m1"),
            other => panic!("expected Rewind, got {other:?}"),
        }
    }

    #[test]
    fn rewind_to_numeric_index_stringifies() {
        let evs = parse_all(&[r#"{"$rewindTo":3}"#]);
        match &evs[0].kind {
            EventKind::Rewind { to_event } => assert_eq!(to_event, "3"),
            other => panic!("expected Rewind, got {other:?}"),
        }
    }

    #[test]
    fn garbage_never_panics_and_is_lossless() {
        // Invalid JSON, an empty object, a number, an unknown role, a blank
        // line: none may panic, none may be silently dropped (except blanks).
        let evs = parse_all(&[
            "not json at all",
            "{}",
            "42",
            r#"{"role":"system","text":"?"}"#,
            "   ",
            r#"{"foo":"bar"}"#,
        ]);
        // The blank line yields nothing; everything else is at least Unknown.
        assert_eq!(evs.len(), 5);
        assert!(evs.iter().all(|e| {
            matches!(
                e.kind.tag(),
                "unknown" | "user_turn" | "assistant_turn" | "session_start"
            )
        }));
        // The unknown-role record is preserved verbatim, not dropped.
        assert!(evs.iter().any(|e| e.kind.tag() == "unknown"));
    }

    #[test]
    fn repeated_record_is_deduped_for_idempotency() {
        let line = r#"{"id":"dup-1","role":"user","text":"hello"}"#;
        let once = parse_all(&[line]);
        assert_eq!(tags(&once), ["user_turn"]);
        // Re-ingesting the SAME record id within the session yields nothing.
        let twice = parse_all(&[line, line]);
        assert_eq!(tags(&twice), ["user_turn"]);
    }

    #[test]
    fn parse_is_deterministic() {
        let lines = [
            r#"{"$set":{"sessionId":"s","cwd":"/w"}}"#,
            r#"{"id":"a","role":"user","text":"go with Stripe instead of PayPal"}"#,
            r#"{"id":"b","role":"model","text":"ok","toolCalls":[{"name":"edit","args":{},"resultDisplay":{"fileName":"a.rs","newContent":"x","fileDiff":"@@","diffStat":{"added":1,"removed":0}}}]}"#,
        ];
        let a = parse_all(&lines);
        let b = parse_all(&lines);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn seq_is_monotonic_across_a_message_with_tool_calls() {
        let lines = [
            r#"{"id":"a","role":"user","text":"hi"}"#,
            r#"{"id":"b","role":"model","text":"editing","toolCalls":[{"name":"e","args":{},"resultDisplay":{"fileName":"f","newContent":"n","diff":"d"}}]}"#,
        ];
        let evs = parse_all(&lines);
        for w in evs.windows(2) {
            assert!(w[1].seq > w[0].seq, "seq must strictly increase");
        }
    }

    #[test]
    fn legacy_text_only_assistant_has_no_usage_when_absent() {
        let evs = parse_all(&[r#"{"id":"z","role":"gemini","content":"plain reply"}"#]);
        match &evs[0].kind {
            EventKind::AssistantTurn { text, usage, .. } => {
                assert_eq!(text, "plain reply");
                assert!(usage.is_none());
            }
            other => panic!("expected AssistantTurn, got {other:?}"),
        }
    }

    #[test]
    fn token_count_flat_field_becomes_output_usage() {
        let evs = parse_all(&[r#"{"id":"t","role":"model","text":"x","tokenCount":99}"#]);
        match &evs[0].kind {
            EventKind::AssistantTurn { usage, .. } => {
                assert_eq!(usage.as_ref().unwrap().output_tokens, Some(99));
            }
            other => panic!("expected AssistantTurn, got {other:?}"),
        }
    }

    #[test]
    fn schema_fingerprint_classifies_records() {
        let a = GeminiAdapter;
        assert_eq!(
            a.schema_fingerprint(&raw(r#"{"$set":{"cwd":"/w"}}"#))
                .variant,
            "gemini/control"
        );
        assert_eq!(
            a.schema_fingerprint(&raw(r#"{"$rewindTo":1}"#)).variant,
            "gemini/control"
        );
        assert_eq!(
            a.schema_fingerprint(&raw(r#"{"role":"user","text":"hi"}"#))
                .variant,
            "gemini/chat-v1"
        );
        assert_eq!(a.schema_fingerprint(&raw("garbage")).confidence, 0);
    }

    #[test]
    fn real_type_keyed_schema_is_not_silently_unknown() {
        // 2026-07 regression test for the confirmed bug: real gemini-cli
        // records (chatRecordingTypes.ts, verified against the tool's own
        // source) discriminate on `type` — `user`|`gemini`|`info`|`error`|
        // `warning` — never `role`. Every `parse_value`/`parse_message` call
        // in this file gated on `value.get("role")`, so 100% of real message
        // records fell through to Unknown, defeating the adapter entirely.
        // Real `content` is an array of Part-like objects, not `parts`.
        let user = r#"{"id":"m1","timestamp":"2026-06-22T10:00:00Z","type":"user","content":[{"text":"Switch the config loader to Postgres."}]}"#;
        let assistant = r#"{"id":"m2","timestamp":"2026-06-22T10:00:05Z","type":"gemini","content":[{"text":"Switching to Postgres."}],"toolCalls":[{"id":"call-1","name":"edit_file","args":{"path":"config.toml"}}]}"#;
        // Session chrome — must stay Unknown, not be coerced into a turn.
        let info = r#"{"id":"m3","timestamp":"2026-06-22T10:00:06Z","type":"info","content":[{"text":"Context compacted"}]}"#;

        let events = parse_all(&[user, assistant, info]);
        assert_eq!(
            tags(&events),
            ["user_turn", "assistant_turn", "tool_call", "unknown"]
        );

        match &events[0].kind {
            EventKind::UserTurn { text, parts } => {
                assert_eq!(text, "Switch the config loader to Postgres.");
                // message_parts must also read `content`, not just `parts`.
                assert_eq!(parts.len(), 1);
                assert!(matches!(&parts[0], Part::Text { text } if text == "Switch the config loader to Postgres."));
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
        match &events[1].kind {
            EventKind::AssistantTurn { text, .. } => {
                assert_eq!(text, "Switching to Postgres.");
            }
            other => panic!("expected AssistantTurn, got {other:?}"),
        }
        assert!(matches!(
            &events[2].kind,
            EventKind::ToolCall { name, .. } if name == "edit_file"
        ));
    }

    #[test]
    fn legacy_role_keyed_records_still_parse() {
        // Backward compatibility: any legacy/exported data still keyed on
        // `role` (the pre-fix assumption) must keep working unchanged.
        let line = r#"{"id":"m1","role":"user","text":"hello"}"#;
        let events = parse_all(&[line]);
        assert_eq!(tags(&events), ["user_turn"]);
    }
}
