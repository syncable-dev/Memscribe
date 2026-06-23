//! The adapter layer contract (whitepaper §5).
//!
//! Each tool implements [`TranscriptAdapter`]: where its logs live, and how to
//! turn one raw record into normalized events. Parsers are **version-tolerant**:
//! they pattern-match on the fields they need and route anything unrecognized to
//! [`crate::model::EventKind::Unknown`] rather than failing the stream. A parser
//! **must never panic**.

use crate::error::ParseError;
use crate::model::{CaptureEvent, ProjectRef, SourceKind, SourceLocation};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// A raw, unparsed record as produced by a Source: a JSONL line, a hook stdin
/// blob, or an OTLP record — carrying the provenance needed to replay it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRecord {
    /// The raw bytes of the record.
    pub bytes: Vec<u8>,
    /// Where the record came from.
    pub location: SourceLocation,
}

impl RawRecord {
    /// Construct a raw record.
    pub fn new(bytes: impl Into<Vec<u8>>, location: SourceLocation) -> Self {
        RawRecord {
            bytes: bytes.into(),
            location,
        }
    }

    /// The record as UTF-8 text, if valid.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    /// Construct from a string and a location (convenience for tests).
    pub fn from_line(line: &str, location: SourceLocation) -> Self {
        RawRecord::new(line.as_bytes().to_vec(), location)
    }
}

/// Mutable per-session context threaded through parsing. It assigns the
/// monotonic `seq`, dedups by `event_id`, resolves tool-call/result pairing by
/// `call_id`, and carries the session-start project binding.
#[derive(Clone, Debug, Default)]
pub struct ParseCtx {
    /// The session id, set once known.
    pub session_id: Option<String>,
    /// The next sequence number to assign.
    pub next_seq: u64,
    /// Event ids already emitted (for dedup / idempotency).
    pub seen_event_ids: HashSet<String>,
    /// The project binding captured at session start.
    pub project: Option<ProjectRef>,
    /// Map of `call_id` → tool name, for pairing calls with results/edits.
    pub call_names: HashMap<String, String>,
    /// Map of `call_id` → success flag, from observed tool results.
    pub call_ok: HashMap<String, bool>,
}

impl ParseCtx {
    /// A fresh context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next monotonic sequence number for this session.
    pub fn alloc_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    /// Record an `event_id` as seen; returns `true` if it was new (not a dup).
    pub fn first_seen(&mut self, event_id: &str) -> bool {
        self.seen_event_ids.insert(event_id.to_string())
    }

    /// The project ref to stamp on an event, defaulting to the cwd.
    #[must_use]
    pub fn project_or_default(&self) -> ProjectRef {
        self.project
            .clone()
            .unwrap_or_else(|| ProjectRef::from_cwd("."))
    }
}

/// Where a tool's transcripts live and how to discover them.
#[derive(Clone, Debug, Default)]
pub struct DiscoverCfg {
    /// Override for `$HOME` (used by tests and sandboxes).
    pub home: Option<PathBuf>,
    /// Per-tool path overrides (e.g. `CODEX_HOME`, `CLAUDE_CONFIG_DIR`).
    pub overrides: HashMap<String, PathBuf>,
    /// Restrict discovery to a single project root, if set.
    pub project_filter: Option<PathBuf>,
}

impl DiscoverCfg {
    /// The effective home directory (override, then `$HOME`, then `.`).
    #[must_use]
    pub fn home_dir(&self) -> PathBuf {
        if let Some(h) = &self.home {
            return h.clone();
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// A discovered transcript file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptHandle {
    /// The transcript file path.
    pub path: PathBuf,
    /// The tool that produced it.
    pub source: SourceKind,
    /// A session-id hint derived from the path, if any.
    pub session_hint: Option<String>,
    /// Whether the file is zstd-compressed (e.g. Codex cold rollouts).
    pub compressed: bool,
}

/// The result of fingerprinting a raw record, so the corpus and runtime can
/// version-gate the parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaVariant {
    /// The tool the variant belongs to.
    pub source: SourceKind,
    /// A variant identifier (e.g. `claude_code/2.1`, `codex/rollout-v2`).
    pub variant: String,
    /// Confidence 0..=100 that the fingerprint is correct.
    pub confidence: u8,
}

impl SchemaVariant {
    /// A variant with full confidence.
    #[must_use]
    pub fn certain(source: SourceKind, variant: impl Into<String>) -> Self {
        SchemaVariant {
            source,
            variant: variant.into(),
            confidence: 100,
        }
    }

    /// An unknown variant (zero confidence).
    #[must_use]
    pub fn unknown(source: SourceKind) -> Self {
        SchemaVariant {
            source,
            variant: "unknown".to_string(),
            confidence: 0,
        }
    }
}

/// How an adapter's discovered store is read into [`RawRecord`]s. Most tools
/// write newline-delimited transcripts ([`StoreReader::LineDelimited`], read by
/// `memscribe-io`); some keep their conversation in a database
/// ([`StoreReader::Native`], read by the adapter's own [`TranscriptAdapter::read_native`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreReader {
    /// Newline-delimited records — the common case (the io file reader applies).
    LineDelimited,
    /// A non-line store (e.g. a SQLite database) the adapter reads itself.
    Native,
}

/// Each tool implements this trait. See the module docs for the contract.
pub trait TranscriptAdapter: Send + Sync {
    /// The tool this adapter handles.
    fn source_kind(&self) -> SourceKind;

    /// Locate live & historical transcripts (handles globbing, project hashing,
    /// rotation, `.zst`).
    fn discover(&self, cfg: &DiscoverCfg) -> Vec<TranscriptHandle>;

    /// Parse ONE raw record into zero or more normalized events. Must never
    /// panic; unknowns route to [`crate::model::EventKind::Unknown`].
    fn parse(&self, raw: &RawRecord, ctx: &mut ParseCtx) -> Result<Vec<CaptureEvent>, ParseError>;

    /// Fingerprint a sample record so the corpus and runtime can version-gate
    /// the parser.
    fn schema_fingerprint(&self, sample: &RawRecord) -> SchemaVariant;

    /// How this adapter's store is read. Defaults to line-delimited files; a
    /// database-backed adapter (Cursor, Zed) overrides this to
    /// [`StoreReader::Native`].
    fn store_reader(&self) -> StoreReader {
        StoreReader::LineDelimited
    }

    /// Extract raw records from a non-line store (only called when
    /// [`store_reader`](TranscriptAdapter::store_reader) is [`StoreReader::Native`]).
    /// This is the adapter's I/O boundary — it opens the database in `handle`
    /// and yields one [`RawRecord`] per logical message/event, which [`parse`]
    /// then consumes purely. The default errors, since line-delimited adapters
    /// never reach it.
    ///
    /// # Errors
    /// Returns a [`ParseError`] if the store cannot be opened or read.
    fn read_native(&self, handle: &TranscriptHandle) -> Result<Vec<RawRecord>, ParseError> {
        let _ = handle;
        Err(ParseError::Io(
            "this adapter reads line-delimited files; read_native is not implemented".to_string(),
        ))
    }
}
