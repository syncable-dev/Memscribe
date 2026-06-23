//! Cursor adapter.
//!
//! Cursor is a VS Code fork and is **database-backed**: its chat lives in a
//! SQLite store, `state.vscdb`, under the user's Cursor application-support
//! directory. This adapter reads that store **read-only** ([`StoreReader::Native`]),
//! normalizes each chat message into a stable one-object-per-line JSON record,
//! and [`parse`](CursorAdapter::parse) turns those records into events. The
//! normalized shape is the same `{role, text, ...}` shape the older exported-JSONL
//! model used, so on-disk export fixtures keep working unchanged.
//!
//! ## Reverse-engineered store schema (Cursor ~1.x, verified June 2026)
//!
//! Two SQLite files matter:
//! - `…/Cursor/User/globalStorage/state.vscdb` — holds **all** chat sessions.
//! - `…/Cursor/User/workspaceStorage/<hash>/state.vscdb` — per-workspace; in
//!   current builds the chat tables here are empty (only `aiService.prompts`),
//!   the conversation having migrated to `globalStorage`. We still read both.
//!
//! Each file has two tables: `ItemTable(key TEXT, value BLOB)` and
//! `cursorDiskKV(key TEXT, value BLOB)`. The chat lives in `cursorDiskKV`:
//!
//! - `composerData:<composerId>` → a JSON object describing one chat session
//!   (a "composer"). The fields we use:
//!   - `composerId` — the session id.
//!   - `name` — the session title (used as a `SessionStart`-ish subtitle).
//!   - `createdAt` — epoch-millis session-start time.
//!   - `fullConversationHeadersOnly` — an **ordered** array of
//!     `{bubbleId, type}` headers. `type: 1` = user, `type: 2` = assistant.
//!     This array is the canonical message order for the session.
//!
//! - `bubbleId:<composerId>:<bubbleId>` → one message ("bubble"). Fields:
//!   - `type` — `1` user, `2` assistant (mirrors the header).
//!   - `text` / `richText` — the message text (may be empty for tool-only
//!     assistant bubbles).
//!   - `createdAt` — an RFC3339 string timestamp (e.g. `"2026-06-08T…Z"`).
//!   - `toolFormerData` — present on assistant tool bubbles; a single tool
//!     call **and** its result, combined:
//!     - `name` — tool name (`edit_file_v2`, `read_file_v2`,
//!       `run_terminal_command_v2`, `ripgrep_raw_search`, `mcp-…`, …).
//!     - `toolCallId` — the tool-native call id.
//!     - `status` — `completed` | `error` | `cancelled` | `loading`.
//!       `error` ⇒ the result is not ok.
//!     - `rawArgs` / `params` — JSON-encoded **strings** of the call arguments
//!       (`rawArgs` is the model's raw args and is sometimes empty; `params`
//!       is Cursor's resolved args and is usually present). For an
//!       `edit_file_v2` the edited file path is in `rawArgs.path` or
//!       `params.relativeWorkspacePath`, and the new file contents (when the
//!       store kept them inline) are in `rawArgs.streamContent`.
//!     - `result` — a JSON-encoded **string** of the tool output.
//!
//! ### What the store gives up vs. what it does not
//! - Roles, message text, per-message timestamps, tool name/args/result/status,
//!   and the **path** of each file edit are all recoverable.
//! - Edit **old/new text** is *only* recoverable when `rawArgs.streamContent`
//!   is present (~⅔ of edits in the sampled store). Otherwise the edit `result`
//!   carries only content-hash ids (`beforeContentId`/`afterContentId`) that
//!   point at separate `composer.content.<hash>` rows; we surface the edit path
//!   and the content-id metadata but cannot synthesize a unified diff. Cursor
//!   stores no unified-diff text inline.
//!
//! ## Normalized record shape (one JSON object per [`RawRecord`])
//!
//! `read_native` emits, in deterministic order:
//! - one `{"kind":"session_start", …}` per composer, then
//! - one message record per bubble:
//!   `{"id","role":"user"|"assistant","ts","sessionId","text","composerId",
//!     "bubbleId","toolCalls":[…],"toolResults":[…],"edits":[…]}`.
//!
//! [`parse`](CursorAdapter::parse) expands each message into the turn
//! (`UserTurn`/`AssistantTurn`), then each `ToolCall`, each `ToolResult`, then
//! each `FileEdit`, exactly as the exported-JSONL model did.
//!
//! ## Invariants
//! [`parse`](CursorAdapter::parse) never panics, is deterministic (no clock /
//! randomness; rows are sorted by a stable key), dedups by record id, and routes
//! anything unrecognized to [`EventKind::Unknown`]. `read_native` opens SQLite
//! read-only (`mode=ro&immutable=1`) and never writes; missing tables/keys/blobs
//! degrade to an empty record set rather than an error.

use crate::util;
use memscribe_core::{
    content_id, CaptureEvent, Diff, DiscoverCfg, EventKind, GitRef, ParseCtx, ParseError, Part,
    ProjectRef, RawRecord, SchemaVariant, SourceKind, SourceLocation, StoreReader,
    TranscriptAdapter, TranscriptHandle, Usage,
};
use serde_json::{json, Value};
use std::path::PathBuf;

const SRC: SourceKind = SourceKind::Cursor;

/// Adapter for Cursor transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct CursorAdapter;

impl TranscriptAdapter for CursorAdapter {
    fn source_kind(&self) -> SourceKind {
        SRC
    }

    /// Cursor keeps its conversation in a SQLite database, so the adapter reads
    /// the store itself via [`read_native`](CursorAdapter::read_native) rather
    /// than the line-delimited file reader.
    fn store_reader(&self) -> StoreReader {
        StoreReader::Native
    }

    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle> {
        let mut out = Vec::new();
        let home = cfg.home_dir();
        // The real product locations. The conversation lives in the global
        // store; the per-workspace stores are read too (older builds kept chat
        // there). We also tolerate exported `.jsonl`/`.cursorchat` files so the
        // exported-transcript model still discovers.
        let roots = [
            home.join("Library/Application Support/Cursor/User/workspaceStorage"),
            home.join("Library/Application Support/Cursor/User/globalStorage"),
            home.join(".config/Cursor/User/workspaceStorage"),
            home.join(".config/Cursor/User/globalStorage"),
            home.join(".cursor"),
        ];
        for root in roots {
            for entry in walkdir::WalkDir::new(&root)
                .max_depth(3)
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                let is_store = name == "state.vscdb"
                    || name.ends_with(".jsonl")
                    || name.ends_with(".cursorchat");
                if is_store && path.is_file() {
                    let session_hint = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .map(str::to_string);
                    out.push(TranscriptHandle {
                        path: path.to_path_buf(),
                        source: SRC,
                        session_hint,
                        compressed: false,
                    });
                }
            }
        }
        // Deterministic order regardless of filesystem iteration order. Also
        // dedup identical paths (overlapping roots can surface a file twice).
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path);
        out
    }

    /// Read the Cursor SQLite store at `handle.path` **read-only** and yield one
    /// [`RawRecord`] per logical message (plus a `session_start` per composer),
    /// in deterministic order. A non-`.vscdb` path (e.g. an exported `.jsonl`)
    /// falls back to reading the file's lines, so the exported-transcript model
    /// keeps working.
    ///
    /// # Errors
    /// Returns [`ParseError::Io`] only if the path is a `.vscdb` that cannot be
    /// opened at all. Missing tables/keys/blobs inside a readable store degrade
    /// to an empty record set, never an error.
    fn read_native(&self, handle: &TranscriptHandle) -> Result<Vec<RawRecord>, ParseError> {
        let path = &handle.path;
        let is_vscdb = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("vscdb"))
            .unwrap_or(false);

        if is_vscdb {
            return read_vscdb(path);
        }

        // Not a database — treat it as a line-delimited exported transcript so
        // the JSONL fixtures and any future `.jsonl` exports still parse.
        read_lines(path)
    }

    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError> {
        // Blank lines yield nothing; invalid JSON is preserved verbatim as an
        // Unknown so the stream is still lossless (never an error here).
        let value = match util::parse_json_line(raw) {
            Some(v) => v,
            None => {
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
            }
        };

        // We only know how to parse JSON objects; anything else is Unknown.
        let obj = match value.as_object() {
            Some(o) => o,
            None => return Ok(vec![util::unknown_event(SRC, ctx, raw, value)]),
        };

        // Bind the session id to the record's own session/composer. A single
        // native store holds MANY composers, emitted composer-by-composer
        // (each composer's `session_start` precedes its bubbles), so the session
        // id must *track the current composer* rather than latch the first one —
        // otherwise every composer would collapse into one session and the
        // segmenter would bleed conversations across unrelated chats. For the
        // legacy single-session exported shape this is a no-op (every record
        // already carries the same id).
        if let Some(sid) = str_field(obj, "sessionId").or_else(|| str_field(obj, "composerId")) {
            if ctx.session_id.as_deref() != Some(sid) {
                ctx.session_id = Some(sid.to_string());
            }
        }

        // Dispatch on the record discriminator. A `kind` of `session_start`
        // (and a couple of tolerant aliases) means the session header; a `role`
        // means a dialogue turn. Everything else is Unknown.
        if let Some(kind) = str_field(obj, "kind") {
            match kind {
                "session_start" | "session-start" | "sessionStart" => {
                    return Ok(parse_session_start(obj, ctx, raw));
                }
                "session_end" | "session-end" | "sessionEnd" => {
                    return Ok(parse_session_end(obj, ctx, raw));
                }
                _ => {}
            }
        }

        if str_field(obj, "role").is_some() {
            return Ok(parse_message(obj, ctx, raw));
        }

        Ok(vec![util::unknown_event(SRC, ctx, raw, value)])
    }

    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant {
        match util::parse_json_line(sample)
            .as_ref()
            .and_then(Value::as_object)
        {
            Some(obj)
                if obj.contains_key("role")
                    || matches!(str_field(obj, "kind"), Some("session_start")) =>
            {
                SchemaVariant::certain(SRC, "cursor/vscdb-v1")
            }
            _ => SchemaVariant::unknown(SRC),
        }
    }
}

// ===========================================================================
// Native SQLite store reader
// ===========================================================================

/// Open a Cursor `state.vscdb` **read-only** and normalize every chat session it
/// holds into [`RawRecord`]s. Deterministic: composers are processed in
/// `composerId` order and bubbles in conversation-header order (falling back to
/// `createdAt`, then `bubbleId`). Errors only if the file can't be opened.
fn read_vscdb(path: &std::path::Path) -> Result<Vec<RawRecord>, ParseError> {
    // `mode=ro&immutable=1` opens read-only and promises we won't observe
    // concurrent writes — Cursor may have the live DB open, but we never write,
    // lock, or create side files (`-wal`/`-shm`).
    let uri = format!("file:{}?mode=ro&immutable=1", path.to_string_lossy());
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| ParseError::Io(format!("opening cursor store {}: {e}", path.display())))?;

    // Missing `cursorDiskKV` (e.g. an unrelated `.vscdb`) → no records, not an
    // error: an empty stream is the lossless outcome for an empty store.
    let composers = match load_composers(&conn) {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()),
    };

    // Process composers in a stable order (by composerId) so output is
    // deterministic regardless of SQLite row order.
    let mut composers = composers;
    composers.sort_by(|a, b| a.composer_id.cmp(&b.composer_id));

    let mut records = Vec::new();
    let mut line_no: u64 = 0;
    let file = path.to_path_buf();

    for composer in &composers {
        // 1) Session header for the composer.
        line_no += 1;
        let header = json!({
            "kind": "session_start",
            "sessionId": composer.composer_id,
            "composerId": composer.composer_id,
            "name": composer.name,
            "ts": composer.created_at_rfc3339(),
            "toolVersion": "cursor",
        });
        records.push(record_for(&file, line_no, &header));

        // 2) Determine the ordered bubble ids for this composer. Prefer the
        //    canonical conversation order from the headers; if absent, fall
        //    back to whatever bubbles exist (sorted deterministically below).
        let mut bubbles = load_bubbles(&conn, &composer.composer_id);

        order_bubbles(&mut bubbles, &composer.header_order);

        for bubble in &bubbles {
            line_no += 1;
            let record = normalize_bubble(&composer.composer_id, bubble);
            records.push(record_for(&file, line_no, &record));
        }
    }

    Ok(records)
}

/// One composer (chat session) loaded from `composerData:*`.
struct Composer {
    composer_id: String,
    name: Option<String>,
    /// Epoch-millis session-start time, if the store recorded one.
    created_at_ms: Option<i64>,
    /// Bubble ids in canonical conversation order (from
    /// `fullConversationHeadersOnly`); empty if the store had no headers.
    header_order: Vec<String>,
}

impl Composer {
    /// The composer's start time as an RFC3339 string for the normalized
    /// `session_start` record (epoch if unknown — keeps output deterministic).
    fn created_at_rfc3339(&self) -> String {
        match self.created_at_ms {
            Some(ms) => util::parse_ts(&ms.to_string())
                .and_then(|t| {
                    t.format(&time::format_description::well_known::Rfc3339)
                        .ok()
                })
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
            None => "1970-01-01T00:00:00Z".to_string(),
        }
    }
}

/// One message bubble loaded from `bubbleId:<composerId>:<bubbleId>`.
struct Bubble {
    bubble_id: String,
    /// The full bubble JSON object (so normalization is total/best-effort).
    value: Value,
}

impl Bubble {
    /// `createdAt` as a sortable string (RFC3339 sorts lexically by time);
    /// empty string sorts first for bubbles missing a timestamp.
    fn created_at(&self) -> &str {
        self.value
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

/// Read a SQLite column as raw bytes whether it is stored as TEXT or BLOB.
///
/// Current Cursor builds store `cursorDiskKV.value` as **TEXT** (a JSON string);
/// rusqlite will **not** coerce a TEXT column into `Vec<u8>` via `row.get` — it
/// returns an `InvalidColumnType` error. Reading through [`ValueRef`] accepts
/// either storage class, so neither TEXT nor BLOB stores are silently dropped.
///
/// [`ValueRef`]: rusqlite::types::ValueRef
fn col_bytes(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Vec<u8>> {
    Ok(match row.get_ref(idx)? {
        rusqlite::types::ValueRef::Text(t) => t.to_vec(),
        rusqlite::types::ValueRef::Blob(b) => b.to_vec(),
        // Null or numeric storage carries no JSON — treat as empty (the caller
        // then degrades to a skip), never an error.
        _ => Vec::new(),
    })
}

/// Load all `composerData:*` rows. Errors if `cursorDiskKV` is missing.
fn load_composers(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<Composer>> {
    let mut stmt =
        conn.prepare("SELECT value FROM cursorDiskKV WHERE key LIKE 'composerData:%'")?;
    let rows = stmt.query_map([], |row| {
        // Values are stored as TEXT (JSON) in current builds, BLOB in older
        // ones; read as bytes either way (see `col_bytes`).
        col_bytes(row, 0)
    })?;

    let mut out = Vec::new();
    for r in rows {
        let bytes = match r {
            Ok(b) => b,
            Err(_) => continue,
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let composer_id = match value.get("composerId").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && *s != "None")
            .map(str::to_string);
        let created_at_ms = value.get("createdAt").and_then(Value::as_i64);
        let header_order = value
            .get("fullConversationHeadersOnly")
            .and_then(Value::as_array)
            .map(|hs| {
                hs.iter()
                    .filter_map(|h| h.get("bubbleId").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        out.push(Composer {
            composer_id,
            name,
            created_at_ms,
            header_order,
        });
    }
    Ok(out)
}

/// Load all `bubbleId:<composerId>:*` rows for one composer.
fn load_bubbles(conn: &rusqlite::Connection, composer_id: &str) -> Vec<Bubble> {
    // `escape '\'` guards composer ids that contain LIKE metacharacters (`%`/`_`).
    let prefix = format!("bubbleId:{}:", escape_like(composer_id));
    let like = format!("{prefix}%");
    let mut stmt =
        match conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE ?1 ESCAPE '\\'") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
    let rows = stmt.query_map([&like], |row| {
        let key: String = row.get(0)?;
        // `value` is TEXT in current builds — read tolerantly (see `col_bytes`).
        let bytes = col_bytes(row, 1)?;
        Ok((key, bytes))
    });
    let rows = match rows {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    let unescaped_prefix = format!("bubbleId:{composer_id}:");
    for r in rows {
        let (key, bytes) = match r {
            Ok(kv) => kv,
            Err(_) => continue,
        };
        // The bubble id is the key suffix after `bubbleId:<composerId>:`.
        let bubble_id = key
            .strip_prefix(&unescaped_prefix)
            .unwrap_or(&key)
            .to_string();
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            // A bubble whose blob isn't JSON is preserved as a stub object so it
            // still becomes a (lossless) record downstream.
            Err(_) => Value::Object(serde_json::Map::new()),
        };
        out.push(Bubble { bubble_id, value });
    }
    out
}

/// Order `bubbles` by the canonical conversation `order` (the composer's header
/// list). Bubbles named in `order` come first, in that order; any extras
/// (orphans, or a composer with no headers) follow, sorted by `createdAt` then
/// `bubbleId` so the result is fully deterministic.
fn order_bubbles(bubbles: &mut [Bubble], order: &[String]) {
    use std::collections::HashMap;
    let rank: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, b)| (b.as_str(), i))
        .collect();
    // Stable sort: primary key = header rank (None ⇒ after all ranked, hence
    // usize::MAX), secondary = createdAt, tertiary = bubbleId.
    bubbles.sort_by(|a, b| {
        let ra = rank
            .get(a.bubble_id.as_str())
            .copied()
            .unwrap_or(usize::MAX);
        let rb = rank
            .get(b.bubble_id.as_str())
            .copied()
            .unwrap_or(usize::MAX);
        ra.cmp(&rb)
            .then_with(|| a.created_at().cmp(b.created_at()))
            .then_with(|| a.bubble_id.cmp(&b.bubble_id))
    });
}

/// Normalize one bubble into the `{role, text, toolCalls, toolResults, edits}`
/// record shape that [`parse`](CursorAdapter::parse) already understands. Total
/// and best-effort: an unrecognized bubble still yields a record (lossless).
fn normalize_bubble(composer_id: &str, bubble: &Bubble) -> Value {
    let obj = bubble.value.as_object();
    let bubble_type = obj
        .and_then(|o| o.get("type"))
        .and_then(json_as_i64)
        .unwrap_or(0);
    let role = match bubble_type {
        1 => "user",
        2 => "assistant",
        // Unknown bubble type → an unrecognized role; `parse_message` routes it
        // to Unknown, preserving losslessness.
        _ => "unknown",
    };

    let text = obj
        .and_then(|o| o.get("text"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            // `richText` is sometimes the only text carrier; only use it when
            // it's a plain string (its doc form is structured and not text).
            obj.and_then(|o| o.get("richText"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("")
        .to_string();

    let ts = obj
        .and_then(|o| o.get("createdAt"))
        .and_then(Value::as_str)
        .unwrap_or("1970-01-01T00:00:00Z")
        .to_string();

    let mut record = serde_json::Map::new();
    record.insert("id".into(), Value::String(bubble.bubble_id.clone()));
    record.insert("role".into(), Value::String(role.to_string()));
    record.insert("ts".into(), Value::String(ts));
    record.insert("sessionId".into(), Value::String(composer_id.to_string()));
    record.insert("composerId".into(), Value::String(composer_id.to_string()));
    record.insert("bubbleId".into(), Value::String(bubble.bubble_id.clone()));
    record.insert("text".into(), Value::String(text));

    // A bubble carries at most one tool call (Cursor stores one tool per
    // assistant bubble). When present it fans out to a ToolCall + ToolResult,
    // and — for an edit tool — a FileEdit.
    if let Some(tfd) = obj
        .and_then(|o| o.get("toolFormerData"))
        .and_then(Value::as_object)
    {
        let (calls, results, edits) = normalize_tool(tfd);
        if !calls.is_empty() {
            record.insert("toolCalls".into(), Value::Array(calls));
        }
        if !results.is_empty() {
            record.insert("toolResults".into(), Value::Array(results));
        }
        if !edits.is_empty() {
            record.insert("edits".into(), Value::Array(edits));
        }
    }

    Value::Object(record)
}

/// Turn a `toolFormerData` object into normalized `toolCalls`, `toolResults`,
/// and (for edit tools) `edits` arrays — the shape [`parse_message`] consumes.
fn normalize_tool(tfd: &serde_json::Map<String, Value>) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let name = tfd
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let call_id = tfd
        .get("toolCallId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            tfd.get("modelCallId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("toolcall:{name}"));

    // Args: prefer the model's raw args, else Cursor's resolved params. Both are
    // JSON-encoded strings; decode best-effort, else keep the raw string.
    let raw_args = decode_json_string(tfd.get("rawArgs"));
    let params = decode_json_string(tfd.get("params"));
    let args = match (raw_args.clone(), params.clone()) {
        (Some(a), _) if !a.is_null() => a,
        (_, Some(p)) if !p.is_null() => p,
        _ => Value::Null,
    };

    // Status: anything other than an explicit `error`/`cancelled` is treated as
    // successful (most rows are `completed`; `loading` is an in-flight call we
    // optimistically treat as ok rather than failed).
    let status = tfd.get("status").and_then(Value::as_str).unwrap_or("");
    let ok = !matches!(status, "error" | "cancelled");

    let result = decode_json_string(tfd.get("result")).unwrap_or(Value::Null);

    let calls = vec![json!({
        "id": call_id,
        "name": name,
        "args": args,
    })];
    let results = vec![json!({
        "id": call_id,
        "ok": ok,
        "output": result,
    })];

    // File edits: only edit-shaped tools yield a FileEdit. The path lives in the
    // decoded args; new content in `streamContent` when the store kept it.
    let mut edits = Vec::new();
    if is_edit_tool(&name) {
        if let Some(path) = edit_path(raw_args.as_ref(), params.as_ref()) {
            let new_text = raw_args
                .as_ref()
                .and_then(|a| a.get("streamContent"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut edit = serde_json::Map::new();
            edit.insert("path".into(), Value::String(path));
            edit.insert("callId".into(), Value::String(call_id.clone()));
            if let Some(nt) = new_text {
                edit.insert("newText".into(), Value::String(nt));
            }
            edits.push(Value::Object(edit));
        }
    }

    (calls, results, edits)
}

/// Whether a Cursor tool name denotes a file edit (so it yields a `FileEdit`).
fn is_edit_tool(name: &str) -> bool {
    // Cursor's edit family: `edit_file_v2`, `edit_file`, `search_replace`,
    // `write`/`create_file`, `apply_patch`, `reapply`. Match on substrings so
    // version suffixes (`_v2`, `_v3`) keep matching.
    let n = name;
    n.contains("edit_file")
        || n.contains("search_replace")
        || n == "write"
        || n.contains("create_file")
        || n.contains("apply_patch")
        || n.contains("apply_diff")
        || n == "reapply"
}

/// Extract the edited file path from decoded edit args. Tries `rawArgs.path`,
/// then `params.relativeWorkspacePath`, then a couple of common aliases.
fn edit_path(raw_args: Option<&Value>, params: Option<&Value>) -> Option<String> {
    let from = |v: Option<&Value>, key: &str| -> Option<String> {
        v.and_then(|o| o.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    from(raw_args, "path")
        .or_else(|| from(params, "relativeWorkspacePath"))
        .or_else(|| from(params, "path"))
        .or_else(|| from(raw_args, "relativeWorkspacePath"))
        .or_else(|| from(raw_args, "target_file"))
        .or_else(|| from(params, "targetFile"))
}

/// Decode a JSON-encoded **string** field (Cursor stores tool args/results as
/// stringified JSON). Returns the parsed value; if the field is a non-empty
/// string that isn't valid JSON, returns it as a `String` value; `None` when
/// the field is absent or an empty string.
fn decode_json_string(v: Option<&Value>) -> Option<Value> {
    let s = v?.as_str()?;
    if s.is_empty() {
        return None;
    }
    Some(serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string())))
}

/// Read `bytes` as `i64` whether the JSON had it as a number or a numeric
/// string (Cursor stores `type` as `"1"`/`"2"` in some builds, `1`/`2` in
/// others).
fn json_as_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// Escape `%`, `_`, and `\` for a SQLite `LIKE … ESCAPE '\'` prefix match.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Serialize a normalized record to a [`RawRecord`] with stable provenance.
fn record_for(file: &std::path::Path, line_no: u64, value: &Value) -> RawRecord {
    let line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    RawRecord::from_line(&line, SourceLocation::new(file, 0, line_no))
}

/// Fall back to reading a non-database path as line-delimited records (for the
/// exported-JSONL model and the on-disk fixtures).
fn read_lines(path: &std::path::Path) -> Result<Vec<RawRecord>, ParseError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| ParseError::Io(format!("reading {}: {e}", path.display())))?;
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        out.push(RawRecord::from_line(
            line,
            SourceLocation::new(path, 0, i as u64 + 1),
        ));
    }
    Ok(out)
}

// ===========================================================================
// Record parsing (normalized `{role, text, …}` shape — shared with exports)
// ===========================================================================

/// Parse a `session_start` header: seeds `ctx.project` and emits `SessionStart`.
fn parse_session_start(
    obj: &serde_json::Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
    let cwd_str = str_field(obj, "cwd").unwrap_or(".");
    let cwd = PathBuf::from(cwd_str);
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

/// Parse a `session_end` header into a `SessionEnd` event.
fn parse_session_end(
    obj: &serde_json::Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
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

/// Parse a dialogue record into the turn event plus any embedded tool calls,
/// tool results, and file edits — in a stable, deterministic order.
fn parse_message(
    obj: &serde_json::Map<String, Value>,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
) -> Vec<CaptureEvent> {
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
            parts: text_parts(obj),
        },
        "assistant" => EventKind::AssistantTurn {
            text,
            thinking: str_field(obj, "thinking").map(str::to_string),
            model: str_field(obj, "model").map(str::to_string),
            usage: parse_usage(obj.get("usage")),
            parts: text_parts(obj),
        },
        _ => {
            // A role we don't recognize → Unknown, but still keep ordering.
            EventKind::Unknown {
                raw_type: role.to_string(),
                raw: Value::Object(obj.clone()),
            }
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

    // 2) Tool calls. Each gets a synthetic, deterministic id derived from the
    //    turn id + the call id so it never collides with the turn or siblings.
    if let Some(calls) = obj.get("toolCalls").and_then(Value::as_array) {
        for (i, call) in calls.iter().enumerate() {
            let call_obj = match call.as_object() {
                Some(o) => o,
                None => continue,
            };
            let call_id = str_field(call_obj, "id")
                .map(str::to_string)
                .unwrap_or_else(|| format!("{base_id}:call:{i}"));
            let name = str_field(call_obj, "name").unwrap_or("").to_string();
            let args = call_obj.get("args").cloned().unwrap_or(Value::Null);
            // Remember the name so a later result can be paired by call_id.
            ctx.call_names.insert(call_id.clone(), name.clone());
            let ev_id = format!("{base_id}#toolcall:{call_id}");
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

    // 3) Tool results.
    if let Some(results) = obj.get("toolResults").and_then(Value::as_array) {
        for (i, res) in results.iter().enumerate() {
            let res_obj = match res.as_object() {
                Some(o) => o,
                None => continue,
            };
            let call_id = str_field(res_obj, "id")
                .map(str::to_string)
                .unwrap_or_else(|| format!("{base_id}:result:{i}"));
            let ok = bool_field(res_obj, "ok").unwrap_or(true);
            ctx.call_ok.insert(call_id.clone(), ok);
            let output = res_obj.get("output").cloned().unwrap_or(Value::Null);
            let ev_id = format!("{base_id}#toolresult:{call_id}");
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

    // 4) File edits.
    if let Some(edits) = obj.get("edits").and_then(Value::as_array) {
        for (i, edit) in edits.iter().enumerate() {
            let edit_obj = match edit.as_object() {
                Some(o) => o,
                None => continue,
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
            let call_id = str_field(edit_obj, "callId")
                .or_else(|| str_field(edit_obj, "call_id"))
                .map(str::to_string);
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

/// Build text/thinking [`Part`]s from a message (best-effort, never fails).
fn text_parts(obj: &serde_json::Map<String, Value>) -> Vec<Part> {
    let mut parts = Vec::new();
    if let Some(t) = str_field(obj, "text") {
        if !t.is_empty() {
            parts.push(Part::Text {
                text: t.to_string(),
            });
        }
    }
    if let Some(th) = str_field(obj, "thinking") {
        if !th.is_empty() {
            parts.push(Part::Thinking {
                text: th.to_string(),
            });
        }
    }
    parts
}

/// Parse `usage:{input,output}` (also tolerant of token-suffixed keys).
fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let obj = value?.as_object()?;
    let input = u64_field(obj, "input").or_else(|| u64_field(obj, "input_tokens"));
    let output = u64_field(obj, "output").or_else(|| u64_field(obj, "output_tokens"));
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: None,
        cache_creation_tokens: None,
    })
}

/// Parse a `{sha, branch}` git ref, if present.
fn parse_git(value: Option<&Value>) -> Option<GitRef> {
    let obj = value?.as_object()?;
    let sha = str_field(obj, "sha")?.to_string();
    Some(GitRef {
        sha,
        branch: str_field(obj, "branch").map(str::to_string),
    })
}

// ---- small, total field accessors (no panics, no indexing) ----

fn str_field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

fn bool_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    obj.get(key).and_then(Value::as_bool)
}

fn u64_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<u64> {
    obj.get(key).and_then(Value::as_u64)
}

fn u32_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<u32> {
    obj.get(key)
        .and_then(Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn parent_field(obj: &serde_json::Map<String, Value>) -> Option<String> {
    str_field(obj, "parentId")
        .or_else(|| str_field(obj, "parent_id"))
        .map(str::to_string)
}

/// The event id for a record: native `id`, else a stable content hash.
fn event_id_for(obj: &serde_json::Map<String, Value>, raw: &RawRecord) -> String {
    str_field(obj, "id")
        .map(str::to_string)
        .unwrap_or_else(|| content_id(&raw.bytes))
}

/// Timestamp from any of the common keys, falling back to the epoch.
fn ts_for(obj: &serde_json::Map<String, Value>) -> memscribe_core::Timestamp {
    util::ts_from(
        &Value::Object(obj.clone()),
        &["ts", "timestamp", "time", "created_at", "createdAt"],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(s: &str, line: u64) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("cursor.jsonl", 0, line))
    }

    /// Parse a whole JSONL string through one shared context (file order),
    /// returning the flattened event-kind tags.
    fn parse_all(jsonl: &str) -> (Vec<CaptureEvent>, ParseCtx) {
        let adapter = CursorAdapter;
        let mut ctx = ParseCtx::new();
        let mut events = Vec::new();
        for (i, line) in jsonl.lines().enumerate() {
            let r = raw(line, i as u64 + 1);
            let evs = adapter.parse(&r, &mut ctx).expect("parse never errors");
            events.extend(evs);
        }
        (events, ctx)
    }

    fn tags(events: &[CaptureEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.kind.tag()).collect()
    }

    #[test]
    fn session_start_then_decision_then_edit() {
        let jsonl = r#"{"kind":"session_start","sessionId":"s1","cwd":"/work/app","git":{"sha":"abc123","branch":"main"},"toolVersion":"0.42.0"}
{"id":"m1","role":"user","ts":"2026-06-22T10:00:00Z","sessionId":"s1","text":"Let's use Postgres instead of MySQL"}
{"id":"m2","parentId":"m1","role":"assistant","ts":"2026-06-22T10:00:05Z","sessionId":"s1","text":"Switching to Postgres.","model":"cursor-fast","usage":{"input":12,"output":7},"edits":[{"path":"db/config.toml","oldText":"engine=mysql","newText":"engine=postgres","diff":"@@\n-engine=mysql\n+engine=postgres","added":1,"removed":1}]}"#;
        let (events, ctx) = parse_all(jsonl);
        assert_eq!(
            tags(&events),
            vec!["session_start", "user_turn", "assistant_turn", "file_edit"]
        );
        // Session + project were learned from the header.
        assert_eq!(ctx.session_id.as_deref(), Some("s1"));
        assert_eq!(events[1].session_id, "s1");
        assert_eq!(events[1].project.cwd, PathBuf::from("/work/app"));
        // seq is monotonic from file order.
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        // The decision turn is a UserTurn carrying the text verbatim.
        match &events[1].kind {
            EventKind::UserTurn { text, .. } => {
                assert_eq!(text, "Let's use Postgres instead of MySQL");
            }
            other => panic!("expected user_turn, got {other:?}"),
        }
        // The FileEdit carries old/new/unified and line counts.
        match &events[3].kind {
            EventKind::FileEdit { diff, .. } => {
                assert_eq!(diff.path, PathBuf::from("db/config.toml"));
                assert_eq!(diff.old.as_deref(), Some("engine=mysql"));
                assert_eq!(diff.new.as_deref(), Some("engine=postgres"));
                assert_eq!(diff.added_lines, 1);
                assert_eq!(diff.removed_lines, 1);
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }

    #[test]
    fn assistant_usage_and_model_captured() {
        let jsonl = r#"{"id":"a1","role":"assistant","sessionId":"s","text":"hi","model":"cursor-pro","usage":{"input":5,"output":9}}"#;
        let (events, _) = parse_all(jsonl);
        match &events[0].kind {
            EventKind::AssistantTurn {
                model, usage, text, ..
            } => {
                assert_eq!(text, "hi");
                assert_eq!(model.as_deref(), Some("cursor-pro"));
                let u = usage.as_ref().expect("usage");
                assert_eq!(u.input_tokens, Some(5));
                assert_eq!(u.output_tokens, Some(9));
            }
            other => panic!("expected assistant_turn, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_then_result_pairing() {
        let jsonl = r#"{"id":"t1","role":"assistant","sessionId":"s","text":"running","toolCalls":[{"id":"c1","name":"shell","args":{"cmd":"ls"}}],"toolResults":[{"id":"c1","ok":true,"output":"a\nb"}]}"#;
        let (events, ctx) = parse_all(jsonl);
        assert_eq!(
            tags(&events),
            vec!["assistant_turn", "tool_call", "tool_result"]
        );
        assert_eq!(ctx.call_names.get("c1").map(String::as_str), Some("shell"));
        assert_eq!(ctx.call_ok.get("c1").copied(), Some(true));
        match &events[2].kind {
            EventKind::ToolResult { call_id, ok, .. } => {
                assert_eq!(call_id, "c1");
                assert!(*ok);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn failed_tool_result_is_marked_not_ok() {
        // Mirrors the tool_failure fixture: edit's result failed.
        let jsonl = r#"{"id":"f1","role":"assistant","sessionId":"s","text":"trying","toolCalls":[{"id":"e1","name":"edit","args":{"path":"x.rs"}}],"toolResults":[{"id":"e1","ok":false,"output":"permission denied"}],"edits":[{"path":"x.rs","oldText":"a","newText":"b","added":1,"removed":1}]}"#;
        let (events, ctx) = parse_all(jsonl);
        assert_eq!(
            tags(&events),
            vec!["assistant_turn", "tool_call", "tool_result", "file_edit"]
        );
        // The failed result is observable so downstream can suppress the Episode.
        assert_eq!(ctx.call_ok.get("e1").copied(), Some(false));
        match &events[2].kind {
            EventKind::ToolResult { ok, .. } => assert!(!*ok),
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn dedup_repeated_record_is_idempotent() {
        let line = r#"{"id":"dup","role":"user","sessionId":"s","text":"hello"}"#;
        let jsonl = format!("{line}\n{line}");
        let (events, _) = parse_all(&jsonl);
        // The second identical record yields nothing.
        assert_eq!(tags(&events), vec!["user_turn"]);
    }

    #[test]
    fn unrecognized_record_routes_to_unknown() {
        let jsonl = r#"{"kind":"telemetry_ping","payload":{"x":1}}
{"id":"w1","role":"wizard","sessionId":"s","text":"???"}"#;
        let (events, _) = parse_all(jsonl);
        // A record with neither a known kind nor a role → Unknown; a record with
        // an unknown role also degrades to Unknown rather than panicking.
        assert_eq!(tags(&events), vec!["unknown", "unknown"]);
    }

    #[test]
    fn garbage_never_panics() {
        let adapter = CursorAdapter;
        let mut ctx = ParseCtx::new();
        for bad in [
            "",
            "   ",
            "not json at all",
            "{",
            "[1,2,3]",
            "42",
            "true",
            "null",
            r#"{"role":42}"#,
            r#"{"kind":"session_start","git":"oops","cwd":12}"#,
            r#"{"id":"x","role":"user","toolCalls":"not-an-array","edits":{"nope":1}}"#,
        ] {
            let r = raw(bad, 1);
            let evs = adapter.parse(&r, &mut ctx).expect("never errors");
            // Blank lines produce nothing; everything else is lossless (>=1).
            if bad.trim().is_empty() {
                assert!(evs.is_empty());
            } else {
                assert!(!evs.is_empty(), "lossless for {bad:?}");
            }
        }
    }

    #[test]
    fn invalid_json_is_preserved_as_unknown() {
        let (events, _) = parse_all("this is not json");
        assert_eq!(tags(&events), vec!["unknown"]);
        match &events[0].kind {
            EventKind::Unknown { raw, .. } => {
                assert_eq!(raw, &Value::String("this is not json".to_string()));
            }
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn schema_fingerprint_detects_export() {
        let adapter = CursorAdapter;
        let hdr = raw(r#"{"kind":"session_start","cwd":"/x"}"#, 1);
        let msg = raw(r#"{"id":"m","role":"user","text":"hi"}"#, 2);
        let junk = raw(r#"{"kind":"telemetry"}"#, 3);
        assert_eq!(adapter.schema_fingerprint(&hdr).confidence, 100);
        assert_eq!(adapter.schema_fingerprint(&msg).confidence, 100);
        assert_eq!(adapter.schema_fingerprint(&junk).confidence, 0);
    }

    #[test]
    fn session_id_set_before_header_is_learned_from_record() {
        // Even without a header, the first record carrying sessionId seeds ctx.
        let jsonl = r#"{"id":"m1","role":"user","sessionId":"late","text":"hi"}"#;
        let (events, ctx) = parse_all(jsonl);
        assert_eq!(ctx.session_id.as_deref(), Some("late"));
        assert_eq!(events[0].session_id, "late");
    }

    #[test]
    fn store_reader_is_native() {
        assert_eq!(CursorAdapter.store_reader(), StoreReader::Native);
    }

    // ---- native SQLite reader, exercised against the REAL store schema ----
    //
    // These tests build an in-memory-shaped `.vscdb` on disk with the same
    // `cursorDiskKV` rows the live Cursor store uses (composerData + bubbleId),
    // then drive `read_native` → `parse` and assert the events. No live-store
    // dependency and no private data ship with the crate.

    /// Build a throwaway `.vscdb` containing the given `(key, value)` rows in
    /// `cursorDiskKV`, mirroring the real Cursor schema. Returns the temp path.
    fn build_store(rows: &[(&str, Value)]) -> std::path::PathBuf {
        // A unique, process-local temp file (deterministic enough for a test;
        // the adapter output is what must be deterministic, not the path).
        let mut path = std::env::temp_dir();
        let uniq = format!(
            "memscribe-cursor-test-{}-{}.vscdb",
            std::process::id(),
            content_id(rows.iter().map(|(k, _)| *k).collect::<String>().as_bytes())
        );
        path.push(uniq);
        let _ = std::fs::remove_file(&path);

        let conn = rusqlite::Connection::open(&path).expect("create temp vscdb");
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)",
            [],
        )
        .expect("create ItemTable");
        conn.execute(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)",
            [],
        )
        .expect("create cursorDiskKV");
        for (k, v) in rows {
            conn.execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                rusqlite::params![k, serde_json::to_string(v).unwrap()],
            )
            .expect("insert row");
        }
        drop(conn);
        path
    }

    /// Read a built store through the adapter and parse all records.
    fn read_and_parse(path: &std::path::Path) -> (Vec<CaptureEvent>, ParseCtx) {
        let adapter = CursorAdapter;
        let handle = TranscriptHandle {
            path: path.to_path_buf(),
            source: SRC,
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
    fn native_reader_extracts_turns_tools_and_edits_from_real_schema() {
        let cid = "11111111-2222-3333-4444-555555555555";
        let user_bub = "aaaaaaaa-0000-0000-0000-000000000001";
        let asst_bub = "bbbbbbbb-0000-0000-0000-000000000002";
        let edit_bub = "cccccccc-0000-0000-0000-000000000003";

        // composerData carries the canonical conversation order.
        let composer = json!({
            "composerId": cid,
            "name": "Switch DB engine",
            "createdAt": 1_780_000_000_000_i64,
            "fullConversationHeadersOnly": [
                {"bubbleId": user_bub, "type": 1},
                {"bubbleId": asst_bub, "type": 2},
                {"bubbleId": edit_bub, "type": 2}
            ]
        });
        // type 1 = user message.
        let user = json!({
            "type": 1,
            "bubbleId": user_bub,
            "createdAt": "2026-06-08T10:00:00.000Z",
            "text": "Use Postgres instead of MySQL"
        });
        // type 2 = assistant text message.
        let asst = json!({
            "type": 2,
            "bubbleId": asst_bub,
            "createdAt": "2026-06-08T10:00:05.000Z",
            "text": "Switching the engine to Postgres."
        });
        // type 2 = assistant tool bubble: an edit_file_v2 with inline content.
        let edit = json!({
            "type": 2,
            "bubbleId": edit_bub,
            "createdAt": "2026-06-08T10:00:09.000Z",
            "text": "",
            "toolFormerData": {
                "toolCallId": "tool_edit_1",
                "name": "edit_file_v2",
                "status": "completed",
                "rawArgs": "{\"path\":\"db/config.toml\",\"streamContent\":\"engine=postgres\"}",
                "params": "{\"relativeWorkspacePath\":\"db/config.toml\"}",
                "result": "{\"beforeContentId\":\"x\",\"afterContentId\":\"y\"}"
            }
        });

        let path = build_store(&[
            (&format!("composerData:{cid}"), composer),
            (&format!("bubbleId:{cid}:{user_bub}"), user),
            (&format!("bubbleId:{cid}:{asst_bub}"), asst),
            (&format!("bubbleId:{cid}:{edit_bub}"), edit),
        ]);

        let (events, ctx) = read_and_parse(&path);
        let _ = std::fs::remove_file(&path);

        // session_start, user_turn, assistant_turn, then the edit bubble fans
        // out to assistant_turn + tool_call + tool_result + file_edit.
        assert_eq!(
            tags(&events),
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "assistant_turn",
                "tool_call",
                "tool_result",
                "file_edit",
            ]
        );
        // Session bound from the composer id.
        assert_eq!(ctx.session_id.as_deref(), Some(cid));
        // User decision text recovered verbatim.
        match &events[1].kind {
            EventKind::UserTurn { text, .. } => {
                assert_eq!(text, "Use Postgres instead of MySQL");
            }
            other => panic!("expected user_turn, got {other:?}"),
        }
        // Tool call name + id recovered from toolFormerData.
        match &events[4].kind {
            EventKind::ToolCall { call_id, name, .. } => {
                assert_eq!(call_id, "tool_edit_1");
                assert_eq!(name, "edit_file_v2");
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
        // The edit recovers the path and the inline new content.
        match &events[6].kind {
            EventKind::FileEdit { call_id, diff } => {
                assert_eq!(call_id.as_deref(), Some("tool_edit_1"));
                assert_eq!(diff.path, PathBuf::from("db/config.toml"));
                assert_eq!(diff.new.as_deref(), Some("engine=postgres"));
            }
            other => panic!("expected file_edit, got {other:?}"),
        }
    }

    #[test]
    fn native_reader_marks_error_tool_results_not_ok() {
        let cid = "99999999-0000-0000-0000-000000000000";
        let bub = "dddddddd-0000-0000-0000-000000000001";
        let composer = json!({
            "composerId": cid,
            "name": "Failing edit",
            "createdAt": 1_780_000_000_000_i64,
            "fullConversationHeadersOnly": [{"bubbleId": bub, "type": 2}]
        });
        let failing = json!({
            "type": 2,
            "bubbleId": bub,
            "createdAt": "2026-06-08T10:00:00.000Z",
            "text": "",
            "toolFormerData": {
                "toolCallId": "tool_fail_1",
                "name": "run_terminal_command_v2",
                "status": "error",
                "params": "{\"command\":\"false\"}",
                "result": "{\"output\":\"boom\"}"
            }
        });
        let path = build_store(&[
            (&format!("composerData:{cid}"), composer),
            (&format!("bubbleId:{cid}:{bub}"), failing),
        ]);
        let (events, ctx) = read_and_parse(&path);
        let _ = std::fs::remove_file(&path);

        // status:error → the tool result is not ok.
        assert_eq!(ctx.call_ok.get("tool_fail_1").copied(), Some(false));
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::ToolResult { call_id, ok: false, .. } if call_id == "tool_fail_1"
        )));
    }

    #[test]
    fn native_reader_is_deterministic_across_row_order() {
        // Two composers + bubbles inserted in different orders must yield the
        // same event stream (read_native sorts by composerId then header order).
        let make = |order: &[usize]| -> Vec<(String, Value)> {
            let cid_a = "aaaa0000-0000-0000-0000-000000000000";
            let cid_b = "bbbb0000-0000-0000-0000-000000000000";
            let bub_a = "0000aaaa-0000-0000-0000-000000000001";
            let bub_b = "0000bbbb-0000-0000-0000-000000000001";
            let comp_a = json!({"composerId":cid_a,"name":"A","createdAt":1_i64,
                "fullConversationHeadersOnly":[{"bubbleId":bub_a,"type":1}]});
            let comp_b = json!({"composerId":cid_b,"name":"B","createdAt":2_i64,
                "fullConversationHeadersOnly":[{"bubbleId":bub_b,"type":1}]});
            let msg_a =
                json!({"type":1,"bubbleId":bub_a,"createdAt":"2026-06-08T10:00:00Z","text":"a"});
            let msg_b =
                json!({"type":1,"bubbleId":bub_b,"createdAt":"2026-06-08T10:00:00Z","text":"b"});
            let all = [
                (format!("composerData:{cid_b}"), comp_b),
                (format!("bubbleId:{cid_b}:{bub_b}"), msg_b),
                (format!("composerData:{cid_a}"), comp_a),
                (format!("bubbleId:{cid_a}:{bub_a}"), msg_a),
            ];
            order.iter().map(|&i| all[i].clone()).collect()
        };

        let texts = |events: &[CaptureEvent]| -> Vec<String> {
            events
                .iter()
                .filter_map(|e| match &e.kind {
                    EventKind::UserTurn { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };

        let rows1 = make(&[0, 1, 2, 3]);
        let rows2 = make(&[3, 2, 1, 0]);
        let r1: Vec<(&str, Value)> = rows1.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let r2: Vec<(&str, Value)> = rows2.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let p1 = build_store(&r1);
        let p2 = build_store(&r2);
        let (e1, _) = read_and_parse(&p1);
        let (e2, _) = read_and_parse(&p2);
        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);

        // composer A sorts before composer B regardless of insert order.
        assert_eq!(texts(&e1), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(texts(&e1), texts(&e2));
    }

    #[test]
    fn native_reader_reads_text_and_blob_value_storage() {
        // REGRESSION: the live Cursor store keeps `cursorDiskKV.value` as the
        // SQLite **TEXT** storage class, and `row.get::<Vec<u8>>` refuses to
        // coerce TEXT → bytes (it errors with InvalidColumnType). The old reader
        // swallowed that per-row error and silently produced ZERO records from
        // the real 1.34 GB store. `col_bytes` must read both TEXT and BLOB.
        let cid = "12121212-0000-0000-0000-000000000000";
        let bub = "34343434-0000-0000-0000-000000000001";
        let composer = json!({
            "composerId": cid,
            "createdAt": 1_780_000_000_000_i64,
            "fullConversationHeadersOnly": [{"bubbleId": bub, "type": 1}]
        });
        let user = json!({"type":1,"bubbleId":bub,"createdAt":"2026-06-08T10:00:00Z","text":"hello from TEXT/BLOB"});

        // Build a store where the composer row is stored as TEXT and the bubble
        // row as a real BLOB — proving both storage classes round-trip.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "memscribe-cursor-textblob-{}.vscdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).expect("create temp vscdb");
        conn.execute(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)",
            [],
        )
        .expect("create cursorDiskKV");
        // TEXT storage class (a Rust String binds as TEXT regardless of the
        // declared column affinity — exactly how Cursor stores it).
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![
                format!("composerData:{cid}"),
                serde_json::to_string(&composer).unwrap()
            ],
        )
        .expect("insert composer as TEXT");
        // BLOB storage class (bind raw bytes) for the bubble.
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![
                format!("bubbleId:{cid}:{bub}"),
                serde_json::to_vec(&user).unwrap()
            ],
        )
        .expect("insert bubble as BLOB");
        // Confirm the storage classes are actually TEXT and BLOB on disk.
        let comp_type: String = conn
            .query_row(
                "SELECT typeof(value) FROM cursorDiskKV WHERE key LIKE 'composerData:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let bub_type: String = conn
            .query_row(
                "SELECT typeof(value) FROM cursorDiskKV WHERE key LIKE 'bubbleId:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(comp_type, "text", "composer must be stored as TEXT");
        assert_eq!(bub_type, "blob", "bubble must be stored as BLOB");
        drop(conn);

        let (events, _) = read_and_parse(&path);
        let _ = std::fs::remove_file(&path);
        // Both rows were read: a session_start (from the TEXT composer) and a
        // user_turn (from the BLOB bubble).
        assert_eq!(tags(&events), vec!["session_start", "user_turn"]);
        match &events[1].kind {
            EventKind::UserTurn { text, .. } => assert_eq!(text, "hello from TEXT/BLOB"),
            other => panic!("expected user_turn, got {other:?}"),
        }
    }

    #[test]
    fn native_reader_binds_each_composer_to_its_own_session() {
        // REGRESSION: a single native store holds MANY composers. The session id
        // must track the current composer (rebind on each composer's records),
        // not latch the first one — otherwise every chat collapses into one
        // session and the segmenter bleeds conversations across unrelated chats.
        let cid_a = "aaaa1111-0000-0000-0000-000000000000";
        let cid_b = "bbbb2222-0000-0000-0000-000000000000";
        let bub_a = "1111aaaa-0000-0000-0000-000000000001";
        let bub_b = "2222bbbb-0000-0000-0000-000000000001";
        let comp_a = json!({"composerId":cid_a,"createdAt":1_i64,
            "fullConversationHeadersOnly":[{"bubbleId":bub_a,"type":1}]});
        let comp_b = json!({"composerId":cid_b,"createdAt":2_i64,
            "fullConversationHeadersOnly":[{"bubbleId":bub_b,"type":1}]});
        let msg_a = json!({"type":1,"bubbleId":bub_a,"createdAt":"2026-06-08T10:00:00Z","text":"in chat A"});
        let msg_b = json!({"type":1,"bubbleId":bub_b,"createdAt":"2026-06-08T10:00:00Z","text":"in chat B"});

        let path = build_store(&[
            (&format!("composerData:{cid_a}"), comp_a),
            (&format!("bubbleId:{cid_a}:{bub_a}"), msg_a),
            (&format!("composerData:{cid_b}"), comp_b),
            (&format!("bubbleId:{cid_b}:{bub_b}"), msg_b),
        ]);
        let (events, _) = read_and_parse(&path);
        let _ = std::fs::remove_file(&path);

        // Each user turn carries its own composer's session id.
        let a_turn = events
            .iter()
            .find(|e| matches!(&e.kind, EventKind::UserTurn { text, .. } if text == "in chat A"))
            .expect("chat A user turn");
        let b_turn = events
            .iter()
            .find(|e| matches!(&e.kind, EventKind::UserTurn { text, .. } if text == "in chat B"))
            .expect("chat B user turn");
        assert_eq!(a_turn.session_id, cid_a);
        assert_eq!(b_turn.session_id, cid_b);
        // And the session_start events bind to their composers too.
        let starts: Vec<&str> = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::SessionStart { .. }))
            .map(|e| e.session_id.as_str())
            .collect();
        assert_eq!(starts, vec![cid_a, cid_b]);
    }

    #[test]
    fn native_reader_missing_tables_is_empty_not_error() {
        // A `.vscdb` with no cursorDiskKV table → empty record set, no error.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "memscribe-cursor-empty-{}.vscdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT, value BLOB)", [])
            .unwrap();
        drop(conn);

        let adapter = CursorAdapter;
        let handle = TranscriptHandle {
            path: path.clone(),
            source: SRC,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).expect("no error");
        let _ = std::fs::remove_file(&path);
        assert!(records.is_empty());
    }

    #[test]
    fn native_reader_nonexistent_db_errors() {
        let adapter = CursorAdapter;
        let handle = TranscriptHandle {
            path: PathBuf::from("/nonexistent/does/not/exist.vscdb"),
            source: SRC,
            session_hint: None,
            compressed: false,
        };
        assert!(adapter.read_native(&handle).is_err());
    }

    // ---- on-disk fixture conformance ----
    //
    // The fixtures under `fixtures/cursor/v1/` are exported-transcript shapes
    // that feed the Phase-2 conformance suite. `read_native` falls back to
    // line-reading for non-`.vscdb` paths, so these still parse through the live
    // adapter, guaranteeing the export model and the adapter never drift apart.

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/cursor/v1")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    #[test]
    fn native_reader_falls_back_to_lines_for_jsonl() {
        // A `.jsonl` export path is line-read by read_native (not SQLite), then
        // parsed exactly as the exported model.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/cursor/v1/happy_path_decision_then_edits.jsonl");
        let adapter = CursorAdapter;
        let handle = TranscriptHandle {
            path: path.clone(),
            source: SRC,
            session_hint: None,
            compressed: false,
        };
        let records = adapter.read_native(&handle).expect("line fallback ok");
        assert!(!records.is_empty());
        let mut ctx = ParseCtx::new();
        let mut events = Vec::new();
        for r in &records {
            events.extend(adapter.parse(r, &mut ctx).expect("parse ok"));
        }
        assert_eq!(ctx.session_id.as_deref(), Some("cur-sess-001"));
        assert!(events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::FileEdit { .. })));
    }

    #[test]
    fn fixture_happy_path_decision_then_edits() {
        let (events, ctx) = parse_all(&fixture("happy_path_decision_then_edits.jsonl"));
        assert_eq!(ctx.session_id.as_deref(), Some("cur-sess-001"));
        // header, decision (user), assistant+call+result+edit, assistant+call+result+edit, end.
        assert_eq!(
            tags(&events),
            vec![
                "session_start",
                "user_turn",
                "assistant_turn",
                "tool_call",
                "tool_result",
                "file_edit",
                "assistant_turn",
                "tool_call",
                "tool_result",
                "file_edit",
                "session_end",
            ]
        );
        // The decision turn is the user's, and a FileEdit follows.
        match &events[1].kind {
            EventKind::UserTurn { text, .. } => assert!(text.contains("Postgres")),
            other => panic!("expected user_turn, got {other:?}"),
        }
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::FileEdit { diff, .. }
                if diff.path == std::path::Path::new("config/database.toml")
        )));
        // Project binding came from the session_start header.
        assert_eq!(
            events[1].project.cwd,
            PathBuf::from("/Users/dev/projects/orders-api")
        );
        // Every tool result in this fixture succeeded.
        assert!(ctx.call_ok.values().all(|ok| *ok));
    }

    #[test]
    fn fixture_rejected_alternative_parses() {
        let (events, _) = parse_all(&fixture("rejected_alternative.jsonl"));
        // Contains a user decision to reject Redux followed by an edit.
        assert!(events.iter().any(
            |e| matches!(&e.kind, EventKind::UserTurn { text, .. } if text.contains("reject"))
        ));
        assert!(events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::FileEdit { .. })));
    }

    #[test]
    fn fixture_ban_parses_decision_and_edit() {
        let (events, _) = parse_all(&fixture("ban.jsonl"));
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::UserTurn { text, .. } if text.contains("never")
        )));
        assert!(events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::FileEdit { .. })));
    }

    #[test]
    fn fixture_tool_failure_edit_has_failed_result() {
        // The edit's tool result FAILED → downstream must NOT mint an Episode.
        let (events, ctx) = parse_all(&fixture("tool_failure.jsonl"));
        let edit = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::FileEdit { call_id, diff } => Some((call_id.clone(), diff.clone())),
                _ => None,
            })
            .expect("an edit event");
        assert_eq!(edit.1.path, PathBuf::from("deploy.sh"));
        assert_eq!(edit.0.as_deref(), Some("call-edit-4"));
        assert_eq!(ctx.call_ok.get("call-edit-4").copied(), Some(false));
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::ToolResult { call_id, ok: false, .. } if call_id == "call-edit-4"
        )));
    }

    #[test]
    fn fixture_tool_failure_yields_no_episode_via_segmenter() {
        // End-to-end through the segmenter: the failed edit must NOT mint an
        // Episode, and the happy path must still mint two.
        use memscribe_core::gate::CommitmentGate;
        use memscribe_core::segmenter::{DefaultSegmenter, Segmenter};

        let gate = CommitmentGate::default();
        let seg = DefaultSegmenter;

        let (fail_events, _) = parse_all(&fixture("tool_failure.jsonl"));
        let fail_seg = seg.segment(&fail_events, &gate);
        assert_eq!(
            fail_seg.episodes.len(),
            0,
            "a failed edit must produce no episode"
        );

        let (ok_events, _) = parse_all(&fixture("happy_path_decision_then_edits.jsonl"));
        let ok_seg = seg.segment(&ok_events, &gate);
        assert_eq!(
            ok_seg.episodes.len(),
            2,
            "the happy path must still produce two episodes"
        );
    }

    #[test]
    fn all_fixtures_lossless_and_never_error() {
        for name in [
            "happy_path_decision_then_edits.jsonl",
            "rejected_alternative.jsonl",
            "ban.jsonl",
            "tool_failure.jsonl",
        ] {
            let (events, _) = parse_all(&fixture(name));
            assert!(!events.is_empty(), "{name} produced no events");
            assert!(
                events.iter().all(|e| e.kind.tag() != "unknown"),
                "{name} produced an Unknown event"
            );
        }
    }
}
