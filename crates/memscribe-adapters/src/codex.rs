//! Codex CLI adapter.
//!
//! Transcripts: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl[.zst]`,
//! `{timestamp,type,payload}` JSONL. Dialogue: `response_item.message`
//! (role user/assistant), `function_call`/`function_call_output` paired by
//! `call_id`; `session_meta.git`. Edits: `function_call` `name=apply_patch` →
//! V4A diff in `arguments` → parse to `Diff`. Quirks: handle `.jsonl.zst`; the
//! protocol enum ≠ wire format (build to wire data); `history.jsonl` ≠ rollouts;
//! files may be `0644` (secrets).
//!
//! The io reader decompresses `.zst` before records reach `parse`, so this
//! module only ever sees plain JSON lines. Every record maps to zero or more
//! [`CaptureEvent`]s; any shape we do not recognize is routed to
//! [`memscribe_core::EventKind::Unknown`] via [`util::unknown_event`] so the
//! stream stays lossless.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, Part,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, TranscriptAdapter, TranscriptHandle,
};
use std::path::PathBuf;

/// Adapter for OpenAI Codex CLI transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAdapter;

impl TranscriptAdapter for CodexAdapter {
    fn source_kind(&self) -> SourceKind {
        SourceKind::Codex
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        discover_rollouts(cfg)
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        // Blank lines carry nothing; skip them (the io layer may hand us trailers).
        let Some(value) = util::parse_json_line(raw) else {
            return Ok(Vec::new());
        };
        Ok(parse_record(raw, ctx, &value))
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        match util::parse_json_line(sample) {
            Some(v) if is_codex_record(&v) => {
                SchemaVariant::certain(SourceKind::Codex, "codex/rollout-v2")
            }
            _ => SchemaVariant::unknown(SourceKind::Codex),
        }
    }
}

/// A record is recognizably a Codex rollout line if it carries a top-level
/// `type` and `payload`. We keep this lenient so version churn still fingerprints.
fn is_codex_record(value: &serde_json::Value) -> bool {
    value.get("type").and_then(|t| t.as_str()).is_some() && value.get("payload").is_some()
}

/// Parse one decoded record into zero or more events. Never panics.
fn parse_record(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: &serde_json::Value,
) -> Vec<CaptureEvent> {
    let rec_type = value.get("type").and_then(|t| t.as_str());
    let payload = value.get("payload");

    match (rec_type, payload) {
        (Some("session_meta"), Some(p)) => parse_session_meta(raw, ctx, value, p),
        (Some("response_item"), Some(p)) => parse_response_item(raw, ctx, value, p),
        // `event_msg` and `turn_context` are valid but carry no normalized
        // payload of their own — preserve them losslessly as Unknown.
        _ => vec![util::unknown_event(
            SourceKind::Codex,
            ctx,
            raw,
            value.clone(),
        )],
    }
}

/// `session_meta` → [`EventKind::SessionStart`]. Sets `ctx.session_id` and
/// `ctx.project` so later records inherit the binding.
fn parse_session_meta(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: &serde_json::Value,
    payload: &serde_json::Value,
) -> Vec<CaptureEvent> {
    // Learn the session id (used by mk_event for every subsequent event).
    if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
        ctx.session_id = Some(id.to_string());
    }

    let cwd = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let git = parse_git(payload.get("git"));

    // Populate the project binding from the session-start record.
    ctx.project = Some(ProjectRef {
        cwd: cwd.clone(),
        repo_root: None,
        git: git.clone(),
    });

    // Codex labels its version under either `cli_version` or `originator`.
    let tool_version = payload
        .get("cli_version")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("originator").and_then(|v| v.as_str()))
        .map(str::to_string);

    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let ts = util::ts_from(value, &["timestamp", "time", "ts"]);
    let event_id = session_event_id(ctx, raw);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }

    let kind = EventKind::SessionStart {
        cwd,
        git,
        model,
        tool_version,
    };
    vec![util::mk_event(
        SourceKind::Codex,
        ctx,
        raw,
        event_id,
        None,
        ts,
        kind,
    )]
}

/// Build a stable event id for the session-start record: prefer the session id,
/// else a content hash, so dedup/idempotency holds for repeated meta lines.
fn session_event_id(ctx: &ParseCtx, raw: &RawRecord) -> String {
    if let Some(id) = ctx.session_id.as_deref() {
        return format!("session_meta:{id}");
    }
    content_id(&raw.bytes)
}

/// Parse a `git` object `{sha, branch}` into a [`GitRef`].
fn parse_git(git: Option<&serde_json::Value>) -> Option<GitRef> {
    let g = git?;
    let sha = g.get("sha").and_then(|v| v.as_str())?.to_string();
    let branch = g.get("branch").and_then(|v| v.as_str()).map(str::to_string);
    Some(GitRef { sha, branch })
}

/// A `response_item` payload has its own `type`. Dispatch on it.
fn parse_response_item(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    value: &serde_json::Value,
    payload: &serde_json::Value,
) -> Vec<CaptureEvent> {
    let item_type = payload.get("type").and_then(|t| t.as_str());
    let ts = util::ts_from(value, &["timestamp", "time", "ts"]);

    match item_type {
        Some("message") => parse_message(raw, ctx, payload, ts),
        Some("function_call") => parse_function_call(raw, ctx, payload, ts),
        Some("function_call_output") => parse_function_call_output(raw, ctx, payload, ts),
        // `reasoning` and any other item type carry no first-class mapping —
        // keep them lossless as Unknown rather than guessing.
        _ => vec![util::unknown_event(
            SourceKind::Codex,
            ctx,
            raw,
            value.clone(),
        )],
    }
}

/// `message` → [`EventKind::UserTurn`] / [`EventKind::AssistantTurn`].
fn parse_message(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    payload: &serde_json::Value,
    ts: memscribe_core::Timestamp,
) -> Vec<CaptureEvent> {
    let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let (text, parts) = flatten_content(payload.get("content"));

    let event_id = item_event_id(payload, raw);
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }

    let kind = match role {
        "user" => EventKind::UserTurn { text, parts },
        "assistant" => EventKind::AssistantTurn {
            text,
            thinking: None,
            model: payload
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            usage: None,
            parts,
        },
        // A message with an unexpected role: keep it lossless.
        _ => {
            return vec![util::unknown_event(
                SourceKind::Codex,
                ctx,
                raw,
                payload.clone(),
            )];
        }
    };
    vec![util::mk_event(
        SourceKind::Codex,
        ctx,
        raw,
        event_id,
        None,
        ts,
        kind,
    )]
}

/// Flatten a `content` array of `{type:input_text|output_text, text}` parts into
/// a joined text blob and the structured [`Part`] list.
fn flatten_content(content: Option<&serde_json::Value>) -> (String, Vec<Part>) {
    let mut text = String::new();
    let mut parts: Vec<Part> = Vec::new();
    let Some(items) = content.and_then(|c| c.as_array()) else {
        return (text, parts);
    };
    for item in items {
        let ptype = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ptype {
            "input_text" | "output_text" | "text" => {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                    parts.push(Part::Text {
                        text: t.to_string(),
                    });
                }
            }
            "input_image" | "image" | "output_image" => {
                parts.push(Part::Image {
                    media_type: item
                        .get("media_type")
                        .or_else(|| item.get("image_url"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                });
            }
            _ => parts.push(Part::Other { raw: item.clone() }),
        }
    }
    (text, parts)
}

/// `function_call` → [`EventKind::ToolCall`], plus one
/// [`EventKind::FileEdit`] per file section when the call is `apply_patch`.
fn parse_function_call(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    payload: &serde_json::Value,
    ts: memscribe_core::Timestamp,
) -> Vec<CaptureEvent> {
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let call_id = payload
        .get("call_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // `arguments` is a JSON *string* on the wire; parse it, falling back to a
    // string value when it is not valid JSON (still lossless).
    let args = parse_arguments(payload.get("arguments"));

    let event_id = call_id
        .clone()
        .unwrap_or_else(|| item_event_id(payload, raw));
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }

    // Remember the call name so a later `function_call_output` can pair with it.
    if let Some(cid) = &call_id {
        ctx.call_names.insert(cid.clone(), name.clone());
    }

    let mut events = Vec::new();
    events.push(util::mk_event(
        SourceKind::Codex,
        ctx,
        raw,
        event_id.clone(),
        None,
        ts,
        EventKind::ToolCall {
            call_id: call_id.clone().unwrap_or_default(),
            name: name.clone(),
            args: args.clone(),
        },
    ));

    // apply_patch also yields one FileEdit per file section in the V4A patch.
    if name == "apply_patch" {
        if let Some(patch) = extract_patch_text(&args) {
            for section in parse_v4a_patch(&patch) {
                // A unique, deterministic id per FileEdit so dedup does not
                // collapse multiple edits from one call.
                let edit_id =
                    content_id(format!("{}:edit:{}", event_id, section.path.display()).as_bytes());
                if !ctx.first_seen(&edit_id) {
                    continue;
                }
                events.push(util::mk_event(
                    SourceKind::Codex,
                    ctx,
                    raw,
                    edit_id,
                    call_id.clone(),
                    ts,
                    EventKind::FileEdit {
                        call_id: call_id.clone(),
                        diff: section.into_diff(),
                    },
                ));
            }
        }
    }

    events
}

/// `function_call_output` → [`EventKind::ToolResult`]. `ok` is false when the
/// output indicates an error.
fn parse_function_call_output(
    raw: &RawRecord,
    ctx: &mut ParseCtx,
    payload: &serde_json::Value,
    ts: memscribe_core::Timestamp,
) -> Vec<CaptureEvent> {
    let call_id = payload
        .get("call_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();

    let output = payload
        .get("output")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let ok = output_is_ok(&output);

    let event_id = if call_id.is_empty() {
        item_event_id(payload, raw)
    } else {
        format!("{call_id}:output")
    };
    if !ctx.first_seen(&event_id) {
        return Vec::new();
    }

    // Record the result outcome so downstream pairing (and the segmenter) can
    // drop edits from a failed call.
    if !call_id.is_empty() {
        ctx.call_ok.insert(call_id.clone(), ok);
    }

    vec![util::mk_event(
        SourceKind::Codex,
        ctx,
        raw,
        event_id,
        Some(call_id.clone()),
        ts,
        EventKind::ToolResult {
            call_id,
            ok,
            output,
        },
    )]
}

/// Decide whether a tool output indicates success. Codex outputs are sometimes a
/// bare string, sometimes an object `{output, metadata:{exit_code}}` or carry a
/// `success` flag. We treat an explicit failure signal as not-ok; otherwise ok.
fn output_is_ok(output: &serde_json::Value) -> bool {
    match output {
        serde_json::Value::String(s) => !string_indicates_error(s),
        serde_json::Value::Object(map) => {
            // Explicit booleans win.
            if let Some(b) = map.get("success").and_then(|v| v.as_bool()) {
                return b;
            }
            if let Some(b) = map.get("ok").and_then(|v| v.as_bool()) {
                return b;
            }
            // A non-zero exit code is a failure.
            if let Some(code) = map
                .get("exit_code")
                .or_else(|| map.get("exitCode"))
                .and_then(serde_json::Value::as_i64)
            {
                return code == 0;
            }
            if let Some(code) = map
                .get("metadata")
                .and_then(|m| m.get("exit_code").or_else(|| m.get("exitCode")))
                .and_then(serde_json::Value::as_i64)
            {
                return code == 0;
            }
            // Otherwise sniff the textual output for an error signature.
            if let Some(s) = map.get("output").and_then(|v| v.as_str()) {
                return !string_indicates_error(s);
            }
            true
        }
        // Null / numbers / arrays: assume ok (no negative signal).
        _ => true,
    }
}

/// Heuristic, deterministic error detection for a free-text tool output.
fn string_indicates_error(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("error")
        || lower.contains("failed")
        || lower.contains("failure")
        || lower.contains("traceback")
        || lower.contains("exception")
        || lower.contains("not found")
        || lower.contains("no such file")
        || lower.contains("patch does not apply")
        || lower.contains("could not apply")
}

/// `arguments` arrives as a JSON-encoded string. Parse it; if it is not a JSON
/// string (or not valid JSON), preserve whatever value was there verbatim.
fn parse_arguments(arguments: Option<&serde_json::Value>) -> serde_json::Value {
    match arguments {
        Some(serde_json::Value::String(s)) => {
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.clone()))
        }
        Some(other) => other.clone(),
        None => serde_json::Value::Null,
    }
}

/// Pull the V4A patch text out of parsed `apply_patch` arguments. Codex stores it
/// under `input` or `patch`; tolerate a bare string too.
fn extract_patch_text(args: &serde_json::Value) -> Option<String> {
    match args {
        serde_json::Value::Object(map) => map
            .get("input")
            .or_else(|| map.get("patch"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// One file section parsed out of a V4A patch.
struct PatchSection {
    path: PathBuf,
    body: String,
    added: u32,
    removed: u32,
}

impl PatchSection {
    fn into_diff(self) -> Diff {
        Diff {
            path: self.path,
            old: None,
            new: None,
            unified: Some(self.body),
            added_lines: self.added,
            removed_lines: self.removed,
        }
    }
}

/// Parse a V4A patch string (the `*** Begin Patch` / `*** End Patch` envelope)
/// into one [`PatchSection`] per file. Deterministic, allocation-only, and
/// panic-free: it indexes nothing and never unwraps.
fn parse_v4a_patch(patch: &str) -> Vec<PatchSection> {
    let mut sections: Vec<PatchSection> = Vec::new();
    let mut current: Option<PatchSection> = None;

    for line in patch.lines() {
        if let Some(path) = section_header(line) {
            if let Some(sec) = current.take() {
                sections.push(sec);
            }
            current = Some(PatchSection {
                path: PathBuf::from(path),
                body: String::new(),
                added: 0,
                removed: 0,
            });
            continue;
        }

        // The envelope markers themselves are not part of any section body.
        if line.starts_with("*** Begin Patch") || line.starts_with("*** End Patch") {
            continue;
        }

        if let Some(sec) = current.as_mut() {
            // Count added/removed lines. A leading '+'/'-' marks the change; '@@'
            // and context (leading space) lines are body but not counted.
            if let Some(first) = line.as_bytes().first() {
                match first {
                    b'+' => sec.added += 1,
                    b'-' => sec.removed += 1,
                    _ => {}
                }
            }
            if !sec.body.is_empty() {
                sec.body.push('\n');
            }
            sec.body.push_str(line);
        }
    }

    if let Some(sec) = current.take() {
        sections.push(sec);
    }
    sections
}

/// If `line` is a V4A file header (`*** Update/Add/Delete File: <path>`), return
/// the path. Otherwise `None`.
fn section_header(line: &str) -> Option<&str> {
    for prefix in ["*** Update File: ", "*** Add File: ", "*** Delete File: "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(rest.trim());
        }
    }
    None
}

/// A stable event id for a `response_item` that carries no native id: prefer an
/// explicit `id`, else a content hash of the raw record bytes.
fn item_event_id(payload: &serde_json::Value, raw: &RawRecord) -> String {
    payload
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| content_id(&raw.bytes))
}

/// Discover Codex rollout transcripts under `~/.codex/sessions/**/rollout-*`.
/// `history.jsonl` is the prompt history file, not a rollout, so it is skipped.
fn discover_rollouts(cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
    let root = codex_sessions_root(cfg);
    if !root.exists() {
        return Vec::new();
    }

    let mut handles: Vec<TranscriptHandle> = Vec::new();
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == "history.jsonl" {
            continue; // prompt history, not a rollout
        }
        if !name.starts_with("rollout-") {
            continue;
        }
        let compressed = name.ends_with(".zst");
        let is_jsonl = name.ends_with(".jsonl") || name.ends_with(".jsonl.zst");
        if !is_jsonl {
            continue;
        }
        handles.push(TranscriptHandle {
            path: path.to_path_buf(),
            source: SourceKind::Codex,
            session_hint: session_hint_from_name(name),
            compressed,
        });
    }

    // Deterministic order regardless of filesystem walk order.
    handles.sort_by(|a, b| a.path.cmp(&b.path));
    handles
}

/// The `~/.codex/sessions` root, honoring a `CODEX_HOME` override.
fn codex_sessions_root(cfg: &DiscoverCfg) -> PathBuf {
    if let Some(p) = cfg.overrides.get("CODEX_HOME") {
        return p.join("sessions");
    }
    cfg.home_dir().join(".codex").join("sessions")
}

/// Derive a session-id hint from a `rollout-<...>.jsonl[.zst]` filename, if one
/// is embedded after the `rollout-` prefix.
fn session_hint_from_name(name: &str) -> Option<String> {
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))
        .unwrap_or(name);
    let rest = stem.strip_prefix("rollout-")?;
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(s: &str) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("rollout-test.jsonl", 0, 1))
    }

    /// Parse a whole multi-line transcript through one shared context, the way
    /// the pipeline does. Returns the flat event stream.
    fn parse_all(lines: &[&str]) -> Vec<CaptureEvent> {
        let adapter = CodexAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let r = RawRecord::from_line(
                line,
                SourceLocation::new("rollout-test.jsonl", i as u64, (i + 1) as u64),
            );
            out.extend(adapter.parse(&r, &mut ctx).expect("never errors"));
        }
        out
    }

    fn tags(events: &[CaptureEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.kind.tag()).collect()
    }

    const META: &str = r#"{"timestamp":"2026-06-22T10:00:00Z","type":"session_meta","payload":{"id":"sess-abc","cwd":"/home/u/proj","git":{"sha":"deadbeef","branch":"main"},"cli_version":"0.5.0"}}"#;

    #[test]
    fn session_meta_maps_to_session_start_with_project() {
        let events = parse_all(&[META]);
        assert_eq!(tags(&events), vec!["session_start"]);
        let ev = &events[0];
        assert_eq!(ev.session_id, "sess-abc");
        match &ev.kind {
            EventKind::SessionStart {
                cwd,
                git,
                tool_version,
                ..
            } => {
                assert_eq!(cwd, &PathBuf::from("/home/u/proj"));
                let g = git.as_ref().expect("git present");
                assert_eq!(g.sha, "deadbeef");
                assert_eq!(g.branch.as_deref(), Some("main"));
                assert_eq!(tool_version.as_deref(), Some("0.5.0"));
            }
            other => panic!("expected SessionStart, got {other:?}"),
        }
        // Project binding is stamped from session_meta.
        assert_eq!(ev.project.cwd, PathBuf::from("/home/u/proj"));
        assert!(ev.project.git.is_some());
    }

    #[test]
    fn user_and_assistant_messages_map_to_turns() {
        let user = r#"{"timestamp":"2026-06-22T10:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Let's use Postgres instead of MySQL."}]}}"#;
        let asst = r#"{"timestamp":"2026-06-22T10:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Sounds good."}]}}"#;
        let events = parse_all(&[META, user, asst]);
        assert_eq!(
            tags(&events),
            vec!["session_start", "user_turn", "assistant_turn"]
        );
        match &events[1].kind {
            EventKind::UserTurn { text, .. } => {
                assert_eq!(text, "Let's use Postgres instead of MySQL.");
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
        // The user-turn inherits the session id learned from session_meta.
        assert_eq!(events[1].session_id, "sess-abc");
    }

    #[test]
    fn function_call_and_output_pair_by_call_id() {
        let call = r#"{"timestamp":"2026-06-22T10:00:03Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}","call_id":"call-1"}}"#;
        let out = r#"{"timestamp":"2026-06-22T10:00:04Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"file1\nfile2"}}"#;
        let events = parse_all(&[META, call, out]);
        assert_eq!(
            tags(&events),
            vec!["session_start", "tool_call", "tool_result"]
        );
        match &events[1].kind {
            EventKind::ToolCall {
                call_id,
                name,
                args,
            } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(name, "shell");
                // arguments string was parsed into JSON.
                assert_eq!(args["command"][0], "ls");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        match &events[2].kind {
            EventKind::ToolResult { call_id, ok, .. } => {
                assert_eq!(call_id, "call-1");
                assert!(*ok, "plain output should be ok");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn apply_patch_emits_tool_call_then_one_file_edit_per_section() {
        let patch = "*** Begin Patch\n*** Update File: src/db.rs\n@@\n-let url = \"mysql://...\";\n+let url = \"postgres://...\";\n*** Add File: src/pg.rs\n+pub fn connect() {}\n*** End Patch";
        let args = serde_json::json!({ "input": patch }).to_string();
        let call = serde_json::json!({
            "timestamp": "2026-06-22T10:00:05Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "apply_patch",
                "arguments": args,
                "call_id": "call-edit"
            }
        })
        .to_string();
        let events = parse_all(&[META, &call]);
        assert_eq!(
            tags(&events),
            vec!["session_start", "tool_call", "file_edit", "file_edit"]
        );
        // First edit: Update File with one add + one remove.
        match &events[2].kind {
            EventKind::FileEdit { call_id, diff } => {
                assert_eq!(call_id.as_deref(), Some("call-edit"));
                assert_eq!(diff.path, PathBuf::from("src/db.rs"));
                assert_eq!(diff.added_lines, 1);
                assert_eq!(diff.removed_lines, 1);
                assert!(diff.unified.as_deref().unwrap().contains("postgres"));
            }
            other => panic!("expected FileEdit, got {other:?}"),
        }
        match &events[3].kind {
            EventKind::FileEdit { diff, .. } => {
                assert_eq!(diff.path, PathBuf::from("src/pg.rs"));
                assert_eq!(diff.added_lines, 1);
                assert_eq!(diff.removed_lines, 0);
            }
            other => panic!("expected FileEdit, got {other:?}"),
        }
    }

    #[test]
    fn decision_then_edit_yields_user_turn_then_file_edit() {
        let user = r#"{"timestamp":"2026-06-22T10:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Let's use Postgres instead of MySQL."}]}}"#;
        let patch = "*** Begin Patch\n*** Update File: src/db.rs\n+let url = \"postgres://...\";\n*** End Patch";
        let args = serde_json::json!({ "patch": patch }).to_string();
        let call = serde_json::json!({
            "type": "response_item",
            "payload": {"type":"function_call","name":"apply_patch","arguments":args,"call_id":"c1"}
        })
        .to_string();
        let events = parse_all(&[META, user, &call]);
        let t = tags(&events);
        // The decision (UserTurn) precedes the FileEdit in stream order.
        let user_idx = t.iter().position(|x| *x == "user_turn").unwrap();
        let edit_idx = t.iter().position(|x| *x == "file_edit").unwrap();
        assert!(
            user_idx < edit_idx,
            "user turn must precede file edit: {t:?}"
        );
    }

    #[test]
    fn failed_function_call_output_marks_not_ok() {
        let out = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c9","output":"error: patch does not apply"}}"#;
        let events = parse_all(&[META, out]);
        match &events[1].kind {
            EventKind::ToolResult { ok, .. } => assert!(!*ok, "error output must be not-ok"),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn exit_code_object_output_failure_is_not_ok() {
        let out = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c8","output":{"output":"done","metadata":{"exit_code":1}}}}"#;
        let events = parse_all(&[META, out]);
        match &events[1].kind {
            EventKind::ToolResult { ok, .. } => assert!(!*ok, "exit_code 1 must be not-ok"),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn unrecognized_record_routes_to_unknown_losslessly() {
        let weird =
            r#"{"timestamp":"2026-06-22T10:00:09Z","type":"turn_context","payload":{"foo":"bar"}}"#;
        let events = parse_all(&[weird]);
        assert_eq!(tags(&events), vec!["unknown"]);
        match &events[0].kind {
            EventKind::Unknown { raw_type, raw } => {
                assert_eq!(raw_type, "turn_context");
                assert_eq!(raw["payload"]["foo"], "bar");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_item_routes_to_unknown() {
        let reasoning =
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":"thinking..."}}"#;
        let events = parse_all(&[META, reasoning]);
        assert_eq!(tags(&events), vec!["session_start", "unknown"]);
    }

    #[test]
    fn garbage_input_never_panics() {
        // Invalid JSON, partial JSON, empty, non-record JSON, bare scalar.
        let inputs = [
            "not json at all",
            "{",
            "",
            "   ",
            "[1,2,3]",
            "42",
            r#"{"type":"session_meta"}"#, // missing payload
            r#"{"payload":{"id":"x"}}"#,  // missing type
            r#"{"type":"response_item","payload":{}}"#, // item with no type
            r#"{"type":"response_item","payload":{"type":"function_call","name":"apply_patch","arguments":"not-json"}}"#,
        ];
        let adapter = CodexAdapter;
        let mut ctx = ParseCtx::new();
        for s in inputs {
            // Must not panic; result is fine either way.
            let _ = adapter.parse(&raw(s), &mut ctx);
        }
    }

    #[test]
    fn repeated_record_is_deduped() {
        // Same session_meta twice → only one SessionStart.
        let events = parse_all(&[META, META]);
        assert_eq!(tags(&events), vec!["session_start"]);

        // Same function_call (same call_id) twice → only one ToolCall.
        let call = r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"dup-1"}}"#;
        let events = parse_all(&[META, call, call]);
        assert_eq!(tags(&events), vec!["session_start", "tool_call"]);
    }

    #[test]
    fn malformed_patch_is_panic_free_and_emits_only_tool_call() {
        // apply_patch whose arguments are not valid JSON: no FileEdit, just the
        // ToolCall, and the args are preserved as a string.
        let call = r#"{"type":"response_item","payload":{"type":"function_call","name":"apply_patch","arguments":"*** Begin Patch (truncated","call_id":"cx"}}"#;
        let events = parse_all(&[META, call]);
        assert_eq!(tags(&events), vec!["session_start", "tool_call"]);
        match &events[1].kind {
            EventKind::ToolCall { args, .. } => {
                assert!(args.is_string(), "non-JSON arguments preserved as string");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn schema_fingerprint_recognizes_codex_records() {
        let adapter = CodexAdapter;
        let fp = adapter.schema_fingerprint(&raw(META));
        assert_eq!(fp.source, SourceKind::Codex);
        assert_eq!(fp.confidence, 100);
        assert_eq!(fp.variant, "codex/rollout-v2");

        let fp2 = adapter.schema_fingerprint(&raw("not a codex record"));
        assert_eq!(fp2.confidence, 0);
    }

    /// Load and parse a fixture from `fixtures/codex/v2/<name>` (workspace root).
    fn parse_fixture(name: &str) -> Vec<CaptureEvent> {
        // CARGO_MANIFEST_DIR is .../crates/memscribe-adapters; fixtures live two
        // levels up at the workspace root.
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("fixtures")
            .join("codex")
            .join("v2")
            .join(name);
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
        let adapter = CodexAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for (i, line) in body.lines().enumerate() {
            let r = RawRecord::from_line(
                line,
                SourceLocation::new(path.clone(), i as u64, (i + 1) as u64),
            );
            out.extend(adapter.parse(&r, &mut ctx).expect("never errors"));
        }
        out
    }

    #[test]
    fn fixture_happy_path_decision_then_edits() {
        let events = parse_fixture("happy_path_decision_then_edits.jsonl");
        let t = tags(&events);
        // session_start, user decision, assistant, tool_call(apply_patch),
        // two file_edits, tool_result, assistant. (reasoning/event_msg → unknown)
        assert_eq!(t.iter().filter(|x| **x == "session_start").count(), 1);
        assert_eq!(t.iter().filter(|x| **x == "user_turn").count(), 1);
        assert_eq!(t.iter().filter(|x| **x == "file_edit").count(), 2, "{t:?}");
        // The decision precedes both edits.
        let user_idx = t.iter().position(|x| *x == "user_turn").unwrap();
        let first_edit = t.iter().position(|x| *x == "file_edit").unwrap();
        assert!(user_idx < first_edit, "decision must precede edits: {t:?}");
        // The successful edit's tool result is ok.
        let res = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ToolResult { ok, call_id, .. } if call_id == "call_apply_patch_001" => {
                    Some(*ok)
                }
                _ => None,
            })
            .expect("tool result present");
        assert!(res, "successful patch result must be ok");
        // Edits carry the originating call_id so the segmenter can pair them.
        for e in &events {
            if let EventKind::FileEdit { call_id, .. } = &e.kind {
                assert_eq!(call_id.as_deref(), Some("call_apply_patch_001"));
            }
        }
    }

    #[test]
    fn fixture_rejected_alternative_has_decision_no_edits() {
        let events = parse_fixture("rejected_alternative.jsonl");
        let t = tags(&events);
        assert_eq!(t.iter().filter(|x| **x == "user_turn").count(), 1);
        assert_eq!(t.iter().filter(|x| **x == "file_edit").count(), 0, "{t:?}");
    }

    #[test]
    fn fixture_ban_has_decision_no_edits() {
        let events = parse_fixture("ban.jsonl");
        let t = tags(&events);
        assert_eq!(t.iter().filter(|x| **x == "user_turn").count(), 1);
        assert_eq!(t.iter().filter(|x| **x == "file_edit").count(), 0, "{t:?}");
        // The ban text is preserved verbatim on the user turn.
        let txt = events.iter().find_map(|e| match &e.kind {
            EventKind::UserTurn { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert!(txt.unwrap().contains("never add a dependency"));
    }

    #[test]
    fn fixture_tool_failure_marks_edit_result_not_ok() {
        let events = parse_fixture("tool_failure.jsonl");
        // The edit IS emitted (losslessly) ...
        let edit = events.iter().find_map(|e| match &e.kind {
            EventKind::FileEdit { call_id, .. } => call_id.clone(),
            _ => None,
        });
        assert_eq!(edit.as_deref(), Some("call_apply_patch_fail_001"));
        // ... but its paired tool result is NOT ok, so the segmenter will drop
        // the Episode (verified there; here we lock the not-ok signal).
        let ok = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ToolResult { ok, call_id, .. }
                    if call_id == "call_apply_patch_fail_001" =>
                {
                    Some(*ok)
                }
                _ => None,
            })
            .expect("tool result present");
        assert!(!ok, "failed patch result must be not-ok");
    }

    #[test]
    fn discover_finds_rollouts_and_skips_history() {
        // Build a fake $CODEX_HOME tree under a temp dir.
        let base = std::env::temp_dir().join(format!("codex-disc-{}", std::process::id()));
        let day = base.join("sessions").join("2026").join("06").join("22");
        std::fs::create_dir_all(&day).expect("mkdir");
        std::fs::write(day.join("rollout-2026-06-22T10-00-00-sess.jsonl"), b"{}").unwrap();
        std::fs::write(day.join("rollout-cold.jsonl.zst"), b"{}").unwrap();
        std::fs::write(base.join("sessions").join("history.jsonl"), b"{}").unwrap();
        std::fs::write(day.join("notes.txt"), b"x").unwrap();

        let mut overrides = std::collections::HashMap::new();
        overrides.insert("CODEX_HOME".to_string(), base.clone());
        let cfg = DiscoverCfg {
            overrides,
            ..Default::default()
        };
        let handles = discover_rollouts(&cfg);

        // Two rollouts found; history.jsonl and notes.txt excluded.
        assert_eq!(handles.len(), 2, "handles: {handles:?}");
        assert!(handles.iter().all(|h| h.source == SourceKind::Codex));
        assert!(handles.iter().any(|h| h.compressed));
        assert!(handles.iter().all(|h| h
            .path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("rollout-")));

        std::fs::remove_dir_all(&base).ok();
    }
}
