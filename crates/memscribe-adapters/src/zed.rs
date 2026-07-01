//! Zed adapter (native SQLite reader).
//!
//! Zed's agent panel persists each conversation as a **row in a SQLite
//! database**, not as a line-delimited log. This adapter therefore declares
//! [`StoreReader::Native`] and reads that store itself in [`read_native`], then
//! normalizes every thread into the same `{role,text,ts,threadId,edits,
//! toolCalls,toolResults}` JSON record shape the pure [`parse`] consumes.
//!
//! [`read_native`]: ZedAdapter::read_native
//! [`parse`]: ZedAdapter::parse
//!
//! ## On-disk store (reverse-engineered, read-only)
//!
//! Primary store — the agent panel threads:
//!   `~/Library/Application Support/Zed/threads/threads.db` (macOS)
//!   `~/.local/share/zed/threads/threads.db` (Linux)
//!
//! Schema (observed on a live install, Zed thread `version` 0.3.0):
//! ```sql
//! CREATE TABLE threads (
//!     id                 TEXT PRIMARY KEY,
//!     summary            TEXT NOT NULL,
//!     updated_at         TEXT NOT NULL,   -- RFC3339
//!     data_type          TEXT NOT NULL,   -- "zstd" (zstd-compressed JSON) | "json" (raw)
//!     data               BLOB NOT NULL,   -- the serialized thread
//!     parent_id          TEXT,
//!     folder_paths       TEXT,
//!     folder_paths_order TEXT,
//!     created_at         TEXT             -- RFC3339, added in a later migration
//! );
//! ```
//!
//! When `data_type = "zstd"` the `data` blob is **zstd-compressed JSON** (magic
//! bytes `28 b5 2f fd`); when `data_type = "json"` it is raw JSON. Either way the
//! decompressed bytes are a serialized thread:
//! ```json
//! {"version":"0.3.0","title":"…","model":{"provider":"…","model":"…"},
//!  "updated_at":"…","detailed_summary":"…","request_token_usage":{…},
//!  "messages":[
//!    {"User":{"id":"…","content":[{"Text":"…"},{"Mention":{…}}]}},
//!    {"Agent":{"content":[
//!         {"Thinking":{"text":"…","signature":"…"}},
//!         {"Text":"…"},
//!         {"ToolUse":{"id":"…","name":"read_file","input":{…},"raw_input":"…"}}],
//!       "tool_results":["<tool_use_id>", …],
//!       "reasoning_details":{…}}}
//!  ]}
//! ```
//!
//! Each message is a **single-key externally-tagged object**: `{"User":{…}}` or
//! `{"Agent":{…}}` (roles are `User`/`Agent`, NOT `Assistant`). Each `content`
//! segment is likewise single-key tagged:
//! - `{"Text":"…"}` — visible dialogue text.
//! - `{"Thinking":{"text":"…","signature":"…"}}` — model reasoning (Agent only).
//! - `{"ToolUse":{"id","name","input","raw_input",…}}` — a tool invocation. The
//!   file-editing ones (`edit_file`/`create_file`/`str_replace`/…) carry a path
//!   (and any old/new text or diff) in `input`; `read_native` lifts those into
//!   the record's `edits[]` so [`parse`] can emit [`EventKind::FileEdit`].
//! - `{"Mention":{"uri":{…},"content":…}}` — a context/file mention, **not**
//!   dialogue; dropped from the turn text (preserved structurally as a part is
//!   out of scope — it carries no conversation content).
//!
//! `Agent.tool_results` is a list of **strings** (the tool_use ids that
//! completed) — Zed does not inline a rich result object here, so each becomes a
//! minimal `ToolResult{ok:true}` keyed by that id.
//!
//! Field names and the message/segment encoding drift across Zed versions; this
//! reader also tolerates an **older, internally-tagged** shape
//! (`{"role":"user","segments":[{"type":"text","text":…}],"tool_uses":[…]}`) so
//! pre-0.3 stores and fixtures keep working. Anything unrecognized is preserved.
//!
//! ## Record shape produced by `read_native` (one JSON object per `RawRecord`)
//!
//! A leading per-thread header, then one message record per message:
//! ```json
//! {"kind":"session_start","sessionId":"<thread id>","cwd":"…","ts":"…",
//!  "toolVersion":"zed/0.3.0","summary":"…"}
//! {"id":"<thread>:msg:0","role":"user","sessionId":"<thread id>","threadId":"…",
//!  "ts":"…","text":"…"}
//! {"id":"<thread>:msg:1","role":"assistant","sessionId":"<thread id>","threadId":"…",
//!  "ts":"…","text":"…","thinking":"…",
//!  "toolCalls":[{"id":"…","name":"…","args":{…}}],
//!  "toolResults":[{"id":"…","ok":true,"output":…}],
//!  "edits":[{"path":"…","oldText":"…","newText":"…","diff":"…","callId":"…"}]}
//! ```
//!
//! ## Mapping (`parse`)
//! - `kind:session_start` → [`EventKind::SessionStart`] (binds `ctx.session_id`/`ctx.project`).
//! - `role:user` → [`EventKind::UserTurn`]; `role:assistant` → [`EventKind::AssistantTurn`].
//! - `toolCalls[]` → [`EventKind::ToolCall`]; `toolResults[]` → [`EventKind::ToolResult`].
//! - `edits[]` → [`EventKind::FileEdit`] with a normalized [`Diff`].
//! - the thread `summary` rides on `session_start`; a thread with no usable
//!   messages still yields a `SessionStart` (never silently dropped).
//!
//! ## Invariants
//! - [`read_native`] opens the store strictly read-only (`mode=ro&immutable=1`)
//!   and never writes to it.
//! - [`parse`] never panics (no `unwrap`/`expect`/indexing on parsed input) and
//!   is fully deterministic (rows sorted by `(updated_at, id)`, messages in array
//!   order; no clock/random/global state).
//! - Records are deduped by id via [`ParseCtx::first_seen`].
//! - Any valid-but-unrecognized record (or a blob that can't be decoded) becomes
//!   [`EventKind::Unknown`] so the stream stays lossless across Zed format churn.

use crate::util;
use memscribe_core::{
    CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, ProjectRef,
    RawRecord, SchemaVariant, SourceKind, SourceLocation, StoreReader, TranscriptAdapter,
    TranscriptHandle, Usage,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const SRC: SourceKind = SourceKind::Zed;

/// Adapter for Zed transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct ZedAdapter;

impl TranscriptAdapter for ZedAdapter {
    fn source_kind(&self) -> SourceKind {
        SRC
    }

    /// Zed keeps its conversation in a SQLite database — read it natively.
    fn store_reader(&self) -> StoreReader {
        StoreReader::Native
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        let home = cfg.home_dir();
        // The agent-panel thread store, in product-location precedence order.
        //
        // 2026-07: Zed's own source (crates/paths/src/paths.rs data_dir())
        // resolves via `dirs::data_local_dir()`, which honors $XDG_DATA_HOME
        // on Linux (default ~/.local/share) rather than a hardcoded literal,
        // and has a full Windows candidate at %LOCALAPPDATA%\Zed — Zed has
        // shipped full, official Windows support since ~April 2026, so
        // omitting it entirely (as before) left every Windows install
        // undiscoverable, not a deliberate "unsupported platform" gap.
        let xdg_data_home = std::env::var("XDG_DATA_HOME")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        let candidates = [
            home.join("Library/Application Support/Zed/threads/threads.db"), // macOS
            xdg_data_home.join("zed/threads/threads.db"),                    // Linux
            home.join("AppData/Local/Zed/threads/threads.db"), // Windows — %LOCALAPPDATA%\Zed
            home.join(".local/share/zed/threads.db"),
        ];
        let mut handles = Vec::new();
        for path in candidates {
            if !path.is_file() {
                continue;
            }
            handles.push(TranscriptHandle {
                path,
                source: SRC,
                session_hint: None,
                compressed: false,
            });
        }
        // Deterministic ordering across platforms / filesystem iteration order.
        handles.sort_by(|a, b| a.path.cmp(&b.path));
        handles
    }

    /// Open Zed's `threads.db` strictly read-only and yield one [`RawRecord`] per
    /// logical message (plus a per-thread `session_start` header). The records
    /// are the normalized JSON shape documented above, which [`parse`] then
    /// consumes purely.
    ///
    /// [`parse`]: ZedAdapter::parse
    fn read_native(&self, handle: &TranscriptHandle) -> Result<Vec<RawRecord>, ParseError> {
        read_threads_db(&handle.path)
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        // Blank → nothing; non-JSON garbage → lossless Unknown.
        let Some(value) = util::parse_json_line(raw) else {
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
        };

        let Some(obj) = value.as_object() else {
            return Ok(vec![util::unknown_event(SRC, ctx, raw, value)]);
        };

        // Seed the session id from any record that carries one (records are
        // produced in thread order, so the header wins for the whole stream).
        if ctx.session_id.is_none() {
            if let Some(sid) = str_field(obj, "sessionId") {
                ctx.session_id = Some(sid.to_string());
            }
        }

        // `kind`-tagged control records (session lifecycle).
        if let Some(kind) = str_field(obj, "kind") {
            match kind {
                "session_start" => return Ok(parse_session_start(obj, ctx, raw)),
                "session_end" => return Ok(parse_session_end(obj, ctx, raw)),
                _ => return Ok(vec![util::unknown_event(SRC, ctx, raw, value)]),
            }
        }

        // Otherwise it should be a `role`-tagged message record.
        if str_field(obj, "role").is_some() {
            return Ok(parse_message(obj, ctx, raw));
        }

        // Valid JSON we don't recognize → Unknown (losslessness).
        Ok(vec![util::unknown_event(SRC, ctx, raw, value)])
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        let Some(obj) = util::parse_json_line(sample)
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
        else {
            return SchemaVariant::unknown(SRC);
        };
        let looks_like_zed = matches!(str_field(&obj, "kind"), Some("session_start"))
            || (obj.contains_key("role")
                && (obj.contains_key("toolCalls")
                    || obj.contains_key("toolResults")
                    || obj.contains_key("edits")
                    || obj.contains_key("threadId")
                    || obj.contains_key("sessionId")));
        if looks_like_zed {
            SchemaVariant::certain(SRC, "zed/threads-db-v1")
        } else {
            SchemaVariant::unknown(SRC)
        }
    }
}

// ---------------------------------------------------------------------------
// Native store reader: threads.db → normalized RawRecords
// ---------------------------------------------------------------------------

/// Open `threads.db` read-only and expand every thread row into normalized
/// records. Deterministic: rows are ordered by `(updated_at, id)` and messages
/// in their stored array order. A missing file yields an empty stream rather
/// than an error (so an absent Zed install is a no-op, not a failure).
fn read_threads_db(path: &Path) -> Result<Vec<RawRecord>, ParseError> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    // Strictly read-only + immutable: never mutate the user's live store, and
    // tolerate a concurrently-open Zed (immutable=1 skips lock/WAL coordination).
    let uri = format!("file:{}?mode=ro&immutable=1", path.display());
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI;
    let conn = rusqlite::Connection::open_with_flags(&uri, flags)
        .map_err(|e| ParseError::Io(format!("opening Zed threads.db {}: {e}", path.display())))?;

    // Probe which optional columns exist so we work across Zed migrations
    // (older stores predate `created_at`).
    let has_created_at = column_exists(&conn, "threads", "created_at");

    let select = if has_created_at {
        "SELECT id, summary, updated_at, data_type, data, created_at \
         FROM threads ORDER BY updated_at ASC, id ASC"
    } else {
        "SELECT id, summary, updated_at, data_type, data, NULL as created_at \
         FROM threads ORDER BY updated_at ASC, id ASC"
    };

    let mut stmt = conn
        .prepare(select)
        .map_err(|e| ParseError::Io(format!("preparing Zed thread query: {e}")))?;

    // Collect rows first (owned), so the connection/statement lifetimes stay
    // local and the rest of the work is pure. `data` is read tolerantly: the
    // schema declares it BLOB, but a TEXT-storing build must not error out
    // (mirrors the Cursor `col_bytes` pattern).
    let rows = stmt
        .query_map([], |row| {
            Ok(ThreadRow {
                id: text_col(row, 0),
                summary: text_col(row, 1),
                updated_at: text_col(row, 2),
                data_type: text_col(row, 3),
                data: col_bytes(row, 4)?,
                created_at: {
                    let c = text_col(row, 5);
                    if c.is_empty() {
                        None
                    } else {
                        Some(c)
                    }
                },
            })
        })
        .map_err(|e| ParseError::Io(format!("querying Zed threads: {e}")))?;

    let mut threads: Vec<ThreadRow> = Vec::new();
    for r in rows {
        match r {
            Ok(t) => threads.push(t),
            Err(e) => return Err(ParseError::Io(format!("reading Zed thread row: {e}"))),
        }
    }

    let file = path.to_path_buf();
    let mut out = Vec::new();
    // 1-based logical line number across the synthesized record stream, so each
    // RawRecord carries a stable provenance pointer back into the store.
    let mut line_no: u64 = 0;
    for thread in &threads {
        expand_thread(thread, &file, &mut line_no, &mut out);
    }
    Ok(out)
}

/// A thread row, owned so it outlives the SQLite statement.
struct ThreadRow {
    id: String,
    summary: String,
    updated_at: String,
    data_type: String,
    data: Vec<u8>,
    created_at: Option<String>,
}

/// Read a SQLite column as a `String` whether it is stored as TEXT, BLOB, an
/// integer, or NULL — never erroring (`row.get::<String>` rejects non-TEXT).
fn text_col(row: &rusqlite::Row<'_>, idx: usize) -> String {
    match row.get_ref(idx) {
        Ok(rusqlite::types::ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Ok(rusqlite::types::ValueRef::Blob(b)) => String::from_utf8_lossy(b).into_owned(),
        Ok(rusqlite::types::ValueRef::Integer(n)) => n.to_string(),
        Ok(rusqlite::types::ValueRef::Real(r)) => r.to_string(),
        _ => String::new(),
    }
}

/// Read a SQLite column as raw bytes whether it is stored as BLOB or TEXT, so a
/// build that stores `data` as TEXT is accepted rather than erroring out (the
/// schema declares BLOB, but rusqlite will not coerce TEXT → `Vec<u8>`).
fn col_bytes(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Vec<u8>> {
    Ok(match row.get_ref(idx)? {
        rusqlite::types::ValueRef::Blob(b) => b.to_vec(),
        rusqlite::types::ValueRef::Text(t) => t.to_vec(),
        // Null / numeric storage carries no thread bytes — treat as empty.
        _ => Vec::new(),
    })
}

/// Decode a thread `data` blob into its JSON value, honoring `data_type`.
///
/// `zstd` ⇒ zstd-decompress then parse; `json`/empty ⇒ parse raw. As a final
/// tolerance, if the labeled path fails but the bytes happen to begin with the
/// zstd magic (or are valid JSON), we try the other decoder — so a mislabeled
/// row is still recovered. Returns `None` when nothing parses.
fn decode_thread_blob(data_type: &str, data: &[u8]) -> Option<Value> {
    if data.is_empty() {
        return None;
    }
    let is_zstd_label = data_type.eq_ignore_ascii_case("zstd");
    let has_zstd_magic = data.len() >= 4 && data[..4] == [0x28, 0xb5, 0x2f, 0xfd];

    // Primary attempt per the label.
    if is_zstd_label || has_zstd_magic {
        if let Some(v) = zstd_decode_json(data) {
            return Some(v);
        }
    }
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            return Some(v);
        }
    }
    // Fallback: try zstd even when unlabeled (covers a mislabeled `json` row).
    if !is_zstd_label && !has_zstd_magic {
        if let Some(v) = zstd_decode_json(data) {
            return Some(v);
        }
    }
    None
}

/// zstd-decompress `data` and parse the result as JSON. Total / never panics.
fn zstd_decode_json(data: &[u8]) -> Option<Value> {
    let bytes = zstd::stream::decode_all(data).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

/// Expand one thread row into a `session_start` header followed by one record
/// per message. A row whose blob can't be understood still yields a header (so
/// the thread is never silently lost) plus one lossless `Unknown`-shaped record.
fn expand_thread(thread: &ThreadRow, file: &Path, line_no: &mut u64, out: &mut Vec<RawRecord>) {
    let ts = if thread.updated_at.is_empty() {
        None
    } else {
        Some(thread.updated_at.as_str())
    };

    let blob = decode_thread_blob(&thread.data_type, &thread.data);
    let blob_obj = blob.as_ref().and_then(Value::as_object);

    // Summary / title: prefer the blob's `summary` then `title`, else the row.
    let summary = blob_obj
        .and_then(|o| str_field(o, "summary").or_else(|| str_field(o, "title")))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            if thread.summary.is_empty() {
                None
            } else {
                Some(thread.summary.clone())
            }
        });
    let version = blob_obj
        .and_then(|o| str_field(o, "version"))
        .map(str::to_string);
    // The model is recorded on the thread (`model:{provider,model}`) in the
    // real format; surface it on the header so SessionStart carries it.
    let model = blob_obj.and_then(thread_model);

    // 1) The per-thread session_start header.
    let mut header = Map::new();
    header.insert("kind".into(), Value::String("session_start".into()));
    header.insert("sessionId".into(), Value::String(thread.id.clone()));
    if let Some(ts) = thread
        .created_at
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(ts)
    {
        header.insert("ts".into(), Value::String(ts.to_string()));
    }
    if let Some(s) = &summary {
        header.insert("summary".into(), Value::String(s.clone()));
    }
    if let Some(m) = &model {
        header.insert("model".into(), Value::String(m.clone()));
    }
    header.insert(
        "toolVersion".into(),
        Value::String(match &version {
            Some(v) => format!("zed/{v}"),
            None => "zed".into(),
        }),
    );
    push_record(out, file, line_no, &Value::Object(header));

    // 2) The messages.
    let messages = blob_obj
        .and_then(|o| o.get("messages"))
        .and_then(Value::as_array);

    match messages {
        Some(msgs) if !msgs.is_empty() => {
            for (i, msg) in msgs.iter().enumerate() {
                if let Some(rec) = message_record(thread, msg, i, ts) {
                    push_record(out, file, line_no, &rec);
                }
            }
        }
        _ => {
            // No usable messages. If the blob existed but had an unrecognized
            // shape, preserve it losslessly as one Unknown-tagged record so no
            // thread content is ever dropped.
            if let Some(b) = &blob {
                if blob_obj
                    .map(|o| !o.contains_key("messages"))
                    .unwrap_or(true)
                {
                    let mut rec = Map::new();
                    rec.insert("kind".into(), Value::String("zed_thread_blob".into()));
                    rec.insert("sessionId".into(), Value::String(thread.id.clone()));
                    rec.insert("data".into(), b.clone());
                    push_record(out, file, line_no, &Value::Object(rec));
                }
            } else if !thread.data.is_empty() {
                // An undecodable blob (corrupt / a future encoding): record a
                // structural Unknown marker (we do not embed the raw bytes).
                let mut rec = Map::new();
                rec.insert("kind".into(), Value::String("zed_thread_blob".into()));
                rec.insert("sessionId".into(), Value::String(thread.id.clone()));
                rec.insert("dataType".into(), Value::String(thread.data_type.clone()));
                rec.insert(
                    "byteLen".into(),
                    Value::Number(serde_json::Number::from(thread.data.len() as u64)),
                );
                push_record(out, file, line_no, &Value::Object(rec));
            }
        }
    }
}

/// Pull a human-readable model string off a thread blob. The real format stores
/// `model:{provider,model}`; older shapes store a flat `model`/`model_id`.
fn thread_model(o: &Map<String, Value>) -> Option<String> {
    if let Some(m) = o.get("model").and_then(Value::as_object) {
        let provider = str_field(m, "provider").unwrap_or("");
        let name = str_field(m, "model").or_else(|| str_field(m, "id"))?;
        if provider.is_empty() {
            return Some(name.to_string());
        }
        return Some(format!("{provider}/{name}"));
    }
    str_field(o, "model")
        .or_else(|| str_field(o, "model_id"))
        .map(str::to_string)
}

/// Build a normalized message record from one Zed message. Handles both the real
/// **externally-tagged** shape (`{"User":{…}}` / `{"Agent":{…}}`) and the older
/// **internally-tagged** shape (`{"role":…,"segments":[…]}`). Returns `None` only
/// for a non-object message with no recognizable role.
fn message_record(
    thread: &ThreadRow,
    msg: &Value,
    index: usize,
    thread_ts: Option<&str>,
) -> Option<Value> {
    let m = msg.as_object()?;

    // Distinguish the encoding. Real format: a single key `User`/`Agent` whose
    // value is the body. Legacy format: a flat object carrying `role`.
    let (role, body): (&str, &Map<String, Value>) =
        if let Some(body) = m.get("User").and_then(Value::as_object) {
            ("user", body)
        } else if let Some(body) = m.get("Agent").and_then(Value::as_object) {
            ("assistant", body)
        } else if str_field(m, "role").is_some() {
            // Legacy internally-tagged message.
            let role = match str_field(m, "role").unwrap_or("") {
                "user" | "User" => "user",
                "assistant" | "Assistant" | "Agent" => "assistant",
                other => other,
            };
            (role, m)
        } else {
            return None;
        };

    let mut rec = Map::new();

    // Stable, deterministic id: thread id + the body's native id or its index.
    let native_id = body
        .get("id")
        .map(value_to_id)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| index.to_string());
    rec.insert(
        "id".into(),
        Value::String(format!("{}:msg:{native_id}", thread.id)),
    );
    rec.insert("role".into(), Value::String(role.to_string()));
    rec.insert("sessionId".into(), Value::String(thread.id.clone()));
    rec.insert("threadId".into(), Value::String(thread.id.clone()));
    // Per-message timestamps are absent in the real format; fall back to the
    // thread timestamp so events stay temporally ordered within the thread.
    if let Some(ts) = str_field(body, "timestamp")
        .or_else(|| str_field(body, "created_at"))
        .or(thread_ts)
    {
        rec.insert("ts".into(), Value::String(ts.to_string()));
    }

    // text + thinking from `content[]` segments (real) or `segments[]`/flat
    // fields (legacy).
    let (text, thinking) = collect_text(body);
    rec.insert("text".into(), Value::String(text));
    if let Some(th) = thinking {
        if !th.is_empty() {
            rec.insert("thinking".into(), Value::String(th));
        }
    }

    // model / usage on assistant turns (legacy flat shapes; the real format
    // records these at thread level, surfaced on the header instead).
    if role == "assistant" {
        if let Some(model) = str_field(body, "model").or_else(|| str_field(body, "model_id")) {
            rec.insert("model".into(), Value::String(model.to_string()));
        }
        if let Some(usage) = body.get("usage").and_then(Value::as_object) {
            let mut u = Map::new();
            for (src, dst) in [
                ("input_tokens", "input"),
                ("input", "input"),
                ("output_tokens", "output"),
                ("output", "output"),
            ] {
                if let Some(n) = usage.get(src).and_then(Value::as_u64) {
                    u.entry(dst.to_string())
                        .or_insert_with(|| Value::Number(n.into()));
                }
            }
            if !u.is_empty() {
                rec.insert("usage".into(), Value::Object(u));
            }
        }
    }

    // tool uses → toolCalls[], and the file-editing ones → edits[].
    let (tool_calls, edits) = collect_tools(body);
    if !tool_calls.is_empty() {
        rec.insert("toolCalls".into(), Value::Array(tool_calls));
    }
    if !edits.is_empty() {
        rec.insert("edits".into(), Value::Array(edits));
    }

    // tool results → toolResults[].
    let tool_results = collect_results(body);
    if !tool_results.is_empty() {
        rec.insert("toolResults".into(), Value::Array(tool_results));
    }

    Some(Value::Object(rec))
}

/// Flatten a message body's text and thinking. Handles the real
/// externally-tagged `content[]` segments (`{"Text":"…"}`,
/// `{"Thinking":{text,…}}`, `{"Mention":{…}}`, `{"ToolUse":{…}}`), the legacy
/// internally-tagged `segments[]` (`{"type":"text"|"thinking","text":…}`), and
/// flat string fields — in that precedence. `Mention` and `ToolUse` segments
/// carry no dialogue text and are skipped here (tool uses are lifted separately).
fn collect_text(m: &Map<String, Value>) -> (String, Option<String>) {
    let mut text = String::new();
    let mut thinking = String::new();

    if let Some(content) = m.get("content").and_then(Value::as_array) {
        for seg in content {
            match classify_segment(seg) {
                Segment::Text(s) => push_chunk(&mut text, s),
                Segment::Thinking(s) => push_chunk(&mut thinking, s),
                // Mention / ToolUse / unknown → no dialogue text.
                Segment::Other => {}
            }
        }
    } else if let Some(segments) = m.get("segments").and_then(Value::as_array) {
        // Legacy internally-tagged segments.
        for seg in segments {
            let Some(s) = seg.as_object() else { continue };
            let kind = str_field(s, "type").unwrap_or("text");
            let chunk = str_field(s, "text")
                .or_else(|| str_field(s, "content"))
                .unwrap_or("");
            if chunk.is_empty() {
                continue;
            }
            match kind {
                "thinking" | "redacted_thinking" => push_chunk(&mut thinking, chunk),
                _ => push_chunk(&mut text, chunk),
            }
        }
    }

    // Fallbacks for shapes that store the body as a flat field.
    if text.is_empty() {
        if let Some(t) = str_field(m, "text") {
            text.push_str(t);
        } else if let Some(t) = m.get("content").and_then(Value::as_str) {
            text.push_str(t);
        }
    }
    if thinking.is_empty() {
        if let Some(t) = str_field(m, "thinking") {
            thinking.push_str(t);
        }
    }

    let thinking = if thinking.is_empty() {
        None
    } else {
        Some(thinking)
    };
    (text, thinking)
}

/// A classified content segment.
enum Segment<'a> {
    Text(&'a str),
    Thinking(&'a str),
    Other,
}

/// Classify one externally-tagged `content[]` segment. Tolerant: an unexpected
/// shape (or a `Text`/`Thinking` carrying no string) becomes [`Segment::Other`].
fn classify_segment(seg: &Value) -> Segment<'_> {
    let Some(obj) = seg.as_object() else {
        // A bare string segment is treated as visible text.
        if let Some(s) = seg.as_str() {
            return Segment::Text(s);
        }
        return Segment::Other;
    };
    // Externally-tagged: exactly one key naming the variant.
    if let Some(s) = obj.get("Text").and_then(Value::as_str) {
        return Segment::Text(s);
    }
    if let Some(th) = obj.get("Thinking") {
        if let Some(s) = th
            .as_str()
            .or_else(|| th.get("text").and_then(Value::as_str))
        {
            return Segment::Thinking(s);
        }
        return Segment::Other;
    }
    // `Mention`, `ToolUse`, and anything else carry no dialogue text.
    Segment::Other
}

/// Lift a message body's tool uses into `toolCalls[]`, and the file-editing ones
/// into `edits[]` (correlated back to the originating call via `callId`).
/// Recognizes the real `content[]` `{"ToolUse":{…}}` segments and the legacy
/// `tool_uses[]` array.
fn collect_tools(m: &Map<String, Value>) -> (Vec<Value>, Vec<Value>) {
    let mut calls = Vec::new();
    let mut edits = Vec::new();

    // Gather raw tool-use objects from whichever shape is present.
    let mut uses: Vec<&Map<String, Value>> = Vec::new();
    if let Some(content) = m.get("content").and_then(Value::as_array) {
        for seg in content {
            if let Some(tu) = seg
                .as_object()
                .and_then(|o| o.get("ToolUse"))
                .and_then(Value::as_object)
            {
                uses.push(tu);
            }
        }
    }
    if uses.is_empty() {
        if let Some(arr) = m
            .get("tool_uses")
            .or_else(|| m.get("toolUses"))
            .and_then(Value::as_array)
        {
            for u in arr {
                if let Some(o) = u.as_object() {
                    uses.push(o);
                }
            }
        }
    }

    for (i, u) in uses.iter().enumerate() {
        let call_id = u
            .get("id")
            .map(value_to_id)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("toolcall{i}"));
        let name = str_field(u, "name").unwrap_or("").to_string();
        // Prefer the structured `input`; fall back to `raw_input` (a JSON
        // string), then legacy `args`/`arguments`.
        let args = u
            .get("input")
            .filter(|v| !v.is_null())
            .cloned()
            .or_else(|| decode_json_string(u.get("raw_input")))
            .or_else(|| u.get("args").cloned())
            .or_else(|| u.get("arguments").cloned())
            .unwrap_or(Value::Null);

        let mut call = Map::new();
        call.insert("id".into(), Value::String(call_id.clone()));
        call.insert("name".into(), Value::String(name.clone()));
        call.insert("args".into(), args.clone());
        calls.push(Value::Object(call));

        // File-editing tools → one or more normalized edits, joined to this call.
        edits.extend(edits_from_tool(&name, &args, &call_id));
    }

    (calls, edits)
}

/// Decode a JSON-encoded **string** field (Zed stores `raw_input` as a string
/// JSON blob). Returns the parsed value, or `None` when absent / empty / not a
/// string.
fn decode_json_string(v: Option<&Value>) -> Option<Value> {
    let s = v?.as_str()?;
    if s.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(s).ok()
}

/// If a tool use is a file edit, derive normalized edit object(s) from its input.
/// Recognizes Zed/ACP edit tools by name and pulls a path plus any old/new/diff.
///
/// Zed's `edit_file` carries `{path, edits:[{old_text,new_text}, …]}` — a single
/// tool call applies **several** sub-edits to one file. Each sub-edit yields its
/// own normalized edit (sharing the path + `callId`). Tools that put old/new/diff
/// directly on the input (and legacy shapes) yield a single edit. Returns an
/// empty vec for a non-edit tool or one with no recoverable path.
fn edits_from_tool(name: &str, args: &Value, call_id: &str) -> Vec<Value> {
    let lname = name.to_ascii_lowercase();
    let is_edit = lname.contains("edit")
        || lname.contains("create_file")
        || lname.contains("write")
        || lname.contains("str_replace")
        || lname.contains("apply_patch")
        || lname.contains("replace");
    if !is_edit {
        return Vec::new();
    }
    let Some(a) = args.as_object() else {
        return Vec::new();
    };
    let Some(path) = a
        .get("path")
        .or_else(|| a.get("file_path"))
        .or_else(|| a.get("abs_path"))
        .or_else(|| a.get("filePath"))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };

    // Zed nests the actual hunks under `edits:[{old_text,new_text}, …]`.
    if let Some(sub_edits) = a.get("edits").and_then(Value::as_array) {
        let mut out = Vec::new();
        for sub in sub_edits {
            let Some(s) = sub.as_object() else { continue };
            out.push(normalized_edit(path, s, call_id));
        }
        if !out.is_empty() {
            return out;
        }
        // An `edits` array with no usable hunks → still record the path edit.
    }

    // Single-hunk / legacy shape: old/new/diff live on the input itself.
    vec![normalized_edit(path, a, call_id)]
}

/// Build one normalized edit object for `path` from a hunk map (which may carry
/// `old_text`/`new_text`/`diff` under various key spellings), joined to `call_id`.
fn normalized_edit(path: &str, hunk: &Map<String, Value>, call_id: &str) -> Value {
    let mut edit = Map::new();
    edit.insert("path".into(), Value::String(path.to_string()));
    if let Some(old) = hunk
        .get("old_text")
        .or_else(|| hunk.get("old_string"))
        .or_else(|| hunk.get("oldText"))
        .and_then(Value::as_str)
    {
        edit.insert("oldText".into(), Value::String(old.to_string()));
    }
    if let Some(new) = hunk
        .get("new_text")
        .or_else(|| hunk.get("new_string"))
        .or_else(|| hunk.get("newText"))
        .or_else(|| hunk.get("content"))
        .and_then(Value::as_str)
    {
        edit.insert("newText".into(), Value::String(new.to_string()));
    }
    if let Some(diff) = hunk
        .get("diff")
        .or_else(|| hunk.get("patch"))
        .and_then(Value::as_str)
    {
        edit.insert("diff".into(), Value::String(diff.to_string()));
    }
    edit.insert("callId".into(), Value::String(call_id.to_string()));
    Value::Object(edit)
}

/// Lift a message body's tool results into `toolResults[]`.
///
/// In the real format `tool_results` is a **map** `{<tool_use_id>: {tool_use_id,
/// tool_name, is_error, content, output}, …}` — one rich result per completed
/// call, with `ok` derived from `is_error`. We also tolerate older shapes: a list
/// of such objects, and a list of bare `tool_use_id` strings (each a successful
/// result). Map iteration is sorted by key so output is deterministic.
fn collect_results(m: &Map<String, Value>) -> Vec<Value> {
    let raw = m.get("tool_results").or_else(|| m.get("toolResults"));
    let Some(raw) = raw else {
        return Vec::new();
    };

    // The real format: a map keyed by tool_use_id. Sort keys for determinism.
    if let Some(map) = raw.as_object() {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        let mut out = Vec::new();
        for k in keys {
            if let Some(r) = map.get(k).and_then(Value::as_object) {
                out.push(result_object(r, k));
            }
        }
        return out;
    }

    let Some(results) = raw.as_array() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (i, res_v) in results.iter().enumerate() {
        // A bare tool_use id string → a successful result.
        if let Some(id) = res_v.as_str() {
            if id.is_empty() {
                continue;
            }
            let mut out_obj = Map::new();
            out_obj.insert("id".into(), Value::String(id.to_string()));
            out_obj.insert("ok".into(), Value::Bool(true));
            out_obj.insert("output".into(), Value::Null);
            out.push(Value::Object(out_obj));
            continue;
        }

        let Some(r) = res_v.as_object() else { continue };
        let fallback = format!("toolresult{i}");
        out.push(result_object(r, &fallback));
    }
    out
}

/// Normalize one tool-result object into `{id, ok, output}`. `id` prefers the
/// object's own `tool_use_id`/`id`, else `default_id`; `ok` is `!is_error`
/// (absent ⇒ success); `output` prefers a flat `output` string, else `content`.
fn result_object(r: &Map<String, Value>, default_id: &str) -> Value {
    let call_id = r
        .get("tool_use_id")
        .or_else(|| r.get("toolUseId"))
        .or_else(|| r.get("id"))
        .map(value_to_id)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_id.to_string());
    // Zed/Anthropic mark failure with `is_error: true`; absence ⇒ success.
    let ok = match r.get("is_error").or_else(|| r.get("isError")) {
        Some(Value::Bool(b)) => !b,
        _ => r.get("ok").and_then(Value::as_bool).unwrap_or(true),
    };
    let output = r
        .get("output")
        .or_else(|| r.get("content"))
        .cloned()
        .unwrap_or(Value::Null);

    let mut out_obj = Map::new();
    out_obj.insert("id".into(), Value::String(call_id));
    out_obj.insert("ok".into(), Value::Bool(ok));
    out_obj.insert("output".into(), output);
    Value::Object(out_obj)
}

/// Append a JSON value as a `RawRecord` with a fresh, stable provenance line.
fn push_record(out: &mut Vec<RawRecord>, file: &Path, line_no: &mut u64, value: &Value) {
    *line_no += 1;
    // `to_string` is deterministic (serde_json preserves insertion order).
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
    let location = SourceLocation::new(file.to_path_buf(), 0, *line_no);
    out.push(RawRecord::new(bytes, location));
}

/// Render a JSON id field as a string (Zed message ids are sometimes integers).
fn value_to_id(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// Append a chunk to a text accumulator, inserting a newline between chunks.
fn push_chunk(buf: &mut String, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(chunk);
}

/// Does `column` exist on `table`? Used to stay tolerant of Zed migrations.
fn column_exists(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    for r in rows.flatten() {
        if r.eq_ignore_ascii_case(column) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Pure parse: normalized JSON records → CaptureEvents
// ---------------------------------------------------------------------------

/// Parse a `session_start` header, binding session + project on `ctx`.
fn parse_session_start(
    obj: &Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
    if let Some(sid) = str_field(obj, "sessionId") {
        ctx.session_id = Some(sid.to_string());
    }
    let cwd = str_field(obj, "cwd").map(PathBuf::from).unwrap_or_else(|| {
        // Best-effort: a worktree path if the header carries one, else ".".
        str_field(obj, "worktree")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    });
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

/// Parse a `session_end` trailer into a `SessionEnd` event.
fn parse_session_end(
    obj: &Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
    if let Some(sid) = str_field(obj, "sessionId") {
        if ctx.session_id.is_none() {
            ctx.session_id = Some(sid.to_string());
        }
    }
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

/// Parse a `role`-tagged message record into its turn plus any embedded tool
/// calls, tool results, and file edits — in a stable, deterministic order.
fn parse_message(
    obj: &Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
    if let Some(sid) = str_field(obj, "sessionId") {
        if ctx.session_id.is_none() {
            ctx.session_id = Some(sid.to_string());
        }
    }

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
            parts: Vec::new(),
        },
        "assistant" => EventKind::AssistantTurn {
            text,
            thinking: str_field(obj, "thinking").map(str::to_string),
            model: str_field(obj, "model").map(str::to_string),
            usage: parse_usage(obj.get("usage")),
            parts: Vec::new(),
        },
        _ => {
            // An unrecognized role → lossless Unknown for the whole record.
            return vec![util::unknown_event(
                SRC,
                ctx,
                raw,
                Value::Object(obj.clone()),
            )];
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

    // 2) Tool calls embedded in the turn.
    if let Some(calls) = obj.get("toolCalls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            let Some(call_obj) = call.as_object() else {
                continue;
            };
            let call_id = str_field(call_obj, "id")
                .map(str::to_string)
                .unwrap_or_else(|| format!("{base_id}#call{i}"));
            let name = str_field(call_obj, "name").unwrap_or("").to_string();
            let args = call_obj.get("args").cloned().unwrap_or(Value::Null);
            ctx.call_names.insert(call_id.clone(), name.clone());
            let ev_id = format!("{base_id}:call:{call_id}");
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

    // 3) Tool results embedded in the turn.
    if let Some(results) = obj.get("toolResults").and_then(Value::as_array) {
        for (i, res) in results.iter().enumerate() {
            let Some(res_obj) = res.as_object() else {
                continue;
            };
            let call_id = str_field(res_obj, "id")
                .map(str::to_string)
                .unwrap_or_else(|| format!("{base_id}#res{i}"));
            let ok = res_obj.get("ok").and_then(Value::as_bool).unwrap_or(true);
            ctx.call_ok.insert(call_id.clone(), ok);
            let output = res_obj.get("output").cloned().unwrap_or(Value::Null);
            let ev_id = format!("{base_id}:result:{call_id}");
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

    // 4) File edits embedded in the turn.
    if let Some(edits) = obj.get("edits").and_then(Value::as_array) {
        for (i, edit) in edits.iter().enumerate() {
            let Some(edit_obj) = edit.as_object() else {
                continue;
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
            // Correlate to a tool call: an explicit callId, else the sole call.
            let call_id = str_field(edit_obj, "callId")
                .or_else(|| str_field(edit_obj, "call_id"))
                .map(str::to_string)
                .or_else(|| sole_tool_call_id(obj));
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

/// Resolve the `event_id`: tool-native `id` when present, else a content hash.
fn event_id_for(obj: &Map<String, Value>, raw: &RawRecord) -> String {
    str_field(obj, "id")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| memscribe_core::content_id(&raw.bytes))
}

/// The record timestamp, from any of the common keys (epoch default).
fn ts_for(obj: &Map<String, Value>) -> memscribe_core::Timestamp {
    util::ts_from(
        &Value::Object(obj.clone()),
        &["ts", "timestamp", "time", "created_at", "updated_at"],
    )
}

/// The optional DAG parent link.
fn parent_field(obj: &Map<String, Value>) -> Option<String> {
    str_field(obj, "parentId")
        .or_else(|| str_field(obj, "parent_id"))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse an optional `git` object into a [`GitRef`]. A missing/blank sha yields
/// `None` rather than an empty ref.
fn parse_git(value: Option<&Value>) -> Option<GitRef> {
    let g = value?.as_object()?;
    let sha = str_field(g, "sha").filter(|s| !s.is_empty())?.to_string();
    Some(GitRef {
        sha,
        branch: str_field(g, "branch")
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// Parse an optional `usage` object. Returns `None` when no fields are present.
fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let u = value?.as_object()?;
    let input_tokens = u64_field(u, "input").or_else(|| u64_field(u, "input_tokens"));
    let output_tokens = u64_field(u, "output").or_else(|| u64_field(u, "output_tokens"));
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
fn sole_tool_call_id(obj: &Map<String, Value>) -> Option<String> {
    let calls = obj.get("toolCalls").and_then(Value::as_array)?;
    if calls.len() != 1 {
        return None;
    }
    calls
        .first()
        .and_then(Value::as_object)
        .and_then(|c| str_field(c, "id"))
        .map(str::to_string)
}

// ---- small, total field accessors (no panics, no indexing) ----

fn str_field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

fn u64_field(obj: &Map<String, Value>, key: &str) -> Option<u64> {
    obj.get(key).and_then(Value::as_u64)
}

fn u32_field(obj: &Map<String, Value>, key: &str) -> Option<u32> {
    obj.get(key)
        .and_then(Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;
    use rusqlite::Connection;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn raw(s: &str) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("zed-threads.db", 0, 1))
    }

    /// A self-cleaning scratch directory under the OS temp dir (we avoid an extra
    /// dev-dependency on `tempfile`). The path is unique per call.
    struct ScratchDir {
        path: PathBuf,
    }

    impl ScratchDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("memscribe-zed-test-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create scratch dir");
            ScratchDir { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Parse a slice of RawRecords through one shared ctx, mirroring runtime use.
    fn parse_records(records: &[RawRecord]) -> (Vec<CaptureEvent>, ParseCtx) {
        let adapter = ZedAdapter;
        let mut ctx = ParseCtx::new();
        let mut out = Vec::new();
        for rec in records {
            let evs = adapter.parse(rec, &mut ctx).expect("parse never errors");
            out.extend(evs);
        }
        (out, ctx)
    }

    fn parse_lines(lines: &str) -> (Vec<CaptureEvent>, ParseCtx) {
        let recs: Vec<RawRecord> = lines.lines().map(raw).collect();
        parse_records(&recs)
    }

    fn tags(evs: &[CaptureEvent]) -> Vec<&'static str> {
        evs.iter().map(|e| e.kind.tag()).collect()
    }

    /// zstd-compress some bytes for the real-format fixtures.
    fn zstd_compress(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 0).expect("zstd compress")
    }

    /// Create the REAL Zed `threads` schema on a fresh connection.
    fn create_threads_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                data_type TEXT NOT NULL,
                data BLOB NOT NULL,
                parent_id TEXT,
                folder_paths TEXT,
                folder_paths_order TEXT,
                created_at TEXT
            );",
        )
        .unwrap();
    }

    // ---- read_native against the REAL externally-tagged, zstd format ----

    /// Build a temp SQLite database matching the REAL Zed `threads` schema, with
    /// one `data_type='zstd'` thread whose blob is zstd-compressed JSON in the
    /// real `{messages:[{User:{…}},{Agent:{…}}]}` externally-tagged shape: a User
    /// message (Text + Mention), and an Agent message with Thinking + Text + an
    /// `edit_file` ToolUse (with Zed's nested `input.edits[]` hunks), plus the
    /// real map-shaped `tool_results` (keyed by tool_use_id, with `is_error`).
    fn write_zed_db_real(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        create_threads_table(&conn);

        let thread = serde_json::json!({
            "version": "0.3.0",
            "title": "Switch DB to Postgres",
            "model": {"provider": "zed.dev", "model": "gpt-5.4"},
            "updated_at": "2026-06-22T10:00:00Z",
            "messages": [
                {"User": {
                    "id": "6b5d4efe-user",
                    "content": [
                        {"Text": "Use Postgres instead of MySQL"},
                        {"Mention": {"uri": {"Directory": {"abs_path": "/w/app/"}}, "content": ""}}
                    ]
                }},
                {"Agent": {
                    "content": [
                        {"Thinking": {"text": "they want postgres", "signature": "sig"}},
                        {"Text": "Editing the db config."},
                        {"ToolUse": {
                            "id": "call_edit1",
                            "name": "edit_file",
                            "raw_input": "{\"path\":\"src/db.rs\"}",
                            "input": {
                                "path": "src/db.rs",
                                "edits": [
                                    {"old_text": "mysql", "new_text": "postgres"}
                                ]
                            },
                            "is_input_complete": true
                        }}
                    ],
                    // Real shape: a MAP keyed by tool_use_id → rich result object.
                    "tool_results": {
                        "call_edit1": {
                            "tool_use_id": "call_edit1",
                            "tool_name": "edit_file",
                            "is_error": false,
                            "content": [{"Text": "edited"}],
                            "output": "edited"
                        }
                    },
                    "reasoning_details": {"reasoning_items": []}
                }}
            ]
        });
        let blob = zstd_compress(&serde_json::to_vec(&thread).unwrap());
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "thread-aaa",
                "Switch DB to Postgres",
                "2026-06-22T10:00:00Z",
                "zstd",
                blob,
                "2026-06-22T09:59:00Z",
            ],
        )
        .unwrap();
    }

    #[test]
    fn discover_finds_windows_localappdata_threads_db() {
        // 2026-07 regression: zed.rs had NO Windows candidate at all, despite
        // Zed shipping full official Windows support since ~April 2026 — the
        // real path (per Zed's own paths.rs data_dir()) is
        // %LOCALAPPDATA%\Zed\threads\threads.db.
        let dir = ScratchDir::new();
        let win_threads = dir.path().join("AppData/Local/Zed/threads/threads.db");
        std::fs::create_dir_all(win_threads.parent().unwrap()).unwrap();
        std::fs::write(&win_threads, b"").unwrap();

        let cfg = DiscoverCfg {
            home: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let handles = ZedAdapter.discover(&cfg);
        assert!(handles.iter().any(|h| h.path == win_threads));
    }

    #[test]
    fn read_native_extracts_real_zstd_thread_shape() {
        let dir = ScratchDir::new();
        let db = dir.path().join("threads.db");
        write_zed_db_real(&db);

        let adapter = ZedAdapter;
        assert_eq!(adapter.store_reader(), StoreReader::Native);
        let handle = TranscriptHandle {
            path: db.clone(),
            source: SourceKind::Zed,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).expect("read_native ok");
        // header + 2 messages.
        assert_eq!(records.len(), 3);

        let (evs, ctx) = parse_records(&records);
        assert_eq!(
            tags(&evs),
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "tool_call",
                "tool_result",
                "file_edit",
            ]
        );

        // Session/project bound from the synthesized header; model surfaced.
        assert_eq!(ctx.session_id.as_deref(), Some("thread-aaa"));
        assert!(evs.iter().all(|e| e.session_id == "thread-aaa"));
        match &evs[0].kind {
            EventKind::SessionStart { model, .. } => {
                assert_eq!(model.as_deref(), Some("zed.dev/gpt-5.4"));
            }
            other => panic!("expected SessionStart, got {other:?}"),
        }

        // User turn text came out of the Text segment; the Mention is dropped.
        match &evs[1].kind {
            EventKind::UserTurn { text, .. } => {
                assert!(text.contains("Postgres"));
                assert!(!text.contains("abs_path"), "Mention must not leak in text");
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
        // Assistant turn: Text from the Text segment, thinking from the Thinking
        // segment.
        match &evs[2].kind {
            EventKind::AssistantTurn { text, thinking, .. } => {
                assert!(text.contains("Editing"));
                assert_eq!(thinking.as_deref(), Some("they want postgres"));
            }
            other => panic!("expected AssistantTurn, got {other:?}"),
        }
        // Tool call from the ToolUse segment.
        match &evs[3].kind {
            EventKind::ToolCall { name, call_id, .. } => {
                assert_eq!(name, "edit_file");
                assert_eq!(call_id, "call_edit1");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        // Tool result: from the map entry; is_error:false ⇒ ok:true, output set.
        match &evs[4].kind {
            EventKind::ToolResult {
                call_id,
                ok,
                output,
            } => {
                assert_eq!(call_id, "call_edit1");
                assert!(ok);
                assert_eq!(output, &Value::String("edited".to_string()));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        // File edit lifted from the edit tool's input, joined to the call.
        match &evs[5].kind {
            EventKind::FileEdit { call_id, diff } => {
                assert_eq!(call_id.as_deref(), Some("call_edit1"));
                assert_eq!(diff.path, PathBuf::from("src/db.rs"));
                assert_eq!(diff.old.as_deref(), Some("mysql"));
                assert_eq!(diff.new.as_deref(), Some("postgres"));
            }
            other => panic!("expected FileEdit, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_results_and_multi_hunk_edits() {
        // The real format keys `tool_results` by tool_use_id and `edit_file`
        // carries several hunks under `input.edits[]`. One edit_file call with
        // two hunks ⇒ two FileEdits; a failed result ⇒ ok:false.
        let dir = ScratchDir::new();
        let db = dir.path().join("threads.db");
        let conn = Connection::open(&db).unwrap();
        create_threads_table(&conn);

        let thread = serde_json::json!({
            "version": "0.3.0",
            "title": "Multi-hunk",
            "messages": [
                {"User": {"id": "u1", "content": [{"Text": "fix two spots"}]}},
                {"Agent": {
                    "content": [
                        {"ToolUse": {
                            "id": "call_e",
                            "name": "edit_file",
                            "input": {
                                "path": "src/lib.rs",
                                "edits": [
                                    {"old_text": "a", "new_text": "b"},
                                    {"old_text": "c", "new_text": "d"}
                                ]
                            }
                        }},
                        {"ToolUse": {"id": "call_t", "name": "terminal", "input": {"command": "false"}}}
                    ],
                    "tool_results": {
                        "call_e": {"tool_use_id": "call_e", "is_error": false, "output": "ok"},
                        "call_t": {"tool_use_id": "call_t", "is_error": true, "output": "boom"}
                    }
                }}
            ]
        });
        let blob = zstd_compress(&serde_json::to_vec(&thread).unwrap());
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data)
             VALUES ('t-mh', 'Multi-hunk', '2026-06-22T10:00:00Z', 'zstd', ?1)",
            rusqlite::params![blob],
        )
        .unwrap();

        let adapter = ZedAdapter;
        let handle = TranscriptHandle {
            path: db,
            source: SourceKind::Zed,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).unwrap();
        let (evs, ctx) = parse_records(&records);
        // session_start, user_turn, assistant_turn, 2 tool_calls,
        // 2 tool_results, 2 file_edits.
        assert_eq!(
            tags(&evs),
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "tool_call",
                "tool_call",
                "tool_result",
                "tool_result",
                "file_edit",
                "file_edit",
            ]
        );
        // The failed terminal result is observable.
        assert_eq!(ctx.call_ok.get("call_t").copied(), Some(false));
        assert_eq!(ctx.call_ok.get("call_e").copied(), Some(true));
        // Both hunks of the single edit_file call surface, sharing path + callId.
        let edits: Vec<&Diff> = evs
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::FileEdit { diff, .. } => Some(diff),
                _ => None,
            })
            .collect();
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|d| d.path == Path::new("src/lib.rs")));
        assert_eq!(edits[0].old.as_deref(), Some("a"));
        assert_eq!(edits[0].new.as_deref(), Some("b"));
        assert_eq!(edits[1].old.as_deref(), Some("c"));
        assert_eq!(edits[1].new.as_deref(), Some("d"));
    }

    #[test]
    fn read_native_handles_uncompressed_json_data_type() {
        // A `data_type='json'` row whose blob is the real externally-tagged JSON
        // stored uncompressed must parse identically.
        let dir = ScratchDir::new();
        let db = dir.path().join("threads.db");
        let conn = Connection::open(&db).unwrap();
        create_threads_table(&conn);

        let thread = serde_json::json!({
            "version": "0.3.0",
            "title": "Plain JSON thread",
            "messages": [
                {"User": {"id": "u1", "content": [{"Text": "hello there"}]}},
                {"Agent": {
                    "content": [{"Text": "hi back"}],
                    "tool_results": []
                }}
            ]
        });
        let blob = serde_json::to_vec(&thread).unwrap();
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data)
             VALUES ('t-json', 'Plain JSON thread', '2026-06-22T10:00:00Z', 'json', ?1)",
            rusqlite::params![blob],
        )
        .unwrap();

        let adapter = ZedAdapter;
        let handle = TranscriptHandle {
            path: db,
            source: SourceKind::Zed,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).unwrap();
        let (evs, _) = parse_records(&records);
        assert_eq!(
            tags(&evs),
            vec!["session_start", "user_turn", "assistant_turn"]
        );
        match &evs[1].kind {
            EventKind::UserTurn { text, .. } => assert_eq!(text, "hello there"),
            other => panic!("expected UserTurn, got {other:?}"),
        }
        match &evs[2].kind {
            EventKind::AssistantTurn { text, .. } => assert_eq!(text, "hi back"),
            other => panic!("expected AssistantTurn, got {other:?}"),
        }
    }

    #[test]
    fn read_native_supports_legacy_internally_tagged_shape() {
        // The pre-0.3 internally-tagged shape (role + segments + tool_uses) must
        // still parse so old stores / fixtures keep working.
        let dir = ScratchDir::new();
        let db = dir.path().join("threads.db");
        let conn = Connection::open(&db).unwrap();
        create_threads_table(&conn);

        let thread = serde_json::json!({
            "version": "0.2.0",
            "summary": "Legacy thread",
            "messages": [
                {"id": 1, "role": "user", "segments": [{"type": "text", "text": "old shape"}]},
                {
                    "id": 2,
                    "role": "assistant",
                    "segments": [
                        {"type": "thinking", "text": "hmm"},
                        {"type": "text", "text": "done"}
                    ],
                    "usage": {"input_tokens": 3, "output_tokens": 4},
                    "tool_uses": [{
                        "id": "tool_1",
                        "name": "edit_file",
                        "input": {"path": "a.rs", "old_text": "x", "new_text": "y"}
                    }],
                    "tool_results": [{"tool_use_id": "tool_1", "is_error": false, "content": "ok"}]
                }
            ]
        });
        let blob = serde_json::to_vec(&thread).unwrap();
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data)
             VALUES ('t-legacy', 'Legacy thread', '2026-06-22T10:00:00Z', 'json', ?1)",
            rusqlite::params![blob],
        )
        .unwrap();

        let adapter = ZedAdapter;
        let handle = TranscriptHandle {
            path: db,
            source: SourceKind::Zed,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).unwrap();
        let (evs, _) = parse_records(&records);
        assert_eq!(
            tags(&evs),
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "tool_call",
                "tool_result",
                "file_edit",
            ]
        );
        match &evs[2].kind {
            EventKind::AssistantTurn {
                text,
                thinking,
                usage,
                ..
            } => {
                assert_eq!(text, "done");
                assert_eq!(thinking.as_deref(), Some("hmm"));
                let u = usage.as_ref().expect("usage present");
                assert_eq!(u.input_tokens, Some(3));
                assert_eq!(u.output_tokens, Some(4));
            }
            other => panic!("expected AssistantTurn, got {other:?}"),
        }
    }

    #[test]
    fn read_native_is_deterministic_and_ordered() {
        let dir = ScratchDir::new();
        let db = dir.path().join("threads.db");
        let conn = Connection::open(&db).unwrap();
        create_threads_table(&conn);
        // Insert out of chronological order; reader must sort by updated_at.
        for (id, updated) in [
            ("t-late", "2026-06-22T12:00:00Z"),
            ("t-early", "2026-06-22T08:00:00Z"),
        ] {
            let blob = zstd_compress(
                &serde_json::to_vec(&serde_json::json!({
                    "messages": [{"User": {"id": "u", "content": [{"Text": "hi"}]}}]
                }))
                .unwrap(),
            );
            conn.execute(
                "INSERT INTO threads (id, summary, updated_at, data_type, data)
                 VALUES (?1, '', ?2, 'zstd', ?3)",
                rusqlite::params![id, updated, blob],
            )
            .unwrap();
        }

        let adapter = ZedAdapter;
        let handle = TranscriptHandle {
            path: db.clone(),
            source: SourceKind::Zed,
            session_hint: None,
            compressed: false,
        };
        let a = adapter.read_native(&handle).unwrap();
        let b = adapter.read_native(&handle).unwrap();
        assert_eq!(a, b, "read_native must be byte-deterministic");

        // The early thread's header must precede the late thread's header.
        let (evs, _) = parse_records(&a);
        let sessions: Vec<&str> = evs
            .iter()
            .filter(|e| e.kind.tag() == "session_start")
            .map(|e| e.session_id.as_str())
            .collect();
        assert_eq!(sessions, vec!["t-early", "t-late"]);
    }

    #[test]
    fn missing_db_is_empty_not_an_error() {
        let adapter = ZedAdapter;
        let handle = TranscriptHandle {
            path: PathBuf::from("/nonexistent/threads.db"),
            source: SourceKind::Zed,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).expect("missing db is ok");
        assert!(records.is_empty());
    }

    #[test]
    fn thread_with_unrecognized_blob_is_lossless() {
        let dir = ScratchDir::new();
        let db = dir.path().join("threads.db");
        let conn = Connection::open(&db).unwrap();
        create_threads_table(&conn);
        // A zstd JSON blob with no `messages` key → header + one lossless Unknown.
        let blob =
            zstd_compress(&serde_json::to_vec(&serde_json::json!({"unexpected": true})).unwrap());
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data)
             VALUES ('t-weird', '', '2026-06-22T10:00:00Z', 'zstd', ?1)",
            rusqlite::params![blob],
        )
        .unwrap();

        let adapter = ZedAdapter;
        let handle = TranscriptHandle {
            path: db,
            source: SourceKind::Zed,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).unwrap();
        let (evs, _) = parse_records(&records);
        assert_eq!(tags(&evs), vec!["session_start", "unknown"]);
    }

    #[test]
    fn corrupt_zstd_blob_yields_structural_unknown() {
        // A `data_type='zstd'` row whose bytes are not valid zstd nor JSON must
        // degrade to a header + a structural blob marker, never panic.
        let dir = ScratchDir::new();
        let db = dir.path().join("threads.db");
        let conn = Connection::open(&db).unwrap();
        create_threads_table(&conn);
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data)
             VALUES ('t-corrupt', '', '2026-06-22T10:00:00Z', 'zstd', ?1)",
            rusqlite::params![vec![0xde_u8, 0xad, 0xbe, 0xef, 0x00]],
        )
        .unwrap();

        let adapter = ZedAdapter;
        let handle = TranscriptHandle {
            path: db,
            source: SourceKind::Zed,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).unwrap();
        let (evs, _) = parse_records(&records);
        assert_eq!(tags(&evs), vec!["session_start", "unknown"]);
    }

    // ---- pure parse() behavior on the normalized record shape ----

    #[test]
    fn session_start_binds_session_and_project() {
        let line = r#"{"kind":"session_start","sessionId":"s1","cwd":"/w/orbit","git":{"sha":"abc","branch":"main"},"toolVersion":"zed/0.3.0","ts":"2026-06-22T10:00:00Z"}"#;
        let (evs, ctx) = parse_lines(line);
        assert_eq!(tags(&evs), vec!["session_start"]);
        assert_eq!(ctx.session_id.as_deref(), Some("s1"));
        match &evs[0].kind {
            EventKind::SessionStart {
                cwd,
                git,
                tool_version,
                ..
            } => {
                assert_eq!(cwd.as_path(), Path::new("/w/orbit"));
                assert_eq!(git.as_ref().map(|g| g.sha.as_str()), Some("abc"));
                assert_eq!(tool_version.as_deref(), Some("zed/0.3.0"));
            }
            other => panic!("expected SessionStart, got {other:?}"),
        }
        assert_eq!(evs[0].project.cwd, Path::new("/w/orbit"));
    }

    #[test]
    fn failed_tool_result_marks_call_not_ok() {
        let lines = concat!(
            r#"{"id":"a","role":"assistant","sessionId":"s","text":"editing","toolCalls":[{"id":"c9","name":"edit_file","args":{}}],"edits":[{"path":"src/c.rs","oldText":"x","newText":"y","callId":"c9"}]}"#,
            "\n",
            r#"{"id":"b","role":"assistant","sessionId":"s","text":"failed","toolResults":[{"id":"c9","ok":false,"output":"locked"}]}"#,
        );
        let (evs, ctx) = parse_lines(lines);
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
        match &evs[2].kind {
            EventKind::FileEdit { call_id, .. } => assert_eq!(call_id.as_deref(), Some("c9")),
            other => panic!("expected FileEdit, got {other:?}"),
        }
        assert_eq!(ctx.call_ok.get("c9"), Some(&false));
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
    fn unknown_role_and_kind_are_lossless() {
        let (evs, _) = parse_lines(r#"{"id":"x","role":"system","sessionId":"s","text":"boot"}"#);
        assert_eq!(tags(&evs), vec!["unknown"]);
        let (evs2, _) = parse_lines(r#"{"kind":"telemetry_ping","payload":42}"#);
        assert_eq!(tags(&evs2), vec!["unknown"]);
    }

    #[test]
    fn garbage_input_never_panics_and_is_lossless() {
        let adapter = ZedAdapter;
        let mut ctx = ParseCtx::new();
        let g = adapter.parse(&raw("}{ not json at all"), &mut ctx).unwrap();
        assert_eq!(tags(&g), vec!["unknown"]);
        let blank = adapter.parse(&raw("   "), &mut ctx).unwrap();
        assert!(blank.is_empty());
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
    fn no_id_falls_back_to_content_hash() {
        let (evs, _) = parse_lines(r#"{"role":"user","sessionId":"s","text":"anon"}"#);
        assert_eq!(tags(&evs), vec!["user_turn"]);
        assert_eq!(evs[0].event_id.len(), 64);
        assert!(evs[0].event_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprint_recognizes_zed_records() {
        let adapter = ZedAdapter;
        let start = raw(r#"{"kind":"session_start","sessionId":"s"}"#);
        assert_eq!(adapter.schema_fingerprint(&start).confidence, 100);
        let msg = raw(r#"{"id":"a","role":"assistant","threadId":"t","edits":[]}"#);
        assert_eq!(adapter.schema_fingerprint(&msg).confidence, 100);
        let foreign = raw(r#"{"type":"summary","text":"x"}"#);
        assert_eq!(adapter.schema_fingerprint(&foreign).confidence, 0);
    }
}
