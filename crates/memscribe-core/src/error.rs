//! Error types for the deterministic pipeline. Adapters return [`ParseError`]
//! only for genuinely malformed input; unrecognized-but-well-formed records are
//! routed to [`crate::model::EventKind::Unknown`], never an error.

use thiserror::Error;

/// A parse failure. Adapters must reserve this for malformed bytes — anything
/// merely *unrecognized* becomes [`crate::model::EventKind::Unknown`] so the
/// stream stays lossless and version-tolerant.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The record could not be parsed at all (e.g. invalid UTF-8 / broken JSON).
    #[error("malformed record at {location}: {message}")]
    Malformed { location: String, message: String },
    /// JSON deserialization failed.
    #[error("json error: {0}")]
    Json(String),
    /// The schema variant is recognized but not supported by this adapter.
    #[error("unsupported schema variant: {0}")]
    UnsupportedSchema(String),
    /// An I/O problem occurred while reading the record.
    #[error("io error: {0}")]
    Io(String),
}

/// A failure while writing to a [`crate::sink::Sink`].
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SinkError {
    /// The underlying write failed.
    #[error("sink write failed: {0}")]
    Write(String),
    /// Flushing buffered nodes failed.
    #[error("sink flush failed: {0}")]
    Flush(String),
    /// A node could not be serialized.
    #[error("serialization failed: {0}")]
    Serialize(String),
}

/// A top-level pipeline failure, wrapping a parse or sink error.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// A record failed to parse.
    #[error(transparent)]
    Parse(#[from] ParseError),
    /// A node failed to reach the sink.
    #[error(transparent)]
    Sink(#[from] SinkError),
}
