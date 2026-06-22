//! # memscribe-testkit
//!
//! The test harness that makes Memscribe's determinism a property you can run.
//! It provides:
//!
//! - [`parse_events`] / [`prepare_nodes`] — drive a tool's adapter and the
//!   pipeline over raw bytes, the way every golden and property test does.
//! - [`invariants`] — reusable checks for the whitepaper §8.3 invariants
//!   (determinism, monotonic seq, losslessness, idempotency).
//! - [`golden`] — fixture path resolution and load/compare helpers.
//! - [`scenarios`] — the cross-tool conformance scenario catalog (§8.2).
#![forbid(unsafe_code)]

pub mod golden;
pub mod invariants;
pub mod scenarios;

use memscribe_adapters::adapter_for;
use memscribe_core::{
    pipeline::parse_records, CaptureEvent, DefaultPipeline, PreparedNode, SourceKind,
};
use memscribe_io::read_records_from_bytes;
use std::path::Path;

/// Parse a tool's transcript bytes into the normalized event stream.
///
/// # Panics
/// Panics if the adapter for `tool` is not compiled into the build.
#[must_use]
pub fn parse_events(tool: SourceKind, jsonl: &[u8], path: &Path) -> Vec<CaptureEvent> {
    let recs = read_records_from_bytes(jsonl, path);
    let adapter = adapter_for(tool).expect("adapter feature must be enabled for this tool");
    let (events, _ctx) = parse_records(adapter.as_ref(), &recs);
    events
}

/// Parse and prepare a tool's transcript bytes into the prepared-node stream
/// (redaction off, so tests can assert on verbatim content).
///
/// # Panics
/// Panics if the adapter for `tool` is not compiled into the build.
#[must_use]
pub fn prepare_nodes(tool: SourceKind, jsonl: &[u8], path: &Path) -> Vec<PreparedNode> {
    let recs = read_records_from_bytes(jsonl, path);
    let adapter = adapter_for(tool).expect("adapter feature must be enabled for this tool");
    DefaultPipeline::without_redaction().run_records(adapter.as_ref(), &recs)
}

/// Count non-blank lines — the lower bound on events for the losslessness check.
#[must_use]
pub fn count_nonblank_lines(jsonl: &[u8]) -> usize {
    String::from_utf8_lossy(jsonl)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}
