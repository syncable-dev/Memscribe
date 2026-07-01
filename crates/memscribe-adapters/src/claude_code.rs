//! Claude Code adapter.
//!
//! Transcripts: `~/.claude/projects/<slug>/<session>.jsonl`, append-only JSONL,
//! DAG via `parentUuid`. Dialogue: `type:user`/`assistant` records;
//! `message.content[]` text/thinking/tool_use/tool_result; `model`, `usage`.
//! Edits: `toolUseResult.structuredPatch` (Edit/Write/MultiEdit) → old/new/
//! unified; `file-history-snapshot` baseline. Quirks: dedup by `uuid`; a
//! session's summary may live in another file (join by `leafUuid`); subagents
//! are separate `isSidechain` files.
//!
//! One on-disk record can normalize to several events: an assistant record with
//! a `tool_use` block emits an [`EventKind::AssistantTurn`] plus an
//! [`EventKind::ToolCall`]; a user record with a `tool_result` block emits an
//! [`EventKind::UserTurn`] plus an [`EventKind::ToolResult`]; an edit record's
//! top-level `toolUseResult.structuredPatch` emits an [`EventKind::FileEdit`].
//! Every event for one record shares that record's `uuid` lineage; secondary
//! events derive a deterministic id from `uuid` + a stable discriminator so they
//! never collide. The whole record is deduplicated once, on its `uuid`.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, Part,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, TranscriptAdapter, TranscriptHandle, Usage,
};
use std::path::PathBuf;

const SOURCE: SourceKind = SourceKind::ClaudeCode;

/// Adapter for Anthropic Claude Code transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeCodeAdapter;

impl TranscriptAdapter for ClaudeCodeAdapter {
    fn source_kind(&self) -> SourceKind {
        SOURCE
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        discover_transcripts(cfg)
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        // A blank line or non-JSON line carries nothing; stay lossless via the
        // shared stub (which routes non-JSON to Unknown and skips blanks).
        let Some(value) = util::parse_json_line(raw) else {
            return util::stub_parse(SOURCE, raw, ctx);
        };
        Ok(parse_record(raw, ctx, &value))
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        fingerprint(sample)
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Discover `<config>/projects/<slug>/<session>.jsonl` transcripts. The config
/// dir is `CLAUDE_CONFIG_DIR` (memscribe.toml override, then the REAL process
/// env var — Claude Code's own officially documented override, per
/// code.claude.com/docs/en/claude-directory — else `<home>/.claude`).
///
/// Before this fix, a user who legitimately set `CLAUDE_CONFIG_DIR` in their
/// shell (the real, documented way to relocate Claude Code's data) was never
/// honored unless they ALSO duplicated it into a memscribe.toml — silently
/// diverging from where Claude Code itself actually writes transcripts.
fn discover_transcripts(cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
    let base = cfg
        .overrides
        .get("CLAUDE_CONFIG_DIR")
        .cloned()
        .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok().filter(|v| !v.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| cfg.home_dir().join(".claude"));
    let projects = base.join("projects");

    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&projects)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let session_hint = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string);
        out.push(TranscriptHandle {
            path: path.to_path_buf(),
            source: SOURCE,
            session_hint,
            compressed: false,
        });
    }
    // Deterministic order regardless of filesystem walk order.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

// ---------------------------------------------------------------------------
// Fingerprinting
// ---------------------------------------------------------------------------

/// Fingerprint a sample record. Claude Code records are JSON objects carrying a
/// `type` plus `uuid`/`message`/`parentUuid` shape and (modern) a top-level
/// `version`.
fn fingerprint(sample: &RawRecord) -> SchemaVariant {
    let Some(value) = util::parse_json_line(sample) else {
        return SchemaVariant::unknown(SOURCE);
    };
    let has_type = value.get("type").and_then(|v| v.as_str()).is_some();
    let looks_claude = value.get("uuid").is_some()
        || value.get("parentUuid").is_some()
        || value.get("sessionId").is_some()
        || value.get("message").is_some();
    if !has_type || !looks_claude {
        return SchemaVariant::unknown(SOURCE);
    }
    // The `2.x` line stamps a top-level `version`; older lines do not.
    let variant = match value.get("version").and_then(|v| v.as_str()) {
        Some(v) if v.starts_with("2.") => "claude_code/2.0",
        Some(_) => "claude_code/1.x",
        None => "claude_code/unknown",
    };
    SchemaVariant::certain(SOURCE, variant)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse one record into zero or more normalized events.
fn parse_record(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: &serde_json::Value,
) -> Vec<CaptureEvent> {
    // Learn the session id the first time we see it (subagents keep their own
    // distinct sessionId — we never merge sidechains into the parent session).
    if ctx.session_id.is_none() {
        if let Some(sid) = value.get("sessionId").and_then(|v| v.as_str()) {
            ctx.session_id = Some(sid.to_string());
        }
    }

    // The record's native id and DAG parent.
    let uuid = value
        .get("uuid")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| content_id(&raw.bytes));
    let parent_uuid = value
        .get("parentUuid")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Idempotency: a repeated record (same uuid) yields nothing on the replay.
    if !ctx.first_seen(&uuid) {
        return Vec::new();
    }

    let ts = util::ts_from(value, &["timestamp", "time", "ts"]);
    let rec_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // The very first record opens the session: capture cwd / git / model /
    // version as a SessionStart, then continue parsing the same record's body.
    let mut events: Vec<CaptureEvent> = Vec::new();
    let is_session_start = ctx.project.is_none() && session_startable(rec_type);
    if is_session_start {
        let project = project_from(value);
        ctx.project = Some(project.clone());
        let model = string_field(value, "model").or_else(|| {
            value
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
        let tool_version = string_field(value, "version");
        events.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            session_start_id(&uuid),
            parent_uuid.clone(),
            ts,
            EventKind::SessionStart {
                cwd: project.cwd.clone(),
                git: project.git.clone(),
                model,
                tool_version,
            },
        ));
    }

    match rec_type {
        "user" => parse_turn(raw, ctx, value, &uuid, parent_uuid, ts, false, &mut events),
        "assistant" => parse_turn(raw, ctx, value, &uuid, parent_uuid, ts, true, &mut events),
        "summary" | "file-history-snapshot" | "system" => {
            // Recognized container records we do not normalize into a turn:
            // keep them lossless as Unknown (a summary explicitly maps to
            // Unknown per the format spec).
            events.push(util::unknown_event(SOURCE, ctx, raw, value.clone()));
        }
        _ => {
            events.push(util::unknown_event(SOURCE, ctx, raw, value.clone()));
        }
    }

    events
}

/// Whether a record type can open a session (only the dialogue records carry the
/// cwd/git/version we bind the project from).
fn session_startable(rec_type: &str) -> bool {
    matches!(rec_type, "user" | "assistant" | "system")
}

/// Parse a `user`/`assistant` record body into a turn plus any embedded
/// tool_use / tool_result / file-edit events.
#[allow(clippy::too_many_arguments)]
fn parse_turn(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: &serde_json::Value,
    uuid: &str,
    parent_uuid: Option<String>,
    ts: memscribe_core::Timestamp,
    is_assistant: bool,
    events: &mut Vec<CaptureEvent>,
) {
    let message = value.get("message");
    let blocks = message.and_then(|m| m.get("content"));

    let mut text = String::new();
    let mut thinking = String::new();
    let mut parts: Vec<Part> = Vec::new();
    // Tool calls / results discovered inside the content blocks; emitted after
    // the turn so the turn always precedes its embedded tool events.
    let mut tool_calls: Vec<(String, String, serde_json::Value)> = Vec::new();
    let mut tool_results: Vec<(String, bool, serde_json::Value)> = Vec::new();

    match blocks {
        // content as a plain string.
        Some(serde_json::Value::String(s)) => {
            push_text(&mut text, s);
            parts.push(Part::Text { text: s.clone() });
        }
        // content as an array of typed blocks.
        Some(serde_json::Value::Array(arr)) => {
            for block in arr {
                let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match btype {
                    "text" => {
                        let t = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        push_text(&mut text, t);
                        parts.push(Part::Text {
                            text: t.to_string(),
                        });
                    }
                    "thinking" => {
                        let t = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                        push_text(&mut thinking, t);
                        parts.push(Part::Thinking {
                            text: t.to_string(),
                        });
                    }
                    "tool_use" => {
                        let call_id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args = block
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        parts.push(Part::ToolUse {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            args: args.clone(),
                        });
                        tool_calls.push((call_id, name, args));
                    }
                    "tool_result" => {
                        let call_id = block
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let is_error = block
                            .get("is_error")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        let output = block
                            .get("content")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        parts.push(Part::ToolResult {
                            call_id: call_id.clone(),
                            output: output.clone(),
                        });
                        tool_results.push((call_id, !is_error, output));
                    }
                    "image" => {
                        let media_type = block
                            .get("source")
                            .and_then(|s| s.get("media_type"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        parts.push(Part::Image { media_type });
                    }
                    _ => {
                        parts.push(Part::Other { raw: block.clone() });
                    }
                }
            }
        }
        _ => {}
    }

    // The turn event itself, carrying the record's native uuid.
    if is_assistant {
        let model = message
            .and_then(|m| m.get("model"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| string_field(value, "model"));
        let usage = message.and_then(|m| m.get("usage")).map(usage_from);
        events.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            uuid.to_string(),
            parent_uuid.clone(),
            ts,
            EventKind::AssistantTurn {
                text,
                thinking: if thinking.is_empty() {
                    None
                } else {
                    Some(thinking)
                },
                model,
                usage,
                parts,
            },
        ));
    } else {
        events.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            uuid.to_string(),
            parent_uuid.clone(),
            ts,
            EventKind::UserTurn { text, parts },
        ));
    }

    // Embedded tool calls (assistant) — one ToolCall per tool_use block.
    for (idx, (call_id, name, args)) in tool_calls.into_iter().enumerate() {
        events.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            derived_id(uuid, "tool_call", idx),
            Some(uuid.to_string()),
            ts,
            EventKind::ToolCall {
                call_id,
                name,
                args,
            },
        ));
    }

    // Embedded tool results (user) — one ToolResult per tool_result block.
    for (idx, (call_id, ok, output)) in tool_results.into_iter().enumerate() {
        events.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            derived_id(uuid, "tool_result", idx),
            Some(uuid.to_string()),
            ts,
            EventKind::ToolResult {
                call_id,
                ok,
                output,
            },
        ));
    }

    // A top-level `toolUseResult` with a structured patch is a file edit.
    if let Some(edit) = parse_file_edit(value) {
        events.push(util::mk_event(
            SOURCE,
            ctx,
            raw,
            derived_id(uuid, "file_edit", 0),
            Some(uuid.to_string()),
            ts,
            edit,
        ));
    }
}

/// Build a [`EventKind::FileEdit`] from a record's top-level `toolUseResult`,
/// when it carries a `structuredPatch`. Returns `None` otherwise.
fn parse_file_edit(value: &serde_json::Value) -> Option<EventKind> {
    let tur = value.get("toolUseResult")?;
    let patch = tur.get("structuredPatch")?.as_array()?;

    let path = tur
        .get("filePath")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_default();
    let old = tur
        .get("oldString")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let new = tur
        .get("newString")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut unified_lines: Vec<String> = Vec::new();
    let mut added: u32 = 0;
    let mut removed: u32 = 0;
    for hunk in patch {
        if let Some(lines) = hunk.get("lines").and_then(|v| v.as_array()) {
            for line in lines {
                if let Some(s) = line.as_str() {
                    if let Some(first) = s.chars().next() {
                        if first == '+' {
                            added += 1;
                        } else if first == '-' {
                            removed += 1;
                        }
                    }
                    unified_lines.push(s.to_string());
                }
            }
        }
    }
    let unified = if unified_lines.is_empty() {
        None
    } else {
        Some(unified_lines.join("\n"))
    };

    // The originating tool call id. An edit's structuredPatch arrives on the
    // tool_result record, so the call id is the content block's `tool_use_id`
    // (a `tool_result`) — or, when the patch is colocated with the call, the
    // `tool_use` block's `id`. Either block type resolves the same call.
    let call_id = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter()
                .find_map(|b| match b.get("type").and_then(|v| v.as_str()) {
                    Some("tool_result") => b
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    Some("tool_use") => b.get("id").and_then(|v| v.as_str()).map(str::to_string),
                    _ => None,
                })
        });

    Some(EventKind::FileEdit {
        call_id,
        diff: Diff {
            path,
            old,
            new,
            unified,
            added_lines: added,
            removed_lines: removed,
        },
    })
}

// ---------------------------------------------------------------------------
// Small deterministic helpers
// ---------------------------------------------------------------------------

/// Build the project binding from a session-opening record's cwd / git fields.
fn project_from(value: &serde_json::Value) -> ProjectRef {
    let cwd = value
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let branch = value
        .get("gitBranch")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let sha = value
        .get("gitSha")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    let git = if branch.is_some() || !sha.is_empty() {
        Some(GitRef { sha, branch })
    } else {
        None
    };
    ProjectRef {
        cwd,
        repo_root: None,
        git,
    }
}

/// Read a top-level string field.
fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Build a [`Usage`] from a Claude Code `usage` object.
fn usage_from(u: &serde_json::Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(serde_json::Value::as_u64);
    Usage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cache_read_tokens: g("cache_read_input_tokens"),
        cache_creation_tokens: g("cache_creation_input_tokens"),
    }
}

/// Append `s` to a text accumulator, separating multiple text blocks by a single
/// newline so concatenation stays readable and deterministic.
fn push_text(acc: &mut String, s: &str) {
    if s.is_empty() {
        return;
    }
    if !acc.is_empty() {
        acc.push('\n');
    }
    acc.push_str(s);
}

/// A deterministic event id for the SessionStart synthesized from a record.
fn session_start_id(uuid: &str) -> String {
    content_id(format!("claude_code/session_start/{uuid}").as_bytes())
}

/// A deterministic, collision-free id for a secondary event derived from a
/// record's `uuid` (the primary turn keeps the bare `uuid`).
fn derived_id(uuid: &str, kind: &str, idx: usize) -> String {
    content_id(format!("claude_code/{kind}/{idx}/{uuid}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(s: &str) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("session.jsonl", 0, 1))
    }

    fn parse_all(lines: &[&str]) -> Vec<CaptureEvent> {
        let adapter = ClaudeCodeAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for line in lines {
            let evs = adapter.parse(&raw(line), &mut ctx).expect("never errors");
            out.extend(evs);
        }
        out
    }

    fn tags(evs: &[CaptureEvent]) -> Vec<&'static str> {
        evs.iter().map(|e| e.kind.tag()).collect()
    }

    // --- TDD: the normalized sequence for a small dialogue --------------------

    #[test]
    fn first_user_record_yields_session_start_then_user_turn() {
        let line = r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-22T10:00:00Z","sessionId":"s1","cwd":"/repo","gitBranch":"main","version":"2.0.1","message":{"role":"user","content":"Let's use Postgres instead of MySQL."}}"#;
        let evs = parse_all(&[line]);
        assert_eq!(tags(&evs), vec!["session_start", "user_turn"]);
        // Session + project were learned from the first record.
        assert_eq!(evs[1].session_id, "s1");
        match &evs[0].kind {
            EventKind::SessionStart {
                cwd,
                git,
                tool_version,
                ..
            } => {
                assert_eq!(cwd.to_str(), Some("/repo"));
                assert_eq!(git.as_ref().and_then(|g| g.branch.as_deref()), Some("main"));
                assert_eq!(tool_version.as_deref(), Some("2.0.1"));
            }
            other => panic!("expected session_start, got {other:?}"),
        }
        match &evs[1].kind {
            EventKind::UserTurn { text, .. } => {
                assert_eq!(text, "Let's use Postgres instead of MySQL.");
            }
            other => panic!("expected user_turn, got {other:?}"),
        }
    }

    #[test]
    fn assistant_with_tool_use_yields_turn_then_tool_call() {
        let session = r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-22T10:00:00Z","sessionId":"s1","cwd":"/repo","gitBranch":"main","version":"2.0.1","message":{"role":"user","content":"go"}}"#;
        let asst = r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-06-22T10:00:01Z","sessionId":"s1","message":{"role":"assistant","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":2,"cache_creation_input_tokens":1},"content":[{"type":"text","text":"Editing now."},{"type":"tool_use","id":"call_1","name":"Edit","input":{"file_path":"/repo/a.rs"}}]}}"#;
        let evs = parse_all(&[session, asst]);
        assert_eq!(
            tags(&evs),
            vec!["session_start", "user_turn", "assistant_turn", "tool_call"]
        );
        match &evs[2].kind {
            EventKind::AssistantTurn {
                text, model, usage, ..
            } => {
                assert_eq!(text, "Editing now.");
                assert_eq!(model.as_deref(), Some("claude-opus-4-8"));
                let u = usage.as_ref().expect("usage present");
                assert_eq!(u.input_tokens, Some(10));
                assert_eq!(u.cache_read_tokens, Some(2));
                assert_eq!(u.cache_creation_tokens, Some(1));
            }
            other => panic!("expected assistant_turn, got {other:?}"),
        }
        match &evs[3].kind {
            EventKind::ToolCall { call_id, name, .. } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(name, "Edit");
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
        // The secondary tool_call carries a distinct, deterministic id.
        assert_ne!(evs[2].event_id, evs[3].event_id);
        assert_eq!(evs[3].parent_id.as_deref(), Some("a1"));
    }

    #[test]
    fn tool_result_record_yields_user_turn_then_tool_result() {
        let session = r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-22T10:00:00Z","sessionId":"s1","cwd":"/repo","version":"2.0.1","message":{"role":"user","content":"go"}}"#;
        let res = r#"{"type":"user","uuid":"u2","parentUuid":"a1","timestamp":"2026-06-22T10:00:02Z","sessionId":"s1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"ok","is_error":false}]}}"#;
        let evs = parse_all(&[session, res]);
        // The session record emits session_start + user_turn; the tool_result
        // record emits its own (empty) user_turn carrier + the tool_result.
        assert_eq!(
            tags(&evs),
            vec!["session_start", "user_turn", "user_turn", "tool_result"]
        );
        let res_ev = evs.iter().find(|e| e.kind.tag() == "tool_result").unwrap();
        match &res_ev.kind {
            EventKind::ToolResult { call_id, ok, .. } => {
                assert_eq!(call_id, "call_1");
                assert!(*ok);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    // --- TDD: a decision + an edit produces UserTurn then FileEdit ------------

    #[test]
    fn decision_then_edit_yields_user_turn_then_file_edit() {
        let decision = r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-22T10:00:00Z","sessionId":"s1","cwd":"/repo","gitBranch":"main","version":"2.0.1","message":{"role":"user","content":"Let's use Postgres instead of MySQL."}}"#;
        let edit = r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-06-22T10:00:01Z","sessionId":"s1","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"tool_use","id":"call_1","name":"Edit","input":{"file_path":"/repo/db.rs"}}]},"toolUseResult":{"filePath":"/repo/db.rs","oldString":"mysql","newString":"postgres","structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-mysql","+postgres"]}]}}"#;
        let evs = parse_all(&[decision, edit]);
        let t = tags(&evs);
        // user_turn appears (the decision), and a file_edit appears.
        assert!(t.contains(&"user_turn"), "tags: {t:?}");
        assert!(t.contains(&"file_edit"), "tags: {t:?}");
        // The user_turn precedes the file_edit.
        let ut = t.iter().position(|x| *x == "user_turn").unwrap();
        let fe = t.iter().position(|x| *x == "file_edit").unwrap();
        assert!(ut < fe, "user_turn must precede file_edit: {t:?}");

        let edit_ev = evs.iter().find(|e| e.kind.tag() == "file_edit").unwrap();
        match &edit_ev.kind {
            EventKind::FileEdit { call_id, diff } => {
                assert_eq!(call_id.as_deref(), Some("call_1"));
                assert_eq!(diff.path.to_str(), Some("/repo/db.rs"));
                assert_eq!(diff.added_lines, 1);
                assert_eq!(diff.removed_lines, 1);
                assert_eq!(diff.old.as_deref(), Some("mysql"));
                assert_eq!(diff.new.as_deref(), Some("postgres"));
                assert_eq!(diff.unified.as_deref(), Some("-mysql\n+postgres"));
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }

    // --- TDD: tool failure → FileEdit references a failing result -------------

    #[test]
    fn failed_edit_keeps_call_id_so_segmenter_can_drop_it() {
        // The FileEdit must carry the call_id, and a sibling tool_result with
        // ok=false must exist, so the segmenter drops the episode.
        let session = r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-22T10:00:00Z","sessionId":"s1","cwd":"/repo","version":"2.0.1","message":{"role":"user","content":"edit"}}"#;
        let edit = r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-06-22T10:00:01Z","sessionId":"s1","message":{"role":"assistant","content":[{"type":"tool_use","id":"call_x","name":"Edit","input":{}}]},"toolUseResult":{"filePath":"/repo/x.rs","oldString":"a","newString":"b","structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-a","+b"]}]}}"#;
        let fail = r#"{"type":"user","uuid":"u2","parentUuid":"a1","timestamp":"2026-06-22T10:00:02Z","sessionId":"s1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_x","content":"boom","is_error":true}]}}"#;
        let evs = parse_all(&[session, edit, fail]);

        let edit_ev = evs.iter().find(|e| e.kind.tag() == "file_edit").unwrap();
        let edit_call = match &edit_ev.kind {
            EventKind::FileEdit { call_id, .. } => call_id.clone(),
            _ => unreachable!(),
        };
        assert_eq!(edit_call.as_deref(), Some("call_x"));

        let res_ev = evs.iter().find(|e| e.kind.tag() == "tool_result").unwrap();
        match &res_ev.kind {
            EventKind::ToolResult { call_id, ok, .. } => {
                assert_eq!(call_id, "call_x");
                assert!(!ok, "the failing edit's result must be ok=false");
            }
            _ => unreachable!(),
        }
    }

    // --- TDD: never panics on garbage ----------------------------------------

    #[test]
    fn garbage_input_never_panics_and_is_lossless() {
        let adapter = ClaudeCodeAdapter;
        let mut ctx = ParseCtx::new();
        // Not JSON at all.
        let e1 = adapter.parse(&raw("}{ not json"), &mut ctx).unwrap();
        assert_eq!(e1.len(), 1);
        assert_eq!(e1[0].kind.tag(), "unknown");
        // Valid JSON, unrecognized shape.
        let e2 = adapter
            .parse(&raw(r#"{"hello":"world"}"#), &mut ctx)
            .unwrap();
        assert_eq!(e2.len(), 1);
        assert_eq!(e2[0].kind.tag(), "unknown");
        // Blank line yields nothing.
        let e3 = adapter.parse(&raw("   "), &mut ctx).unwrap();
        assert!(e3.is_empty());
        // A record with a non-string/array content does not panic.
        let e4 = adapter
            .parse(
                &raw(r#"{"type":"user","uuid":"z","message":{"content":42}}"#),
                &mut ctx,
            )
            .unwrap();
        assert!(!e4.is_empty());
    }

    // --- TDD: dedup / idempotency on a repeated record -----------------------

    #[test]
    fn repeated_uuid_is_deduped_to_empty() {
        let adapter = ClaudeCodeAdapter;
        let mut ctx = ParseCtx::new();
        let line = r#"{"type":"user","uuid":"dup","parentUuid":null,"timestamp":"2026-06-22T10:00:00Z","sessionId":"s1","cwd":"/repo","version":"2.0.1","message":{"role":"user","content":"hi"}}"#;
        let first = adapter.parse(&raw(line), &mut ctx).unwrap();
        assert!(!first.is_empty());
        let second = adapter.parse(&raw(line), &mut ctx).unwrap();
        assert!(second.is_empty(), "a repeated uuid must yield nothing");
    }

    // --- summary → Unknown ----------------------------------------------------

    #[test]
    fn summary_record_is_unknown() {
        let line = r#"{"type":"summary","summary":"A recap","leafUuid":"x"}"#;
        let evs = parse_all(&[line]);
        assert_eq!(tags(&evs), vec!["unknown"]);
    }

    // --- ban turn surfaces as a UserTurn (gate runs downstream) ---------------

    #[test]
    fn ban_turn_is_a_user_turn() {
        let line = r#"{"type":"user","uuid":"b1","parentUuid":null,"timestamp":"2026-06-22T10:00:00Z","sessionId":"s1","cwd":"/repo","version":"2.0.1","message":{"role":"user","content":"We will never add a dependency on left-pad."}}"#;
        let evs = parse_all(&[line]);
        assert_eq!(tags(&evs), vec!["session_start", "user_turn"]);
        match &evs[1].kind {
            EventKind::UserTurn { text, .. } => {
                assert!(text.contains("never add a dependency"));
            }
            other => panic!("expected user_turn, got {other:?}"),
        }
    }

    // --- determinism ----------------------------------------------------------

    #[test]
    fn parsing_is_deterministic() {
        let line = r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-22T10:00:00Z","sessionId":"s1","cwd":"/repo","gitBranch":"main","version":"2.0.1","message":{"role":"user","content":"Let's use Postgres."}}"#;
        let a = parse_all(&[line]);
        let b = parse_all(&[line]);
        assert_eq!(a, b);
    }

    // --- fingerprint ----------------------------------------------------------

    #[test]
    fn fingerprint_recognizes_claude_2x() {
        let adapter = ClaudeCodeAdapter;
        let v = adapter.schema_fingerprint(&raw(
            r#"{"type":"user","uuid":"u1","sessionId":"s1","version":"2.0.1","message":{"content":"hi"}}"#,
        ));
        assert_eq!(v.source, SourceKind::ClaudeCode);
        assert_eq!(v.variant, "claude_code/2.0");
        assert_eq!(v.confidence, 100);
    }

    #[test]
    fn fingerprint_unknown_for_foreign_json() {
        let adapter = ClaudeCodeAdapter;
        let v = adapter.schema_fingerprint(&raw(r#"{"foo":"bar"}"#));
        assert_eq!(v.confidence, 0);
    }

    // An edit's structuredPatch arrives on the tool_result record, so the
    // FileEdit must resolve its call_id from the `tool_result` block's
    // `tool_use_id` — this is what lets the segmenter drop a failed edit.
    #[test]
    fn file_edit_on_result_record_resolves_call_id_from_tool_result_block() {
        let session = r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-22T10:00:00Z","sessionId":"s1","cwd":"/repo","version":"2.0.1","message":{"role":"user","content":"go"}}"#;
        // The structuredPatch is colocated with the tool_result block (no
        // tool_use block on this record), exactly as Claude Code writes it.
        let result = r#"{"type":"user","uuid":"u2","parentUuid":"a1","timestamp":"2026-06-22T10:00:02Z","sessionId":"s1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_z","content":"ok","is_error":false}]},"toolUseResult":{"filePath":"/repo/z.rs","oldString":"a","newString":"b","structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-a","+b"]}]}}"#;
        let evs = parse_all(&[session, result]);
        let edit = evs.iter().find(|e| e.kind.tag() == "file_edit").unwrap();
        match &edit.kind {
            EventKind::FileEdit { call_id, .. } => {
                assert_eq!(call_id.as_deref(), Some("call_z"));
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }
}
