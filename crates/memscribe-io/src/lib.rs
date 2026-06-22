//! # memscribe-io
//!
//! The source layer. A `Source` is conceptually just a stream of
//! [`memscribe_core::RawRecord`]s; the rest of the pipeline does not know or
//! care which source produced the bytes.
//!
//! - [`records`] — read a transcript file (or bytes) into `RawRecord`s with
//!   exact byte/line provenance, transparently decompressing `.zst`. This is the
//!   one-shot reader used by `memscribe parse` and the test harness.
//! - [`tailer`] — the live, notify-based file tailer with a persisted byte-offset
//!   cursor so restarts resume exactly where they left off (feature `watch`).
//! - [`hook`] — the hook handler agents invoke (reads event JSON on stdin,
//!   records it, returns immediately).
//! - [`discover`] — walk a directory tree for transcript files by extension,
//!   reporting which are `.zst` cold rollouts.
//! - [`cursor_store`] — the persisted offset store (feature `cursor-store`).
//! - [`otlp`] — a loopback-only HTTP OTLP receiver that ingests pushed
//!   OpenTelemetry GenAI records into `RawRecord`s (feature `otlp`). Off by
//!   default so the default build opens no network ports.
#![forbid(unsafe_code)]

pub mod cursor_store;
pub mod discover;
pub mod hook;
#[cfg(feature = "otlp")]
pub mod otlp;
pub mod records;
pub mod tailer;

pub use records::{read_records, read_records_from_bytes};
pub use tailer::poll_new_records;
