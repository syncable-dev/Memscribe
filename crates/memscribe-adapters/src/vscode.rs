//! VS Code adapter (Copilot Chat / chat sessions).
//!
//! VS Code is **database-backed**: its chat does not live in `chatSessions/*.json`
//! files (current builds ship none) but in a SQLite store, `state.vscdb`, under
//! the user's `Code/User` directory. The chat sits in `ItemTable(key, value)`
//! under the key `interactive.sessions` — a JSON **array of sessions**. This
//! adapter therefore declares [`StoreReader::Native`] and reads that store itself
//! ([`read_native`](VsCodeAdapter::read_native)) **read-only**, normalizing every
//! request into the same stable `{role, text, …, edits, toolCalls}` JSON record
//! shape the pure [`parse`](VsCodeAdapter::parse) already understands.
//!
//! ## On-disk store (reverse-engineered, verified June 2026, read-only)
//!
//! `…/Code/User/workspaceStorage/<hash>/state.vscdb` (per-workspace) and
//! `…/Code/User/globalStorage/state.vscdb` (global). Each has one table,
//! `ItemTable(key TEXT, value BLOB)`. The chat lives under
//! `key = 'interactive.sessions'`, whose value is a JSON array of sessions:
//!
//! - session: `{version, requesterUsername, responderUsername, sessionId,
//!   creationDate, lastMessageDate, customTitle, requests:[…]}`.
//! - request: `{requestId, message:{text, parts}, response:[…], responseId,
//!   result:{metadata}, timestamp, …}`.
//!   - `message.text` (and `message.parts[].text`) is the **user** prompt.
//!   - `response` is an ordered list of **parts**, each a dict. The shapes we map:
//!     - a `markdownContent` part — **or a part with no `kind` but a `value`**
//!       (the live shape) — carries assistant text in `value` (or `content.value`).
//!     - `textEditGroup` carries file **edits**: a `uri` plus `edits`/`textEdits`.
//!     - `codeblockUri`/`inlineReference` is a file reference (`uri`).
//!     - `toolInvocationSerialized`/`prepareToolInvocation` is a **tool call**
//!       (`toolId`/`invocationMessage.value`).
//!
//! `read_native` emits, per session, the messages in request order: a `user`
//! record (from `message`), then an assistant record assembled by concatenating
//! the markdown parts (carrying any `edits[]`/`toolCalls[]` lifted from the
//! response parts).
//!
//! This adapter still parses two **legacy** shapes so existing fixtures keep
//! working, routed by [`parse`](VsCodeAdapter::parse) shape-detection:
//!
//! 1. A stable, **exported** chat JSON-lines shape (one record per line) — a
//!    leading `{kind:session_start, cwd, git, toolVersion}` followed by message
//!    records `{id, parentId, role, ts, sessionId, text, model, usage, toolCalls,
//!    toolResults, edits}`. (`read_native` produces this shape from the store.)
//! 2. The legacy **`chatSessions`** JSON shape, where a single object carries
//!    `{version, requesterUsername, responderUsername, requests:[{message,
//!    response}]}`; each request maps to a `UserTurn` and its response to an
//!    `AssistantTurn`.
//!
//! Anything unrecognized-but-valid routes to [`memscribe_core::EventKind::Unknown`]
//! via [`util::unknown_event`], so the stream stays lossless across VS Code
//! version churn. The parser is fully deterministic and never panics; `read_native`
//! opens SQLite read-only (`mode=ro&immutable=1`) and never writes.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, SourceLocation, StoreReader,
    TranscriptAdapter, TranscriptHandle, Usage,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const SOURCE: SourceKind = SourceKind::VsCode;

/// The `ItemTable` key under which VS Code stores the chat session array.
const SESSIONS_KEY: &str = "interactive.sessions";

/// Adapter for VS Code chat-session transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct VsCodeAdapter;

impl TranscriptAdapter for VsCodeAdapter {
    fn source_kind(&self) -> SourceKind {
        SOURCE
    }

    /// VS Code keeps its chat in a SQLite store, so the adapter reads the store
    /// itself via [`read_native`](VsCodeAdapter::read_native) rather than the
    /// line-delimited file reader.
    fn store_reader(&self) -> StoreReader {
        StoreReader::Native
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        // Glob the real product locations for `state.vscdb`:
        //   …/Code/User/workspaceStorage/<hash>/state.vscdb  (per-workspace)
        //   …/Code/User/globalStorage/state.vscdb            (global)
        // plus a tolerant `~/.vscode` fallback for non-standard installs.
        // 2026-07: added the Windows candidate — there was none at all, so
        // discover() returned zero handles on every real Windows VS Code
        // install (confirmed live: %APPDATA%\Code\User\workspaceStorage is
        // the real path, corroborated by multiple independent sources).
        let home = cfg.home_dir();
        let user_dirs = [
            home.join("Library/Application Support/Code/User"), // macOS
            home.join(".config/Code/User"),                     // Linux
            home.join("AppData/Roaming/Code/User"),             // Windows — %APPDATA%\Code\User
            home.join(".vscode"),
        ];

        let mut handles: Vec<TranscriptHandle> = Vec::new();
        for user in user_dirs {
            // Per-workspace stores under workspaceStorage/<hash>/state.vscdb.
            let ws_root = user.join("workspaceStorage");
            if let Ok(entries) = std::fs::read_dir(&ws_root) {
                let mut hashes: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                hashes.sort();
                for ws in hashes {
                    let db = ws.join("state.vscdb");
                    if db.is_file() {
                        let session_hint =
                            ws.file_name().and_then(|n| n.to_str()).map(str::to_string);
                        handles.push(TranscriptHandle {
                            path: db,
                            source: SOURCE,
                            session_hint,
                            compressed: false,
                        });
                    }
                }
            }
            // The global store.
            let global = user.join("globalStorage/state.vscdb");
            if global.is_file() {
                handles.push(TranscriptHandle {
                    path: global,
                    source: SOURCE,
                    session_hint: None,
                    compressed: false,
                });
            }
        }

        // Deterministic order; dedup identical paths (overlapping roots).
        handles.sort_by(|a, b| a.path.cmp(&b.path));
        handles.dedup_by(|a, b| a.path == b.path);
        handles
    }

    /// Open the VS Code `state.vscdb` at `handle.path` **read-only** and yield one
    /// [`RawRecord`] per logical message, in deterministic order: per session, a
    /// `user` record then an assembled `assistant` record per request. A
    /// non-`.vscdb` path (e.g. a legacy exported `.json`/`.jsonl`) falls back to
    /// reading the file's lines so the legacy shapes keep working.
    ///
    /// # Errors
    /// Returns [`ParseError::Io`] only if a `.vscdb` path cannot be opened, or a
    /// non-database path cannot be read. A readable store with no
    /// `interactive.sessions` key degrades to an empty record set, never an error.
    fn read_native(&self, handle: &TranscriptHandle) -> Result<Vec<RawRecord>, ParseError> {
        let path = &handle.path;
        let is_vscdb = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("vscdb"))
            .unwrap_or(false);
        if is_vscdb {
            read_vscdb(path)
        } else {
            read_lines(path)
        }
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
// Native SQLite store reader: state.vscdb → normalized RawRecords
// ---------------------------------------------------------------------------

/// Open a VS Code `state.vscdb` **read-only** and normalize the chat it holds
/// (under `ItemTable['interactive.sessions']`) into [`RawRecord`]s. Deterministic:
/// sessions are emitted in their stored array order and requests in request
/// order. Errors only if the file cannot be opened at all; a missing key/table or
/// a non-array value degrades to an empty stream (the lossless outcome for an
/// empty store).
fn read_vscdb(path: &Path) -> Result<Vec<RawRecord>, ParseError> {
    // `mode=ro&immutable=1` opens read-only and promises we won't observe
    // concurrent writes — VS Code may have the live DB open, but we never write,
    // lock, or create side files (`-wal`/`-shm`).
    let uri = format!("file:{}?mode=ro&immutable=1", path.to_string_lossy());
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| ParseError::Io(format!("opening vscode store {}: {e}", path.display())))?;

    // The value is stored as TEXT in current builds (older/global stores may use
    // BLOB), and rusqlite will not coerce a TEXT column into `Vec<u8>`. Read it as
    // a `ValueRef` so we accept either storage class. A missing key/table is not
    // an error — it just means there is no chat to read.
    let bytes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            [SESSIONS_KEY],
            |row| {
                Ok(match row.get_ref(0)? {
                    rusqlite::types::ValueRef::Text(t) => t.to_vec(),
                    rusqlite::types::ValueRef::Blob(b) => b.to_vec(),
                    _ => Vec::new(),
                })
            },
        )
        .ok();
    let Some(bytes) = bytes else {
        return Ok(Vec::new());
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(Vec::new());
    };
    let Some(sessions) = value.as_array() else {
        return Ok(Vec::new());
    };

    let mut records = Vec::new();
    let mut line_no: u64 = 0;
    for session in sessions {
        expand_session(session, path, &mut line_no, &mut records);
    }
    Ok(records)
}

/// Expand one VS Code session object into normalized `{role, text, …}` records:
/// for each request, a `user` record then an assembled `assistant` record.
fn expand_session(session: &Value, file: &Path, line_no: &mut u64, out: &mut Vec<RawRecord>) {
    let Some(obj) = session.as_object() else {
        // A non-object session entry is preserved losslessly as one record.
        push_record(out, file, line_no, session);
        return;
    };

    let session_id = obj
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        // Fall back to a stable content-derived id when the store omits one.
        .unwrap_or_else(|| {
            let bytes = serde_json::to_vec(session).unwrap_or_default();
            let cid = content_id(&bytes);
            format!("vscode-{}", &cid[..cid.len().min(16)])
        });
    // Session-level timestamp fallback for requests that carry none.
    let session_ts = obj
        .get("creationDate")
        .or_else(|| obj.get("lastMessageDate"))
        .and_then(value_as_i64);
    let responder = obj
        .get("responderUsername")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let Some(requests) = obj.get("requests").and_then(Value::as_array) else {
        // A session with no requests array is still preserved losslessly.
        push_record(out, file, line_no, session);
        return;
    };

    for (ri, req) in requests.iter().enumerate() {
        let Some(rq) = req.as_object() else { continue };

        // Per-request timestamp (epoch-millis), else the session's.
        let ts_ms = rq.get("timestamp").and_then(value_as_i64).or(session_ts);
        let request_id = rq
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{session_id}:req:{ri}"));
        let response_id = rq
            .get("responseId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{request_id}:resp"));

        // 1) The user message → a `user` record.
        let user_text = rq
            .get("message")
            .map(flatten_native_text)
            .unwrap_or_default();
        let mut user_rec = serde_json::Map::new();
        user_rec.insert("id".into(), Value::String(format!("{request_id}:user")));
        user_rec.insert("role".into(), Value::String("user".into()));
        user_rec.insert("sessionId".into(), Value::String(session_id.clone()));
        if let Some(ms) = ts_ms {
            user_rec.insert("ts".into(), Value::Number(ms.into()));
        }
        user_rec.insert("text".into(), Value::String(user_text));
        push_record(out, file, line_no, &Value::Object(user_rec));

        // 2) The assembled assistant response → an `assistant` record carrying
        //    text (concatenated markdown), tool calls, and file edits.
        let (text, tool_calls, edits) = assemble_response(rq.get("response"));
        let mut asst = serde_json::Map::new();
        asst.insert("id".into(), Value::String(format!("{response_id}:asst")));
        asst.insert(
            "parentId".into(),
            Value::String(format!("{request_id}:user")),
        );
        asst.insert("role".into(), Value::String("assistant".into()));
        asst.insert("sessionId".into(), Value::String(session_id.clone()));
        if let Some(ms) = ts_ms {
            asst.insert("ts".into(), Value::Number(ms.into()));
        }
        asst.insert("text".into(), Value::String(text));
        if let Some(model) = &responder {
            asst.insert("model".into(), Value::String(model.clone()));
        }
        if !tool_calls.is_empty() {
            asst.insert("toolCalls".into(), Value::Array(tool_calls));
        }
        if !edits.is_empty() {
            asst.insert("edits".into(), Value::Array(edits));
        }
        push_record(out, file, line_no, &Value::Object(asst));
    }
}

/// Walk a request's `response` part list and return the concatenated assistant
/// text, the normalized tool calls, and the normalized file edits. Tolerant of
/// the many VS Code part shapes; unknown parts contribute nothing (the raw
/// session text the user sees lives in the markdown parts).
fn assemble_response(response: Option<&Value>) -> (String, Vec<Value>, Vec<Value>) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut edits = Vec::new();

    let Some(parts) = response.and_then(Value::as_array) else {
        // Some builds store the response as a bare string or object.
        let text = match response {
            Some(Value::String(s)) => s.clone(),
            Some(obj @ Value::Object(_)) => part_markdown(obj).unwrap_or_default(),
            _ => String::new(),
        };
        return (text, tool_calls, edits);
    };

    for part in parts {
        let Some(p) = part.as_object() else { continue };
        let kind = p.get("kind").and_then(Value::as_str);
        match kind {
            // A part with no `kind` but a `value` is the live markdown shape.
            Some("markdownContent") | None => {
                if let Some(md) = part_markdown(part) {
                    if !md.is_empty() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(&md);
                    }
                }
            }
            Some("toolInvocationSerialized") | Some("prepareToolInvocation") => {
                if let Some(call) = tool_call_from_part(p, tool_calls.len()) {
                    tool_calls.push(call);
                }
            }
            Some("textEditGroup") | Some("codeblockUri") | Some("inlineReference") => {
                edits.extend(edits_from_part(p));
            }
            // Other parts (progressTask, confirmation, warning, …) carry no
            // dialogue/edit content we map; they're intentionally skipped.
            _ => {}
        }
    }

    (text, tool_calls, edits)
}

/// Extract assistant markdown text from a part. The text lives in `value` (the
/// live shape), or `content.value`, or a plain `content` string.
fn part_markdown(part: &Value) -> Option<String> {
    let obj = part.as_object()?;
    if let Some(s) = obj.get("value").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    match obj.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(c)) => c.get("value").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// Normalize a `toolInvocationSerialized`/`prepareToolInvocation` part into a
/// `{id, name, args}` tool-call record. The human-readable invocation message
/// (`invocationMessage.value`) rides along under `args` so it isn't lost.
fn tool_call_from_part(part: &serde_json::Map<String, Value>, index: usize) -> Option<Value> {
    let call_id = part
        .get("toolCallId")
        .or_else(|| part.get("toolId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("tool:{index}"));
    let name = part
        .get("toolId")
        .or_else(|| part.get("toolName"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| nested_value_string(part.get("invocationMessage")))
        .or_else(|| nested_value_string(part.get("pastTenseMessage")))
        .unwrap_or_default();
    let args = json!({
        "invocationMessage": nested_value_string(part.get("invocationMessage")),
        "pastTenseMessage": nested_value_string(part.get("pastTenseMessage")),
        "isComplete": part.get("isComplete").and_then(Value::as_bool),
    });
    Some(json!({ "id": call_id, "name": name, "args": args }))
}

/// Extract file edits from a `textEditGroup`/`codeblockUri`/`inlineReference`
/// part. The edited path comes from `uri` (a VS Code URI object with a `path`,
/// or a bare string); `textEditGroup` also carries `edits`/`textEdits`.
fn edits_from_part(part: &serde_json::Map<String, Value>) -> Vec<Value> {
    let Some(path) = uri_path(part.get("uri")).or_else(|| uri_path(part.get("resource"))) else {
        return Vec::new();
    };
    let mut edit = serde_json::Map::new();
    edit.insert("path".into(), Value::String(path));
    // Surface the raw edit operations under `diff` when present, so nothing is
    // silently dropped (VS Code stores no unified-diff text inline).
    if let Some(ops) = part
        .get("edits")
        .or_else(|| part.get("textEdits"))
        .filter(|v| !v.is_null())
    {
        if let Ok(s) = serde_json::to_string(ops) {
            edit.insert("diff".into(), Value::String(s));
        }
    }
    vec![Value::Object(edit)]
}

/// Resolve a path from a VS Code URI value: a `{path, scheme, …}` object, a
/// `{uri:{path}}` wrapper, or a bare string.
fn uri_path(value: Option<&Value>) -> Option<String> {
    let v = value?;
    if let Some(s) = v.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    let obj = v.as_object()?;
    obj.get("path")
        .or_else(|| obj.get("fsPath"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| uri_path(obj.get("uri")))
}

/// Read a `{value: "…"}` wrapper's string (VS Code's `MarkdownString` shape),
/// returning `None` for an absent/blank value.
fn nested_value_string(value: Option<&Value>) -> Option<String> {
    let v = value?;
    if let Some(s) = v.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    v.as_object()?
        .get("value")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Read a JSON number whether it was stored as a number or a numeric string
/// (VS Code stores epoch-millis timestamps as numbers).
fn value_as_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// Append a JSON value as a `RawRecord` with a fresh, stable provenance line
/// pointing back into the store (`db path : line_no`).
fn push_record(out: &mut Vec<RawRecord>, file: &Path, line_no: &mut u64, value: &Value) {
    *line_no += 1;
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
    let location = SourceLocation::new(file.to_path_buf(), 0, *line_no);
    out.push(RawRecord::new(bytes, location));
}

/// Fall back to reading a non-database path as either a whole JSON document or
/// line-delimited records.
///
/// VS Code exports sometimes arrive as a pretty-printed `.json` document rather
/// than JSONL: either a single native `chatSessions` object or an array of
/// session objects. In those cases we preserve one logical session per
/// [`RawRecord`] so the parser sees the same shape it expects from the SQLite
/// reader. When the file is not a whole JSON document, we fall back to
/// line-delimited reading for the exported-JSONL fixtures.
fn read_lines(path: &Path) -> Result<Vec<RawRecord>, ParseError> {
    let bytes = std::fs::read(path)
        .map_err(|e| ParseError::Io(format!("reading {}: {e}", path.display())))?;

    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
        let mut out = Vec::new();
        let mut line_no = 0;
        match value {
            Value::Array(items) => {
                for item in items {
                    push_record(&mut out, path, &mut line_no, &item);
                }
                return Ok(out);
            }
            Value::Object(_) => {
                push_record(&mut out, path, &mut line_no, &value);
                return Ok(out);
            }
            _ => {}
        }
    }

    let content = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        out.push(RawRecord::from_line(
            line,
            SourceLocation::new(path, 0, i as u64 + 1),
        ));
    }
    Ok(out)
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

    #[test]
    fn discover_finds_windows_appdata_workspace_store() {
        // 2026-07 regression: there was no Windows candidate at all — on a
        // real Windows VS Code install, discover() returned zero handles.
        let tmp = tempfile::tempdir().unwrap();
        let ws_hash = tmp
            .path()
            .join("AppData/Roaming/Code/User/workspaceStorage/abc123hash");
        std::fs::create_dir_all(&ws_hash).unwrap();
        std::fs::write(ws_hash.join("state.vscdb"), b"").unwrap();

        let cfg = DiscoverCfg {
            home: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        let handles = VsCodeAdapter.discover(&cfg);
        assert!(handles.iter().any(|h| h.path == ws_hash.join("state.vscdb")));
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

    // ---- native SQLite reader against the REAL state.vscdb schema ----
    //
    // These tests build a throwaway `.vscdb` on disk with the same
    // `ItemTable(key, value)` row the live VS Code store uses
    // (`key = 'interactive.sessions'`, a JSON array of sessions), then drive
    // `read_native` → `parse` and assert the events. No live-store dependency and
    // no private data ship with the crate.

    /// Build a throwaway `.vscdb` whose `ItemTable` holds the given
    /// `interactive.sessions` JSON array, mirroring the real schema. Returns the
    /// temp path (the caller removes it).
    fn build_vscdb(sessions: &Value) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let blob = serde_json::to_string(sessions).unwrap();
        let uniq = format!(
            "memscribe-vscode-test-{}-{}.vscdb",
            std::process::id(),
            content_id(blob.as_bytes())
        );
        path.push(uniq);
        let _ = std::fs::remove_file(&path);

        let conn = rusqlite::Connection::open(&path).expect("create temp vscdb");
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .expect("create ItemTable");
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![SESSIONS_KEY, blob],
        )
        .expect("insert interactive.sessions");
        drop(conn);
        path
    }

    /// Read a built store through the adapter and parse every record.
    fn read_and_parse(path: &std::path::Path) -> (Vec<CaptureEvent>, ParseCtx) {
        let adapter = VsCodeAdapter;
        let handle = TranscriptHandle {
            path: path.to_path_buf(),
            source: SOURCE,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).expect("read_native ok");
        let mut ctx = ParseCtx::new();
        let mut events = Vec::new();
        for r in &records {
            events.extend(adapter.parse(r, &mut ctx).expect("parse never errors"));
        }
        (events, ctx)
    }

    #[test]
    fn store_reader_is_native() {
        assert_eq!(
            VsCodeAdapter.store_reader(),
            memscribe_core::StoreReader::Native
        );
    }

    #[test]
    fn native_reader_extracts_turns_tool_and_edit_from_real_schema() {
        // One session with one request: a user message, then a response list with
        // a tool invocation, a markdownContent part (the assistant text), and a
        // textEditGroup part (a file edit) — the documented real VS Code shapes.
        let sessions = serde_json::json!([{
            "version": 3,
            "sessionId": "11111111-2222-3333-4444-555555555555",
            "requesterUsername": "dev",
            "responderUsername": "GitHub Copilot",
            "creationDate": 1_782_000_000_000_i64,
            "lastMessageDate": 1_782_000_005_000_i64,
            "customTitle": "Switch DB engine",
            "requests": [{
                "requestId": "request_abc",
                "responseId": "response_xyz",
                "timestamp": 1_782_000_005_000_i64,
                "message": {
                    "text": "Use Postgres instead of MySQL",
                    "parts": [{"kind": "text", "text": "Use Postgres instead of MySQL"}]
                },
                "response": [
                    {
                        "kind": "toolInvocationSerialized",
                        "toolId": "copilot_editFile",
                        "invocationMessage": {"value": "Editing db/config.toml"},
                        "pastTenseMessage": {"value": "Edited db/config.toml"},
                        "isComplete": true
                    },
                    {
                        "kind": "markdownContent",
                        "content": {"value": "Switching the engine to Postgres now."}
                    },
                    {
                        "value": " It is done.",
                        "supportThemeIcons": false
                    },
                    {
                        "kind": "textEditGroup",
                        "uri": {"$mid": 1, "path": "/work/db/config.toml", "scheme": "file"},
                        "edits": [[{"range": {}, "text": "engine=postgres"}]]
                    }
                ]
            }]
        }]);

        let path = build_vscdb(&sessions);
        let (events, ctx) = read_and_parse(&path);
        let _ = std::fs::remove_file(&path);

        // user_turn, assistant_turn (with concatenated markdown), tool_call, file_edit.
        assert_eq!(
            tags(&events),
            vec!["user_turn", "assistant_turn", "tool_call", "file_edit"]
        );

        // Session bound from the store's sessionId.
        assert_eq!(
            ctx.session_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );

        // User prompt recovered verbatim from message.text.
        match &events[0].kind {
            EventKind::UserTurn { text, .. } => {
                assert_eq!(text, "Use Postgres instead of MySQL");
            }
            other => panic!("expected user_turn, got {other:?}"),
        }

        // Assistant text = the markdownContent part + the kind-less markdown part,
        // joined; the responder name rides through as the model.
        match &events[1].kind {
            EventKind::AssistantTurn { text, model, .. } => {
                assert!(text.contains("Switching the engine to Postgres now."));
                assert!(text.contains("It is done."));
                assert_eq!(model.as_deref(), Some("GitHub Copilot"));
            }
            other => panic!("expected assistant_turn, got {other:?}"),
        }

        // Tool call from the toolInvocationSerialized part.
        match &events[2].kind {
            EventKind::ToolCall { name, .. } => assert_eq!(name, "copilot_editFile"),
            other => panic!("expected tool_call, got {other:?}"),
        }

        // File edit lifted from the textEditGroup uri.
        match &events[3].kind {
            EventKind::FileEdit { diff, .. } => {
                assert_eq!(diff.path, PathBuf::from("/work/db/config.toml"));
                // The raw edit operations are surfaced (no inline unified diff).
                assert!(diff
                    .unified
                    .as_deref()
                    .unwrap_or("")
                    .contains("engine=postgres"));
            }
            other => panic!("expected file_edit, got {other:?}"),
        }

        // The timestamp came from the request's epoch-millis `timestamp`.
        assert!(events[0].timestamp.unix_timestamp() > 1_700_000_000);
    }

    #[test]
    fn native_reader_missing_key_is_empty_not_error() {
        // A `.vscdb` with no `interactive.sessions` row degrades to an empty
        // record set (lossless), never an error.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "memscribe-vscode-empty-{}.vscdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES ('unrelated', 'x')",
            [],
        )
        .unwrap();
        drop(conn);

        let adapter = VsCodeAdapter;
        let handle = TranscriptHandle {
            path: path.clone(),
            source: SOURCE,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).expect("no key is ok");
        let _ = std::fs::remove_file(&path);
        assert!(records.is_empty());
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
