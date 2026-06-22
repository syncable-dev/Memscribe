//! Fuzz target for the Claude Code adapter parser (whitepaper §8.4).
//!
//! Feeds arbitrary bytes to `ClaudeCodeAdapter::parse` through a `RawRecord`
//! with a fresh `ParseCtx`, asserting the parser never panics and terminates.
//! The libFuzzer harness turns any panic into a crash artifact.
#![cfg_attr(fuzzing, no_main)]

use memscribe_adapters::claude_code::ClaudeCodeAdapter;
use memscribe_core::model::SourceLocation;
use memscribe_core::{ParseCtx, RawRecord, TranscriptAdapter};

/// Drive one fuzz input through the adapter. A parser is allowed to return
/// `Ok(events)` or `Err(ParseError)`; the only contract a fuzz run enforces is
/// that it neither panics nor diverges. We deliberately ignore the result.
#[inline]
fn run(data: &[u8]) {
    let adapter = ClaudeCodeAdapter;
    let loc = SourceLocation::new("fuzz://claude_code", 0, 1);
    let raw = RawRecord::new(data.to_vec(), loc);
    let mut ctx = ParseCtx::new();
    let _ = adapter.parse(&raw, &mut ctx);
    // Fingerprinting shares the same parse-tolerance contract.
    let _ = adapter.schema_fingerprint(&raw);
}

#[cfg(fuzzing)]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    run(data);
});

// Plain `cargo build` (no `--cfg fuzzing`): a tiny stub `main` so the target
// compiles and links on stable without the libFuzzer runtime, and exercises the
// same code path once on an empty input.
#[cfg(not(fuzzing))]
fn main() {
    run(b"");
}
