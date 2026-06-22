//! Fuzz target for the OTel adapter parser (whitepaper §8.4).
//!
//! Feeds arbitrary bytes to `OtelAdapter::parse` through a `RawRecord` with a
//! fresh `ParseCtx`, asserting the parser never panics and terminates. The
//! libFuzzer harness turns any panic into a crash artifact.
#![cfg_attr(fuzzing, no_main)]

use memscribe_adapters::otel::OtelAdapter;
use memscribe_core::model::SourceLocation;
use memscribe_core::{ParseCtx, RawRecord, TranscriptAdapter};

#[inline]
fn run(data: &[u8]) {
    let adapter = OtelAdapter;
    let loc = SourceLocation::new("fuzz://otel", 0, 1);
    let raw = RawRecord::new(data.to_vec(), loc);
    let mut ctx = ParseCtx::new();
    let _ = adapter.parse(&raw, &mut ctx);
    let _ = adapter.schema_fingerprint(&raw);
}

#[cfg(fuzzing)]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    run(data);
});

#[cfg(not(fuzzing))]
fn main() {
    run(b"");
}
