//! Non-nightly robustness suite for every adapter parser (whitepaper §8.4).
//!
//! The `fuzz/` crate gives us coverage-guided fuzzing, but it needs a nightly
//! toolchain and `cargo-fuzz`. This suite is its workspace-resident, stable-Rust
//! counterpart so CI gets robustness value on every run: it feeds arbitrary and
//! deliberately *mutated* bytes — random noise, truncated JSON, deeply nested
//! JSON, gigantic numbers, invalid UTF-8 — to **every** adapter's `parse()` and
//! asserts the §8.4 parser contract:
//!
//! 1. **No panic.** A parser must never panic on any input (we wrap each call in
//!    [`std::panic::catch_unwind`]).
//! 2. **Bounded time.** A parser must terminate; we run it on a worker thread and
//!    fail if it does not finish within a generous wall-clock budget.
//! 3. **Stream survival.** A single malformed line must be *skipped* (an `Err`
//!    that [`parse_records`] drops) or routed to `Unknown` — never abort the
//!    surrounding stream. We sandwich a malformed line between two well-formed
//!    ones and assert the good events still come through.
//!
//! Every check runs against all nine adapters via [`adapter_for`].

use memscribe_adapters::adapter_for;
use memscribe_core::model::SourceLocation;
use memscribe_core::pipeline::parse_records;
use memscribe_core::{ParseCtx, RawRecord, SourceKind, TranscriptAdapter};
use proptest::prelude::*;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// The nine adapters this suite must cover, in registry order.
const ALL_KINDS: &[SourceKind] = &[
    SourceKind::ClaudeCode,
    SourceKind::Codex,
    SourceKind::Gemini,
    SourceKind::Otel,
    SourceKind::Cursor,
    SourceKind::Windsurf,
    SourceKind::Zed,
    SourceKind::VsCode,
    SourceKind::Copilot,
];

/// Wall-clock budget for a single `parse()` call on one record. Parsing one
/// line of even pathological JSON is microseconds of work; a multi-second
/// budget only ever trips on a genuine hang or runaway recursion.
const PARSE_BUDGET: Duration = Duration::from_secs(5);

/// Resolve the adapter for `kind`, asserting its feature is compiled in (the
/// default workspace build enables all nine).
fn adapter(kind: SourceKind) -> Box<dyn TranscriptAdapter> {
    adapter_for(kind).unwrap_or_else(|| panic!("adapter feature for {kind} must be enabled"))
}

/// Drive one record through one adapter, enforcing **no panic** and **bounded
/// time**. Returns nothing — a violation is a test failure, not a value.
///
/// The parse runs on a dedicated worker thread so a non-terminating parser can
/// be detected via a receive timeout instead of hanging the whole test binary.
/// The worker re-creates the adapter from `kind` (adapters are zero-sized unit
/// structs, so this is free) to keep everything `Send`.
fn assert_parse_is_safe(kind: SourceKind, bytes: Vec<u8>) {
    let (tx, rx) = mpsc::channel::<()>();
    let worker = thread::spawn(move || {
        let adapter = adapter(kind);
        let loc = SourceLocation::new("robustness://input", 0, 1);
        let raw = RawRecord::new(bytes, loc);
        let mut ctx = ParseCtx::new();
        // A panic inside `parse` is caught here so the worker thread always
        // sends its completion signal; we convert it into an explicit failure.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _ = adapter.parse(&raw, &mut ctx);
            let _ = adapter.schema_fingerprint(&raw);
        }));
        // Ignore send errors: if the receiver already timed out and went away,
        // the main thread has already failed the test.
        let _ = tx.send(());
        outcome
    });

    match rx.recv_timeout(PARSE_BUDGET) {
        Ok(()) => {
            // The worker finished in time; surface any panic it caught.
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(_)) => panic!("{kind} adapter panicked while parsing a mutated record"),
                Err(_) => panic!("{kind} adapter worker thread itself panicked"),
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("{kind} adapter did not terminate within {PARSE_BUDGET:?} on a mutated record");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{kind} adapter worker thread vanished without completing");
        }
    }
}

// --- Adversarial corpus -----------------------------------------------------

/// A minimal, well-formed JSON object. Not a valid record for any specific
/// adapter, but well-formed enough that a tolerant parser routes it to
/// `Unknown` (or `Ok([])`) rather than erroring — our "good anchor" line for the
/// stream-survival check.
const GOOD_ANCHOR: &[u8] = br#"{"type":"unknown_but_well_formed","v":1}"#;

/// Deterministic adversarial inputs that every adapter must survive. These are
/// the named mutation classes from the task, materialized as concrete bytes so
/// the corpus is reproducible and reviewable.
fn adversarial_corpus() -> Vec<(&'static str, Vec<u8>)> {
    // Valid JSON prefix followed by an invalid UTF-8 tail.
    let json_with_invalid_utf8 = {
        let mut v = br#"{"text":""#.to_vec();
        v.extend_from_slice(&[0xff, 0xff]);
        v.extend_from_slice(br#""}"#);
        v
    };

    vec![
        // Empty and whitespace-only.
        ("empty", Vec::new()),
        ("whitespace", b"   \t  ".to_vec()),
        // Random / non-JSON bytes.
        ("random_ascii", b"not json at all, just words".to_vec()),
        ("control_bytes", vec![0x00, 0x01, 0x02, 0x07, 0x1b, 0x7f]),
        // Invalid UTF-8 (only expressible via raw bytes — `as_str()` must
        // return None and the parser must still not panic).
        ("invalid_utf8", vec![0xff, 0xfe, 0xfd, 0xc0, 0x80]),
        ("lone_surrogate_bytes", vec![0xed, 0xa0, 0x80]),
        ("json_with_invalid_utf8", json_with_invalid_utf8),
        // Truncated JSON in several shapes.
        (
            "truncated_object",
            br#"{"type":"user","message":{"role":"#.to_vec(),
        ),
        ("truncated_string", br#"{"text":"unterminated"#.to_vec()),
        ("truncated_array", b"[1,2,3".to_vec()),
        ("dangling_comma", br#"{"a":1,}"#.to_vec()),
        ("just_open_brace", b"{".to_vec()),
        // Deeply nested JSON — exercises any recursive descent for stack safety.
        ("deep_array", deep_nested_array(2_000)),
        ("deep_object", deep_nested_object(2_000)),
        // Huge numbers — beyond i64/u64/f64 range and absurd precision.
        (
            "huge_integer",
            format!(r#"{{"n":{}}}"#, "9".repeat(400)).into_bytes(),
        ),
        ("huge_exponent", br#"{"n":1e400,"m":-1e-400}"#.to_vec()),
        (
            "huge_precision",
            format!(r#"{{"n":0.{}}}"#, "1".repeat(500)).into_bytes(),
        ),
        (
            "huge_timestamp",
            br#"{"timestamp":99999999999999999999,"ts":"+999999-01-01T00:00:00Z"}"#.to_vec(),
        ),
        // A plausible-but-wrong record: right-shaped keys, garbage values.
        (
            "type_confusion",
            br#"{"type":12345,"message":[],"timestamp":true,"usage":"nope"}"#.to_vec(),
        ),
        // A long single line, to make sure nothing is quadratic enough to time
        // out.
        (
            "long_text",
            format!(r#"{{"text":"{}"}}"#, "a".repeat(50_000)).into_bytes(),
        ),
    ]
}

/// `[[[...]]]` nested `depth` deep — a stack-depth stressor for recursive
/// JSON parsers. `serde_json` has its own recursion limit; the point is the
/// adapter must not panic regardless of how `serde_json` reports it.
fn deep_nested_array(depth: usize) -> Vec<u8> {
    let mut s = String::with_capacity(depth * 2 + 2);
    for _ in 0..depth {
        s.push('[');
    }
    for _ in 0..depth {
        s.push(']');
    }
    s.into_bytes()
}

/// `{"a":{"a":{...}}}` nested `depth` deep.
fn deep_nested_object(depth: usize) -> Vec<u8> {
    let mut s = String::new();
    for _ in 0..depth {
        s.push_str(r#"{"a":"#);
    }
    s.push('1');
    for _ in 0..depth {
        s.push('}');
    }
    s.into_bytes()
}

// --- Tests ------------------------------------------------------------------

/// Every adapter survives every deterministic adversarial input: no panic,
/// bounded time. This is the reproducible core of the suite.
#[test]
fn every_adapter_survives_adversarial_corpus() {
    for &kind in ALL_KINDS {
        for (name, bytes) in adversarial_corpus() {
            // A fresh clone per call; `assert_parse_is_safe` takes ownership and
            // moves the bytes onto the worker thread.
            let label = format!("{kind}/{name}");
            assert_parse_is_safe(kind, bytes);
            // Touch `label` so a failing case is identifiable in a backtrace via
            // the panic message above; this also keeps the binding live.
            let _ = label;
        }
    }
}

/// A malformed line sandwiched between two well-formed ones must not abort the
/// stream: [`parse_records`] skips the bad record and keeps going. We assert the
/// good anchors still produce at least as many events as a clean two-line run,
/// proving the malformed middle line neither erased earlier output nor stopped
/// later parsing.
#[test]
fn malformed_line_is_skipped_not_fatal_for_every_adapter() {
    let path = Path::new("robustness://stream");
    for &kind in ALL_KINDS {
        let a = adapter(kind);

        // Baseline: two good anchor lines, no malformed middle.
        let clean = records_from_lines(&[GOOD_ANCHOR, GOOD_ANCHOR], path);
        let (clean_events, _) = parse_records(a.as_ref(), &clean);

        for (name, bad) in adversarial_corpus() {
            // good, MALFORMED, good — the malformed line must not take the
            // surrounding records down with it.
            let mixed = records_from_lines(&[GOOD_ANCHOR, &bad, GOOD_ANCHOR], path);

            // The whole stream must parse without panicking.
            let result = catch_unwind(AssertUnwindSafe(|| parse_records(a.as_ref(), &mixed)));
            let (mixed_events, _) = result.unwrap_or_else(|_| {
                panic!("{kind} aborted the stream on a malformed `{name}` line")
            });

            // Stream survival: the two good anchors still produced their events;
            // the malformed middle did not erase or block them.
            assert!(
                mixed_events.len() >= clean_events.len(),
                "{kind}: malformed `{name}` line suppressed good events \
                 (clean={}, mixed={})",
                clean_events.len(),
                mixed_events.len(),
            );
        }
    }
}

/// Build a single multi-line transcript from the given line byte-slices and
/// split it back into records exactly the way the real reader does, so the
/// per-record provenance (offsets, line numbers) matches production.
fn records_from_lines(lines: &[&[u8]], path: &Path) -> Vec<RawRecord> {
    let mut buf: Vec<u8> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            buf.push(b'\n');
        }
        buf.extend_from_slice(line);
    }
    memscribe_io::read_records_from_bytes(&buf, path)
}

proptest! {
    // Keep the case count modest: each case spawns a worker thread per adapter,
    // so 64 cases × 9 adapters is a few hundred guarded parses — plenty of
    // coverage without making the suite slow.
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Arbitrary byte vectors (including invalid UTF-8) never panic or hang any
    /// adapter. This is the property-based analogue of the cargo-fuzz targets.
    #[test]
    fn arbitrary_bytes_never_panic_any_adapter(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        for &kind in ALL_KINDS {
            assert_parse_is_safe(kind, bytes.clone());
        }
    }

    /// Mutated-JSON inputs: start from a well-formed-ish object and let proptest
    /// splice random bytes in, exercising the "almost valid" region of the input
    /// space that pure-random bytes rarely reach.
    #[test]
    fn mutated_json_never_panics_any_adapter(
        seed in proptest::collection::vec(any::<u8>(), 0..64),
        cut in 0usize..40,
    ) {
        let mut bytes = br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-06-22T10:00:00Z"}"#.to_vec();
        // Truncate at an arbitrary point, then append random noise: a cheap way
        // to manufacture truncated-then-garbled JSON.
        let at = cut.min(bytes.len());
        bytes.truncate(at);
        bytes.extend_from_slice(&seed);
        for &kind in ALL_KINDS {
            assert_parse_is_safe(kind, bytes.clone());
        }
    }
}
