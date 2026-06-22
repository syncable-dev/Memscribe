//! Whitepaper §8.3 invariants, expressed as `proptest` properties.
//!
//! These complement the per-stage unit tests and the golden-file suite: instead
//! of fixed inputs they assert that the §8.3 invariants hold for *arbitrary*
//! generated input — both plausible JSONL records and raw arbitrary bytes.
//!
//! The invariants checked here:
//!
//! 1. **Determinism** — `parse` / `prepare` are byte-identical across runs and
//!    depend only on their input (pure functions, thread-independent).
//! 2. **Idempotency by `event_id`** — concatenating a *uniquely-identified*
//!    record stream with itself through one parse pass dedups back to the
//!    single-pass event set (adapters dedup recognized records via
//!    `ctx.first_seen`).
//! 3. **Monotonic `seq`** — strictly increasing per session, for any stream.
//! 4. **Losslessness** — every distinct non-blank record yields `>= 1` event.
//! 5. **Gate purity** — `CommitmentGate::evaluate` depends only on the text, is
//!    repeatable, and is independent of evaluation order/context.
//! 6. **Offset resumption** — splitting a buffer at a newline boundary and
//!    concatenating the two reads equals reading the whole, at the byte level.
//!
//! A note on the dedup/losslessness contract (ground truth in the adapters):
//! recognized records are deduplicated once on their native id (via
//! `ctx.first_seen`), while *unrecognized* records are routed to
//! `EventKind::Unknown` and preserved verbatim **without** dedup — losslessness
//! for unknown data outranks idempotency. Two records with identical bytes
//! therefore collapse to one event when recognized (same content-hash fallback
//! id) but are both retained when unknown. To test idempotency and losslessness
//! against a well-defined record identity, the generators below stamp a unique
//! native id on every record, so distinct records never collide.

use memscribe_adapters::adapter_for;
use memscribe_core::{pipeline::parse_records, CaptureEvent, CommitmentGate, SourceKind};
use memscribe_io::read_records_from_bytes;
use memscribe_testkit::golden::{discover_cases, GoldenCase};
use memscribe_testkit::invariants::{
    check_determinism, check_lossless, check_monotonic_seq, check_unique_event_ids,
};
use memscribe_testkit::{count_nonblank_lines, parse_events};
use proptest::prelude::*;
use std::path::Path;

// ---------------------------------------------------------------------------
// Strategies: plausible JSONL records and raw arbitrary bytes.
// ---------------------------------------------------------------------------

/// Plausible turn text, including the commitment-marker vocabulary so the gate
/// and segmenter get exercised, plus arbitrary free text.
fn turn_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("let's go with Postgres for storage".to_string()),
        Just("use Stripe instead of PayPal".to_string()),
        Just("we will never add a dependency on left-pad".to_string()),
        Just("we must always use prepared statements".to_string()),
        Just("remember that the cache TTL is 60s".to_string()),
        Just("thanks, that looks good".to_string()),
        // Arbitrary printable-ish text, to stress the gate/segmenter spans.
        "[a-zA-Z0-9 ,.!?'\\-]{0,80}".prop_map(|s| s),
    ]
}

/// A Claude Code record shape (`type`/`uuid`/`message`). Claude Code is the
/// reference adapter for these properties because it recognizes the shape (so
/// records are not all Unknown) and deduplicates once on `uuid` — giving every
/// record a well-defined, stable identity. The `idx` makes the `uuid` unique so
/// distinct records never collide on the content-hash fallback.
fn claude_record(idx: usize) -> impl Strategy<Value = serde_json::Value> {
    (prop_oneof![Just("user"), Just("assistant")], turn_text()).prop_map(move |(role, text)| {
        serde_json::json!({
            "type": role,
            "uuid": format!("evt-{idx}"),
            "timestamp": "2026-06-22T12:00:00.000Z",
            "message": { "role": role, "content": text },
        })
    })
}

/// A JSONL document of uniquely-identified Claude Code records, newline-joined.
/// May or may not end with a trailing newline (both are valid reader inputs).
fn claude_document() -> impl Strategy<Value = Vec<u8>> {
    (0usize..12)
        .prop_flat_map(|n| {
            let recs: Vec<_> = (0..n).map(claude_record).collect();
            (recs, any::<bool>())
        })
        .prop_map(|(records, trailing_nl)| {
            let mut doc = records
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            if trailing_nl && !doc.is_empty() {
                doc.push('\n');
            }
            doc.into_bytes()
        })
}

/// Raw arbitrary bytes — including invalid UTF-8 and random newlines — so the
/// reader and adapters are stressed on input that is *not* well-formed JSONL.
fn arbitrary_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..256)
}

/// The tools whose adapters are compiled in. Driving generated bytes through
/// each one widens coverage of the normalization contract.
fn any_tool() -> impl Strategy<Value = SourceKind> {
    prop_oneof![
        Just(SourceKind::ClaudeCode),
        Just(SourceKind::Codex),
        Just(SourceKind::Gemini),
        Just(SourceKind::Otel),
        Just(SourceKind::Cursor),
        Just(SourceKind::Windsurf),
        Just(SourceKind::Zed),
        Just(SourceKind::VsCode),
        Just(SourceKind::Copilot),
    ]
}

// ---------------------------------------------------------------------------
// Properties.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// (1) Determinism: parsing the same generated JSONL twice is byte-identical
    /// (serialized), and the result depends only on the input — re-parsing in a
    /// freshly spawned thread yields the identical bytes (pure, thread-free).
    #[test]
    fn parse_is_deterministic_and_thread_independent(bytes in claude_document()) {
        let path = Path::new("gen.jsonl");
        let a = parse_events(SourceKind::ClaudeCode, &bytes, path);
        let b = parse_events(SourceKind::ClaudeCode, &bytes, path);
        check_determinism(&a, &b).map_err(TestCaseError::fail)?;

        // Thread-independence: the same pure function, run on another thread,
        // must produce the identical serialized output (no ambient/thread state).
        let bytes_for_thread = bytes.clone();
        let serialized_main = serde_json::to_string(&a).unwrap();
        let serialized_thread = std::thread::spawn(move || {
            let evts = parse_events(SourceKind::ClaudeCode, &bytes_for_thread, Path::new("gen.jsonl"));
            serde_json::to_string(&evts).unwrap()
        })
        .join()
        .expect("parse thread must not panic");
        prop_assert_eq!(serialized_main, serialized_thread);
    }

    /// (1) Determinism over arbitrary (possibly non-UTF8) bytes, through every
    /// adapter: the reader + adapter must still be a pure function and never
    /// panic on malformed input.
    #[test]
    fn parse_is_deterministic_over_arbitrary_bytes(
        tool in any_tool(),
        bytes in arbitrary_bytes(),
    ) {
        let path = Path::new("gen.bin");
        let a = parse_events(tool, &bytes, path);
        let b = parse_events(tool, &bytes, path);
        check_determinism(&a, &b).map_err(TestCaseError::fail)?;
    }

    /// (2) Idempotency by `event_id`: feeding a uniquely-identified record stream
    /// concatenated with itself through a single parse pass (one shared
    /// `ParseCtx`) dedups back to the single-pass event set. Adapters dedup
    /// recognized records via `ctx.first_seen`, so the doubled stream must not
    /// introduce duplicate `(session_id, event_id)` keys and must yield exactly
    /// the single-pass dedup-key set.
    #[test]
    fn parse_is_idempotent_by_event_id(bytes in claude_document()) {
        let path = Path::new("gen.jsonl");

        let single_recs = read_records_from_bytes(&bytes, path);
        let adapter = adapter_for(SourceKind::ClaudeCode).expect("adapter must be compiled");

        let (single, _ctx) = parse_records(adapter.as_ref(), &single_recs);

        // Build a doubled stream: the same records again, in one parse pass.
        let mut doubled_recs = single_recs.clone();
        doubled_recs.extend(single_recs.iter().cloned());
        let (doubled, _ctx2) = parse_records(adapter.as_ref(), &doubled_recs);

        // No duplicate dedup keys were introduced by the second copy.
        check_unique_event_ids(&doubled).map_err(TestCaseError::fail)?;

        // The dedup-key SET is identical between the single and doubled passes.
        let key_set = |evs: &[CaptureEvent]| -> std::collections::BTreeSet<(String, String)> {
            evs.iter()
                .map(|e| (e.session_id.clone(), e.event_id.clone()))
                .collect()
        };
        prop_assert_eq!(key_set(&single), key_set(&doubled));
    }

    /// (3) Monotonic `seq`: holds for any generated record stream, per session.
    /// Checked over every adapter so the property is not Claude-specific.
    #[test]
    fn seq_is_monotonic(tool in any_tool(), bytes in claude_document()) {
        let events = parse_events(tool, &bytes, Path::new("gen.jsonl"));
        check_monotonic_seq(&events).map_err(TestCaseError::fail)?;
    }

    /// (3) Monotonic `seq` over arbitrary bytes too: even a malformed stream that
    /// produces a pile of Unknown events keeps `seq` strictly increasing.
    #[test]
    fn seq_is_monotonic_over_arbitrary_bytes(tool in any_tool(), bytes in arbitrary_bytes()) {
        let events = parse_events(tool, &bytes, Path::new("gen.bin"));
        check_monotonic_seq(&events).map_err(TestCaseError::fail)?;
    }

    /// (4) Losslessness: every distinct non-blank source record yields at least
    /// one event. The generator stamps a unique id on each record, so no two
    /// distinct records collapse via the content-hash dedup fallback; the event
    /// count is therefore `>=` the non-blank record count.
    #[test]
    fn parse_is_lossless(bytes in claude_document()) {
        let events = parse_events(SourceKind::ClaudeCode, &bytes, Path::new("gen.jsonl"));
        let nonblank = count_nonblank_lines(&bytes);
        check_lossless(nonblank, &events).map_err(TestCaseError::fail)?;
    }

    /// (5) Gate purity: `evaluate(s)` depends only on `s`. Two calls are equal,
    /// and evaluating the same text on an independently-constructed gate (a
    /// different "context") yields the identical markers — no order/context
    /// dependence.
    #[test]
    fn gate_is_pure(text in turn_text()) {
        let gate = CommitmentGate::default_table();
        let a = gate.evaluate(&text);
        let b = gate.evaluate(&text);
        prop_assert_eq!(&a, &b);

        // A second, independently constructed gate must agree exactly.
        let other = CommitmentGate::default_table();
        prop_assert_eq!(&a, &other.evaluate(&text));
    }

    /// (5) Gate purity under interleaving: evaluating two texts in either order
    /// on the same gate yields per-text results identical to evaluating each in
    /// isolation. This is the "depends only on s, not on call history" property.
    #[test]
    fn gate_is_order_independent(t1 in turn_text(), t2 in turn_text()) {
        let gate = CommitmentGate::default_table();

        let iso1 = gate.evaluate(&t1);
        let iso2 = gate.evaluate(&t2);

        // Forward order, then reverse order, on the same instance.
        let f1 = gate.evaluate(&t1);
        let f2 = gate.evaluate(&t2);
        let r2 = gate.evaluate(&t2);
        let r1 = gate.evaluate(&t1);

        prop_assert_eq!(&iso1, &f1);
        prop_assert_eq!(&iso2, &f2);
        prop_assert_eq!(&iso1, &r1);
        prop_assert_eq!(&iso2, &r2);
    }

    /// (5) Gate purity over arbitrary text: evaluate must be repeatable and
    /// never panic on any string, including unusual unicode.
    #[test]
    fn gate_is_pure_over_arbitrary_text(text in ".{0,200}") {
        let gate = CommitmentGate::default_table();
        prop_assert_eq!(gate.evaluate(&text), gate.evaluate(&text));
    }

    /// (6) Offset resumption: for any split of the buffer at a newline boundary,
    /// `read_records_from_bytes(prefix) ++ read_records_from_bytes(rest)` equals
    /// `read_records_from_bytes(whole)` at the byte level. This is what lets a
    /// live tailer resume from a persisted offset without losing or duplicating
    /// records.
    #[test]
    fn offset_resumption_holds(bytes in arbitrary_bytes()) {
        let path = Path::new("gen.bin");
        let whole: Vec<Vec<u8>> = read_records_from_bytes(&bytes, path)
            .into_iter()
            .map(|r| r.bytes)
            .collect();

        // Newline boundaries are the only points a real tailer resumes at.
        let mut boundaries = vec![0usize, bytes.len()];
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                boundaries.push(i + 1);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();

        for split in boundaries {
            let mut combined: Vec<Vec<u8>> = read_records_from_bytes(&bytes[..split], path)
                .into_iter()
                .map(|r| r.bytes)
                .collect();
            combined.extend(
                read_records_from_bytes(&bytes[split..], path)
                    .into_iter()
                    .map(|r| r.bytes),
            );
            prop_assert_eq!(&combined, &whole);
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture-driven properties: the same invariants, but exercised on the real
// committed fixtures (every tool / version / case). These anchor the proptest
// strategies on inputs adapters fully recognize.
// ---------------------------------------------------------------------------

/// Resolve the `SourceKind` for a fixture's tool slug, skipping any case whose
/// slug is not a known source (defensive — `discover_cases` walks the dir).
fn tool_for_case(c: &GoldenCase) -> Option<SourceKind> {
    SourceKind::parse(&c.tool)
}

/// Dedup-key set over a normalized event stream.
fn key_set(evs: &[CaptureEvent]) -> std::collections::BTreeSet<(String, String)> {
    evs.iter()
        .map(|e| (e.session_id.clone(), e.event_id.clone()))
        .collect()
}

#[test]
fn fixtures_satisfy_section_8_3_invariants() {
    let cases = discover_cases();
    assert!(
        !cases.is_empty(),
        "expected committed fixtures under fixtures/"
    );

    for case in &cases {
        let Some(tool) = tool_for_case(case) else {
            continue;
        };
        let bytes = case.read_input().expect("fixture readable");
        let path = case.input_path();
        let label = format!("{}/{}/{}", case.tool, case.version, case.case);

        // Determinism.
        let a = parse_events(tool, &bytes, &path);
        let b = parse_events(tool, &bytes, &path);
        check_determinism(&a, &b).unwrap_or_else(|e| panic!("{label}: {e}"));

        // Monotonic seq.
        check_monotonic_seq(&a).unwrap_or_else(|e| panic!("{label}: {e}"));

        // Losslessness.
        let nonblank = count_nonblank_lines(&bytes);
        check_lossless(nonblank, &a).unwrap_or_else(|e| panic!("{label}: {e}"));

        // A single normal parse pass introduces no duplicate dedup keys: every
        // recognized record is deduped on its native id, and the per-record
        // content-hash fallback is unique within a real transcript.
        check_unique_event_ids(&a).unwrap_or_else(|e| panic!("{label}: {e}"));

        // Idempotency: re-parsing the identical bytes yields the identical
        // dedup-key set (true idempotency of the parse function).
        let reparse = parse_events(tool, &bytes, &path);
        assert_eq!(
            key_set(&a),
            key_set(&reparse),
            "{label}: re-parsing changed the event_id key set",
        );
    }
}
