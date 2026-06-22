//! OpenTelemetry GenAI adapter.
//!
//! Source: OTLP push (local collector / file), OTel GenAI semconv records as
//! JSON / NDJSON — the universal fallback channel for any instrumented agent.
//! Each line is a log record or span. Attributes come in two shapes and this
//! adapter accepts both:
//!
//! - **flat**: `{ "gen_ai.input.messages": [...], "gen_ai.system": "...", ... }`
//! - **OTLP nested**: `{ "attributes": [ { "key": "...",
//!   "value": { "stringValue" | "intValue" | "arrayValue" } }, ... ] }`
//!
//! Mapping (whitepaper §5):
//! - `gen_ai.input.messages` (role `user`) and `gen_ai.cli.user_prompt`
//!   → [`EventKind::UserTurn`]
//! - `gen_ai.output.messages` → [`EventKind::AssistantTurn`] (with model + usage)
//! - `execute_tool` span → [`EventKind::ToolCall`] (+ [`EventKind::ToolResult`]
//!   when result attributes are present)
//! - `file_operation` span (and `execute_tool` edits) → [`EventKind::FileEdit`]
//!   with `file.path`, `model_added_lines`/`code.added_lines`,
//!   `model_removed_lines`/`code.removed_lines`
//! - `gen_ai.conversation.id` → session id; record `time` → timestamp.
//!
//! Anything well-formed but unrecognized is routed to [`EventKind::Unknown`] so
//! the stream stays lossless. The parser never panics.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, Part,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, Timestamp, TranscriptAdapter,
    TranscriptHandle, Usage,
};
use serde_json::Value;
use std::path::PathBuf;

/// Adapter for OpenTelemetry GenAI records.
#[derive(Debug, Default, Clone, Copy)]
pub struct OtelAdapter;

impl TranscriptAdapter for OtelAdapter {
    fn source_kind(&self) -> SourceKind {
        SourceKind::Otel
    }

    fn discover(&self, _cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        // OTel records are pushed (collector / file tail) rather than discovered
        // in a well-known per-tool directory, so there is nothing to glob.
        Vec::new()
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        let Some(value) = util::parse_json_line(raw) else {
            // Blank line → nothing; non-JSON → lossless Unknown of the raw text.
            let s = raw.as_str().map(str::trim).unwrap_or("");
            if s.is_empty() {
                return Ok(Vec::new());
            }
            return Ok(vec![util::unknown_event(
                SourceKind::Otel,
                ctx,
                raw,
                Value::String(s.to_string()),
            )]);
        };

        // Normalize both shapes into a flat attribute view.
        let attrs = Attrs::from_record(&value);

        // Learn the session id and project binding as early as we can.
        if ctx.session_id.is_none() {
            if let Some(sid) = attrs.str("gen_ai.conversation.id") {
                ctx.session_id = Some(sid.to_string());
            }
        }

        let ts = attrs.timestamp();
        let op = attrs.operation_name();

        let events = match op.as_deref() {
            Some("session.start") | Some("session_start") | Some("gen_ai.session.start") => {
                vec![self.session_start(ctx, raw, &attrs, ts)]
            }
            Some("session.end") | Some("session_end") | Some("gen_ai.session.end") => {
                vec![mk(
                    ctx,
                    raw,
                    derive_id(raw, "session_end", 0),
                    ts,
                    EventKind::SessionEnd {
                        reason: attrs.str("reason").map(str::to_string),
                    },
                )]
            }
            Some("execute_tool") | Some("gen_ai.execute_tool") => {
                self.execute_tool(ctx, raw, &attrs, ts)
            }
            Some("file_operation") | Some("gen_ai.file_operation") => {
                self.file_operation(ctx, raw, &attrs, ts)
            }
            // Chat / inference records carry the dialogue.
            _ => self.dialogue(ctx, raw, &attrs, ts, &value),
        };

        // Dedup / idempotency: drop any event whose id we have already emitted.
        Ok(self.dedup(ctx, events))
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        let Some(value) = util::parse_json_line(sample) else {
            return SchemaVariant::unknown(SourceKind::Otel);
        };
        // The OTLP nested shape carries an `attributes` array of {key,value}.
        if value
            .get("attributes")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|e| e.get("key").is_some()))
        {
            return SchemaVariant::certain(SourceKind::Otel, "otel/genai-otlp");
        }
        // The flat shape uses dotted `gen_ai.*` keys directly on the object.
        if value
            .as_object()
            .is_some_and(|m| m.keys().any(|k| k.starts_with("gen_ai.")))
        {
            return SchemaVariant::certain(SourceKind::Otel, "otel/genai-flat");
        }
        SchemaVariant::unknown(SourceKind::Otel)
    }
}

impl OtelAdapter {
    fn session_start(
        &self,
        ctx: &mut ParseCtx,
        raw: &RawRecord,
        attrs: &Attrs,
        ts: Timestamp,
    ) -> CaptureEvent {
        let cwd: PathBuf = attrs
            .str("cwd")
            .or_else(|| attrs.str("gen_ai.cli.cwd"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let git = attrs.str("git.commit").map(|sha| GitRef {
            sha: sha.to_string(),
            branch: attrs.str("git.branch").map(str::to_string),
        });
        // Stamp the project binding for the rest of the session.
        if ctx.project.is_none() {
            ctx.project = Some(ProjectRef {
                cwd: cwd.clone(),
                repo_root: attrs.str("repo_root").map(PathBuf::from),
                git: git.clone(),
            });
        }
        mk(
            ctx,
            raw,
            derive_id(raw, "session_start", 0),
            ts,
            EventKind::SessionStart {
                cwd,
                git,
                model: attrs.str("gen_ai.request.model").map(str::to_string),
                tool_version: attrs
                    .str("gen_ai.tool.version")
                    .or_else(|| attrs.str("service.version"))
                    .map(str::to_string),
            },
        )
    }

    /// `execute_tool` span → a ToolCall, plus a ToolResult when the span carries
    /// result/error attributes, plus a FileEdit when it carries `file.path`.
    fn execute_tool(
        &self,
        ctx: &mut ParseCtx,
        raw: &RawRecord,
        attrs: &Attrs,
        ts: Timestamp,
    ) -> Vec<CaptureEvent> {
        let mut out = Vec::new();
        let name = attrs.str("gen_ai.tool.name").unwrap_or("tool").to_string();
        let call_id = attrs
            .str("gen_ai.tool.call.id")
            .map(str::to_string)
            .unwrap_or_else(|| content_id(&raw.bytes));
        let args = attrs
            .value("gen_ai.tool.call.arguments")
            .cloned()
            .unwrap_or(Value::Null);

        ctx.call_names.insert(call_id.clone(), name.clone());
        out.push(mk(
            ctx,
            raw,
            derive_id(raw, "tool_call", 0),
            ts,
            EventKind::ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                args,
            },
        ));

        // A result is present iff the span reports a status/result/error.
        let ok = attrs.tool_ok();
        if let Some(ok) = ok {
            ctx.call_ok.insert(call_id.clone(), ok);
            let output = attrs
                .value("gen_ai.tool.result")
                .cloned()
                .unwrap_or(Value::Null);
            out.push(mk(
                ctx,
                raw,
                derive_id(raw, "tool_result", 0),
                ts,
                EventKind::ToolResult {
                    call_id: call_id.clone(),
                    ok,
                    output,
                },
            ));
        }

        // An edit-shaped tool span also yields a FileEdit (keyed to the call).
        if let Some(diff) = attrs.file_diff() {
            out.push(mk(
                ctx,
                raw,
                derive_id(raw, "file_edit", 0),
                ts,
                EventKind::FileEdit {
                    call_id: Some(call_id),
                    diff,
                },
            ));
        }
        out
    }

    /// `file_operation` span → a FileEdit (no call id).
    fn file_operation(
        &self,
        ctx: &mut ParseCtx,
        raw: &RawRecord,
        attrs: &Attrs,
        ts: Timestamp,
    ) -> Vec<CaptureEvent> {
        match attrs.file_diff() {
            Some(diff) => vec![mk(
                ctx,
                raw,
                derive_id(raw, "file_edit", 0),
                ts,
                EventKind::FileEdit {
                    call_id: None,
                    diff,
                },
            )],
            // A file_operation without a path is unrecognized → lossless Unknown.
            None => vec![util::unknown_event(
                SourceKind::Otel,
                ctx,
                raw,
                attrs.raw().clone(),
            )],
        }
    }

    /// A chat / inference record: zero or more UserTurns from input messages and
    /// the CLI prompt, then zero or more AssistantTurns from output messages.
    fn dialogue(
        &self,
        ctx: &mut ParseCtx,
        raw: &RawRecord,
        attrs: &Attrs,
        ts: Timestamp,
        value: &Value,
    ) -> Vec<CaptureEvent> {
        let mut out = Vec::new();

        // `gen_ai.cli.user_prompt` → one UserTurn.
        if let Some(prompt) = attrs.str("gen_ai.cli.user_prompt") {
            if !prompt.is_empty() {
                out.push(mk(
                    ctx,
                    raw,
                    derive_id(raw, "user_prompt", 0),
                    ts,
                    EventKind::UserTurn {
                        text: prompt.to_string(),
                        parts: vec![Part::Text {
                            text: prompt.to_string(),
                        }],
                    },
                ));
            }
        }

        // `gen_ai.input.messages` → one UserTurn per user-role message.
        if let Some(msgs) = attrs.array("gen_ai.input.messages") {
            for (i, m) in msgs.iter().enumerate() {
                if !is_user_role(m) {
                    continue;
                }
                let text = message_text(m);
                out.push(mk(
                    ctx,
                    raw,
                    derive_id(raw, "input_msg", i),
                    ts,
                    EventKind::UserTurn {
                        text: text.clone(),
                        parts: vec![Part::Text { text }],
                    },
                ));
            }
        }

        // `gen_ai.output.messages` → one AssistantTurn per message.
        if let Some(msgs) = attrs.array("gen_ai.output.messages") {
            let model = attrs.str("gen_ai.request.model").map(str::to_string);
            let usage = attrs.usage();
            for (i, m) in msgs.iter().enumerate() {
                let text = message_text(m);
                out.push(mk(
                    ctx,
                    raw,
                    derive_id(raw, "output_msg", i),
                    ts,
                    EventKind::AssistantTurn {
                        text: text.clone(),
                        thinking: None,
                        model: model.clone(),
                        usage: usage.clone(),
                        parts: vec![Part::Text { text }],
                    },
                ));
            }
        }

        // A record with none of the recognized dialogue fields is preserved
        // verbatim so the stream stays lossless.
        if out.is_empty() {
            out.push(util::unknown_event(
                SourceKind::Otel,
                ctx,
                raw,
                value.clone(),
            ));
        }
        out
    }

    /// Drop events whose ids have already been emitted (dedup / idempotency).
    fn dedup(&self, ctx: &mut ParseCtx, events: Vec<CaptureEvent>) -> Vec<CaptureEvent> {
        events
            .into_iter()
            .filter(|e| ctx.first_seen(&e.event_id))
            .collect()
    }
}

/// Build a normalized event with the OTel source.
fn mk(
    ctx: &mut ParseCtx,
    raw: &RawRecord,
    event_id: String,
    ts: Timestamp,
    kind: EventKind,
) -> CaptureEvent {
    util::mk_event(SourceKind::Otel, ctx, raw, event_id, None, ts, kind)
}

/// A deterministic per-logical-event id: the record's content hash plus a stable
/// `kind`/index suffix so multiple events from one record don't collide and a
/// repeated record dedups to the same ids.
fn derive_id(raw: &RawRecord, kind: &str, index: usize) -> String {
    format!("{}:{kind}:{index}", content_id(&raw.bytes))
}

/// Is a `gen_ai.*.messages` entry a user-role message?
fn is_user_role(m: &Value) -> bool {
    m.get("role")
        .and_then(Value::as_str)
        .map(|r| r.eq_ignore_ascii_case("user"))
        .unwrap_or(false)
}

/// Flatten a GenAI message's text from `content` (string or parts array) or
/// `parts` (array of `{type,text}` / `{content}` / strings).
fn message_text(m: &Value) -> String {
    if let Some(s) = m.get("content").and_then(Value::as_str) {
        return s.to_string();
    }
    let mut buf = String::new();
    for key in ["parts", "content"] {
        if let Some(arr) = m.get(key).and_then(Value::as_array) {
            for p in arr {
                if let Some(s) = p.as_str() {
                    push_part(&mut buf, s);
                } else if let Some(s) = p
                    .get("text")
                    .or_else(|| p.get("content"))
                    .and_then(Value::as_str)
                {
                    push_part(&mut buf, s);
                }
            }
        }
    }
    buf
}

fn push_part(buf: &mut String, s: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(s);
}

/// A flat view over a GenAI record's attributes, hiding the flat-vs-OTLP shape.
struct Attrs<'a> {
    /// The flat record object, when the record is already flat.
    flat: Option<&'a serde_json::Map<String, Value>>,
    /// Materialized {key → value} from the OTLP `attributes` array, when nested.
    nested: Option<std::collections::HashMap<String, Value>>,
    /// The original record (for lossless Unknown fallbacks).
    raw: &'a Value,
}

impl<'a> Attrs<'a> {
    fn from_record(value: &'a Value) -> Self {
        // OTLP nested: an `attributes: [{key, value:{...}}]` array.
        if let Some(arr) = value.get("attributes").and_then(Value::as_array) {
            let mut map = std::collections::HashMap::new();
            for entry in arr {
                if let Some(key) = entry.get("key").and_then(Value::as_str) {
                    if let Some(v) = entry.get("value").map(otlp_value) {
                        map.insert(key.to_string(), v);
                    }
                }
            }
            return Attrs {
                flat: None,
                nested: Some(map),
                raw: value,
            };
        }
        Attrs {
            flat: value.as_object(),
            nested: None,
            raw: value,
        }
    }

    fn raw(&self) -> &'a Value {
        self.raw
    }

    fn value(&self, key: &str) -> Option<&Value> {
        if let Some(m) = self.flat {
            return m.get(key);
        }
        self.nested.as_ref().and_then(|m| m.get(key))
    }

    fn str(&self, key: &str) -> Option<&str> {
        self.value(key).and_then(Value::as_str)
    }

    fn array(&self, key: &str) -> Option<&Vec<Value>> {
        self.value(key).and_then(Value::as_array)
    }

    fn u64(&self, key: &str) -> Option<u64> {
        let v = self.value(key)?;
        if let Some(n) = v.as_u64() {
            return Some(n);
        }
        // OTLP intValue is often a stringified integer.
        v.as_str().and_then(|s| s.trim().parse::<u64>().ok())
    }

    fn u32(&self, key: &str) -> Option<u32> {
        self.u64(key).and_then(|n| u32::try_from(n).ok())
    }

    fn operation_name(&self) -> Option<String> {
        self.str("gen_ai.operation.name")
            .or_else(|| self.str("operation.name"))
            .or_else(|| self.str("name"))
            .map(str::to_string)
    }

    fn timestamp(&self) -> Timestamp {
        // Prefer record-level time fields (which may live outside `attributes`).
        util::ts_from(
            self.raw,
            &[
                "time",
                "timestamp",
                "timeUnixNano",
                "observedTimeUnixNano",
                "ts",
            ],
        )
    }

    fn usage(&self) -> Option<Usage> {
        let input = self.u64("gen_ai.usage.input_tokens");
        let output = self.u64("gen_ai.usage.output_tokens");
        if input.is_none() && output.is_none() {
            return None;
        }
        Some(Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: self.u64("gen_ai.usage.cache_read_tokens"),
            cache_creation_tokens: self.u64("gen_ai.usage.cache_creation_tokens"),
        })
    }

    /// The success flag of a tool span, if any result/error attribute is present.
    /// `None` means "no result observed on this span".
    fn tool_ok(&self) -> Option<bool> {
        if let Some(status) = self
            .str("gen_ai.tool.result.status")
            .or_else(|| self.str("otel.status_code"))
            .or_else(|| self.str("status"))
        {
            let s = status.trim().to_ascii_lowercase();
            return Some(!matches!(s.as_str(), "error" | "failed" | "failure" | "ko"));
        }
        if self.value("error.type").is_some() || self.value("exception.type").is_some() {
            return Some(false);
        }
        if self.value("gen_ai.tool.result").is_some() {
            return Some(true);
        }
        None
    }

    /// A normalized diff from a file-edit span, if it carries a `file.path`.
    fn file_diff(&self) -> Option<Diff> {
        let path = self
            .str("file.path")
            .or_else(|| self.str("code.filepath"))?;
        let added = self
            .u32("model_added_lines")
            .or_else(|| self.u32("code.added_lines"))
            .unwrap_or(0);
        let removed = self
            .u32("model_removed_lines")
            .or_else(|| self.u32("code.removed_lines"))
            .unwrap_or(0);
        Some(Diff {
            path: PathBuf::from(path),
            old: None,
            new: None,
            unified: None,
            added_lines: added,
            removed_lines: removed,
        })
    }
}

/// Collapse an OTLP `value` object (`stringValue` / `intValue` / `boolValue` /
/// `doubleValue` / `arrayValue` / `kvlistValue`) to a plain JSON value.
fn otlp_value(v: &Value) -> Value {
    if let Some(s) = v.get("stringValue") {
        return s.clone();
    }
    if let Some(i) = v.get("intValue") {
        return i.clone();
    }
    if let Some(b) = v.get("boolValue") {
        return b.clone();
    }
    if let Some(d) = v.get("doubleValue") {
        return d.clone();
    }
    if let Some(arr) = v.get("arrayValue").and_then(|a| a.get("values")) {
        if let Some(items) = arr.as_array() {
            return Value::Array(items.iter().map(otlp_value).collect());
        }
    }
    if let Some(kv) = v.get("kvlistValue").and_then(|k| k.get("values")) {
        if let Some(items) = kv.as_array() {
            let mut map = serde_json::Map::new();
            for item in items {
                if let Some(key) = item.get("key").and_then(Value::as_str) {
                    if let Some(val) = item.get("value").map(otlp_value) {
                        map.insert(key.to_string(), val);
                    }
                }
            }
            return Value::Object(map);
        }
    }
    // Already a plain scalar/array, or an unrecognized shape: pass through.
    v.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(s: &str) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("otel.jsonl", 0, 1))
    }

    fn parse_all(lines: &[&str]) -> (Vec<CaptureEvent>, ParseCtx) {
        let adapter = OtelAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for l in lines {
            let evs = adapter.parse(&raw(l), &mut ctx).expect("never errors");
            out.extend(evs);
        }
        (out, ctx)
    }

    fn tags(evs: &[CaptureEvent]) -> Vec<&'static str> {
        evs.iter().map(|e| e.kind.tag()).collect()
    }

    const SESSION_START: &str = r#"{"time":"2026-06-22T10:00:00Z","gen_ai.operation.name":"session.start","gen_ai.conversation.id":"sess-1","gen_ai.request.model":"claude-opus-4-8","cwd":"/home/dev/svc","repo_root":"/home/dev/svc","git.commit":"abc1234","git.branch":"main"}"#;
    const USER_PROMPT: &str = r#"{"time":"2026-06-22T10:00:05Z","gen_ai.operation.name":"chat","gen_ai.conversation.id":"sess-1","gen_ai.cli.user_prompt":"Let's use Postgres instead of MySQL."}"#;
    const FILE_EDIT: &str = r#"{"time":"2026-06-22T10:00:15Z","gen_ai.operation.name":"file_operation","gen_ai.conversation.id":"sess-1","file.path":"db/config.rs","code.added_lines":12,"code.removed_lines":4}"#;

    #[test]
    fn normalized_sequence_for_a_small_session() {
        let assistant = r#"{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"sess-1","gen_ai.request.model":"claude-opus-4-8","gen_ai.usage.input_tokens":42,"gen_ai.usage.output_tokens":8,"gen_ai.output.messages":[{"role":"assistant","content":"Switching to Postgres."}]}"#;
        let session_end = r#"{"gen_ai.operation.name":"session.end","gen_ai.conversation.id":"sess-1","reason":"done"}"#;
        let (evs, ctx) = parse_all(&[
            SESSION_START,
            USER_PROMPT,
            assistant,
            FILE_EDIT,
            session_end,
        ]);
        assert_eq!(
            tags(&evs),
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "file_edit",
                "session_end"
            ]
        );
        // Session id is learned from gen_ai.conversation.id and stamped.
        assert_eq!(ctx.session_id.as_deref(), Some("sess-1"));
        assert!(evs.iter().all(|e| e.session_id == "sess-1"));
        // Project binding came from the session-start record.
        let proj = ctx.project.expect("project set at session start");
        assert_eq!(proj.cwd, PathBuf::from("/home/dev/svc"));
        assert_eq!(proj.git.as_ref().map(|g| g.sha.as_str()), Some("abc1234"));
    }

    #[test]
    fn decision_then_edit_produces_user_turn_then_file_edit() {
        let (evs, _) = parse_all(&[USER_PROMPT, FILE_EDIT]);
        assert_eq!(tags(&evs), vec!["user_turn", "file_edit"]);
        match &evs[0].kind {
            EventKind::UserTurn { text, .. } => {
                assert!(text.contains("Postgres"));
            }
            other => panic!("expected user_turn, got {other:?}"),
        }
        match &evs[1].kind {
            EventKind::FileEdit { diff, call_id } => {
                assert_eq!(diff.path, PathBuf::from("db/config.rs"));
                assert_eq!(diff.added_lines, 12);
                assert_eq!(diff.removed_lines, 4);
                assert!(call_id.is_none());
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }

    #[test]
    fn input_messages_become_user_turns() {
        let line = r#"{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"s","gen_ai.input.messages":[{"role":"user","content":"hello"},{"role":"system","content":"ignore"},{"role":"user","parts":[{"type":"text","text":"world"}]}]}"#;
        let (evs, _) = parse_all(&[line]);
        // Only the two user-role messages map to UserTurns.
        assert_eq!(tags(&evs), vec!["user_turn", "user_turn"]);
        match &evs[1].kind {
            EventKind::UserTurn { text, .. } => assert_eq!(text, "world"),
            other => panic!("expected user_turn, got {other:?}"),
        }
    }

    #[test]
    fn execute_tool_emits_call_result_and_edit() {
        let line = r#"{"gen_ai.operation.name":"execute_tool","gen_ai.conversation.id":"s","gen_ai.tool.name":"edit_file","gen_ai.tool.call.id":"c1","file.path":"a.rs","model_added_lines":3,"model_removed_lines":1,"gen_ai.tool.result":"ok"}"#;
        let (evs, _) = parse_all(&[line]);
        assert_eq!(tags(&evs), vec!["tool_call", "tool_result", "file_edit"]);
        match &evs[1].kind {
            EventKind::ToolResult { ok, call_id, .. } => {
                assert!(*ok);
                assert_eq!(call_id, "c1");
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn failed_tool_result_is_marked_not_ok() {
        // tool_failure scenario: the ToolResult must be ok=false so downstream
        // produces no spurious Episode.
        let line = r#"{"gen_ai.operation.name":"execute_tool","gen_ai.conversation.id":"s","gen_ai.tool.name":"edit_file","gen_ai.tool.call.id":"cf","file.path":"a.rs","model_added_lines":3,"model_removed_lines":1,"gen_ai.tool.result.status":"error","error.type":"PatchConflict","gen_ai.tool.result":"hunk failed"}"#;
        let (evs, _) = parse_all(&[line]);
        assert_eq!(tags(&evs), vec!["tool_call", "tool_result", "file_edit"]);
        match &evs[1].kind {
            EventKind::ToolResult { ok, .. } => assert!(!*ok, "failed tool must be ok=false"),
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn otlp_nested_shape_is_supported() {
        // The same edit, expressed in the OTLP attributes-array shape.
        let line = r#"{"timeUnixNano":"1750586400000000000","attributes":[{"key":"gen_ai.operation.name","value":{"stringValue":"file_operation"}},{"key":"gen_ai.conversation.id","value":{"stringValue":"nested-1"}},{"key":"file.path","value":{"stringValue":"src/main.rs"}},{"key":"code.added_lines","value":{"intValue":"7"}},{"key":"code.removed_lines","value":{"intValue":"2"}}]}"#;
        let (evs, ctx) = parse_all(&[line]);
        assert_eq!(tags(&evs), vec!["file_edit"]);
        assert_eq!(ctx.session_id.as_deref(), Some("nested-1"));
        match &evs[0].kind {
            EventKind::FileEdit { diff, .. } => {
                assert_eq!(diff.path, PathBuf::from("src/main.rs"));
                assert_eq!(diff.added_lines, 7);
                assert_eq!(diff.removed_lines, 2);
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }

    #[test]
    fn unrecognized_record_routes_to_unknown() {
        let line = r#"{"gen_ai.operation.name":"telemetry.heartbeat","gen_ai.conversation.id":"s","foo":"bar"}"#;
        let (evs, _) = parse_all(&[line]);
        assert_eq!(tags(&evs), vec!["unknown"]);
    }

    #[test]
    fn garbage_input_never_panics_and_stays_lossless() {
        let adapter = OtelAdapter;
        let mut ctx = ParseCtx::new();
        // Non-JSON garbage → one lossless Unknown.
        let evs = adapter
            .parse(&raw("}{ not json at all <<<"), &mut ctx)
            .expect("never errors");
        assert_eq!(tags(&evs), vec!["unknown"]);
        // Blank line → nothing.
        let evs = adapter.parse(&raw("   "), &mut ctx).expect("never errors");
        assert!(evs.is_empty());
        // Truncated / weird JSON values must not panic.
        for g in [
            "null",
            "[]",
            "123",
            "\"a string\"",
            r#"{"attributes":"not-an-array"}"#,
            r#"{"gen_ai.input.messages":42}"#,
            r#"{"gen_ai.output.messages":[{}]}"#,
        ] {
            let _ = adapter.parse(&raw(g), &mut ctx).expect("never errors");
        }
    }

    #[test]
    fn repeated_record_dedups_to_empty() {
        let adapter = OtelAdapter;
        let mut ctx = ParseCtx::new();
        let first = adapter.parse(&raw(FILE_EDIT), &mut ctx).expect("ok");
        assert_eq!(tags(&first), vec!["file_edit"]);
        // The very same record again → idempotent, emits nothing.
        let second = adapter.parse(&raw(FILE_EDIT), &mut ctx).expect("ok");
        assert!(second.is_empty(), "repeat must dedup to empty");
    }

    #[test]
    fn ban_prompt_is_carried_as_user_turn() {
        let line = r#"{"gen_ai.operation.name":"chat","gen_ai.conversation.id":"s","gen_ai.cli.user_prompt":"We will never add a dependency on left-pad."}"#;
        let (evs, _) = parse_all(&[line]);
        assert_eq!(tags(&evs), vec!["user_turn"]);
        match &evs[0].kind {
            EventKind::UserTurn { text, .. } => assert!(text.contains("never add a dependency")),
            other => panic!("expected user_turn, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_distinguishes_flat_and_otlp() {
        let adapter = OtelAdapter;
        let flat = adapter.schema_fingerprint(&raw(USER_PROMPT));
        assert_eq!(flat.variant, "otel/genai-flat");
        assert_eq!(flat.confidence, 100);
        let nested = raw(r#"{"attributes":[{"key":"gen_ai.system","value":{"stringValue":"x"}}]}"#);
        let otlp = adapter.schema_fingerprint(&nested);
        assert_eq!(otlp.variant, "otel/genai-otlp");
    }
}
