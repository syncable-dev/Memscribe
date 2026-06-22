//! Whitepaper §8.3 pipeline invariants as `proptest` properties.
//!
//! These drive `DefaultPipeline::without_redaction().prepare_events` over
//! randomly-constructed `Vec<CaptureEvent>` (built directly, not parsed from
//! bytes) and assert the pipeline's core guarantees:
//!
//! - **Determinism** — `prepare_events(x)` is byte-identical across runs.
//! - **Idempotency** — preparing the same event stream twice yields identical
//!   node sets (the pipeline is a pure function with no accumulating state).
//! - **Temporal validity** — every emitted `Binding` carries a PROV record with
//!   `t_use <= t_gen` (`prov.is_temporally_valid()`), the §8.8 invariant the
//!   binder is responsible for.
//!
//! Events are generated with strictly increasing `seq` and non-decreasing
//! timestamps within a session, mirroring how a real adapter assigns them from
//! file order — the precondition the binder's `t_use <= t_gen` search relies on.

use memscribe_core::model::{Diff, EventKind};
use memscribe_core::{
    CaptureEvent, DefaultPipeline, PreparedNode, ProjectRef, SourceKind, SourceLocation,
    SCHEMA_VERSION,
};
use proptest::prelude::*;
use std::path::PathBuf;
use time::OffsetDateTime;

/// A base instant; per-event timestamps are this plus the event's `seq` seconds,
/// so timestamps are non-decreasing in lockstep with `seq` within a session.
fn base_time() -> OffsetDateTime {
    // 2026-06-22T12:00:00Z, expressed via Unix time to avoid macro feature reqs.
    OffsetDateTime::from_unix_timestamp(1_781_697_600).expect("valid unix timestamp")
}

/// The kind of synthetic event to generate. The two that drive the
/// segmenter/binder are a decision-bearing user turn and a file edit; the rest
/// are filler that must pass through without producing spurious bindings.
#[derive(Clone, Debug)]
enum GenEvent {
    /// A user turn whose text fires (or does not fire) the commitment gate.
    UserTurn(String),
    /// A file edit on a path — becomes a `CodeEpisode`.
    FileEdit(String),
    /// An assistant turn (filler; no node).
    Assistant(String),
}

/// Decision-bearing and plain user-turn texts, so the gate is exercised both
/// ways (admitted → Decision/Conversation; plain → no node).
fn user_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("let's go with Postgres for storage".to_string()),
        Just("use Stripe instead of PayPal".to_string()),
        Just("we will never add a dependency on left-pad".to_string()),
        Just("we must always use prepared statements".to_string()),
        Just("thanks, that looks good to me".to_string()),
        "[a-zA-Z ]{0,40}".prop_map(|s| s),
    ]
}

fn edit_path() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("src/main.rs".to_string()),
        Just("src/lib.rs".to_string()),
        Just("Cargo.toml".to_string()),
        "[a-z]{1,8}\\.rs".prop_map(|s| format!("src/{s}")),
    ]
}

fn gen_event() -> impl Strategy<Value = GenEvent> {
    prop_oneof![
        user_text().prop_map(GenEvent::UserTurn),
        edit_path().prop_map(GenEvent::FileEdit),
        "[a-z ]{0,20}".prop_map(GenEvent::Assistant),
    ]
}

/// Build a real `Vec<CaptureEvent>` from generated descriptors, assigning a
/// strictly-increasing `seq` and a non-decreasing timestamp (base + seq seconds)
/// within a single session — the shape an adapter produces from file order.
fn build_events(session_id: &str, gen: &[GenEvent]) -> Vec<CaptureEvent> {
    let base = base_time();
    let project = ProjectRef::from_cwd(".");
    gen.iter()
        .enumerate()
        .map(|(i, g)| {
            let seq = i as u64;
            let timestamp = base + time::Duration::seconds(seq as i64);
            let (event_id, kind) = match g {
                GenEvent::UserTurn(text) => (
                    format!("u-{seq}"),
                    EventKind::UserTurn {
                        text: text.clone(),
                        parts: Vec::new(),
                    },
                ),
                GenEvent::FileEdit(path) => (
                    format!("e-{seq}"),
                    EventKind::FileEdit {
                        call_id: None,
                        diff: Diff::for_path(path.as_str()),
                    },
                ),
                GenEvent::Assistant(text) => (
                    format!("a-{seq}"),
                    EventKind::AssistantTurn {
                        text: text.clone(),
                        thinking: None,
                        model: None,
                        usage: None,
                        parts: Vec::new(),
                    },
                ),
            };
            CaptureEvent {
                schema_version: SCHEMA_VERSION,
                source: SourceKind::ClaudeCode,
                session_id: session_id.to_string(),
                seq,
                event_id,
                parent_id: None,
                timestamp,
                project: project.clone(),
                kind,
                provenance: SourceLocation::new(PathBuf::from("gen.jsonl"), seq, seq + 1),
            }
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Determinism: `prepare_events` is a pure function — two runs over the same
    /// event stream serialize byte-identically.
    #[test]
    fn prepare_events_is_deterministic(gen in proptest::collection::vec(gen_event(), 0..16)) {
        let events = build_events("s1", &gen);
        let pipeline = DefaultPipeline::without_redaction();

        let a = pipeline.prepare_events(&events);
        let b = pipeline.prepare_events(&events);

        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        prop_assert_eq!(ja, jb);
    }

    /// Idempotency: a freshly-constructed pipeline over the same events yields an
    /// identical node set — preparing twice does not accumulate state or drift.
    #[test]
    fn prepare_events_is_idempotent(gen in proptest::collection::vec(gen_event(), 0..16)) {
        let events = build_events("s1", &gen);

        let first = DefaultPipeline::without_redaction().prepare_events(&events);
        let second = DefaultPipeline::without_redaction().prepare_events(&events);

        // Same pipeline instance, applied again, must also match.
        let pipeline = DefaultPipeline::without_redaction();
        let third = pipeline.prepare_events(&events);
        let fourth = pipeline.prepare_events(&events);

        prop_assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        prop_assert_eq!(
            serde_json::to_string(&third).unwrap(),
            serde_json::to_string(&fourth).unwrap()
        );
        prop_assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&third).unwrap()
        );
    }

    /// Temporal validity (§8.8): every emitted `Binding` satisfies the PROV
    /// invariant `t_use <= t_gen` via `prov.is_temporally_valid()`.
    #[test]
    fn every_binding_is_temporally_valid(
        gen in proptest::collection::vec(gen_event(), 0..24),
    ) {
        let events = build_events("s1", &gen);
        let nodes = DefaultPipeline::without_redaction().prepare_events(&events);

        for node in &nodes {
            if let PreparedNode::Binding(edge) = node {
                prop_assert!(
                    edge.prov.is_temporally_valid(),
                    "binding {:?} -> {:?} violates t_use <= t_gen: t_use={}, t_gen={}",
                    edge.from,
                    edge.to,
                    edge.prov.t_use,
                    edge.prov.t_gen
                );
            }
        }
    }

    /// Cross-session temporal validity: even with two interleaved sessions, every
    /// binding's PROV invariant holds (the binder must never bind across
    /// sessions, so `t_use <= t_gen` cannot be violated by session interleaving).
    #[test]
    fn bindings_valid_across_two_sessions(
        a in proptest::collection::vec(gen_event(), 0..12),
        b in proptest::collection::vec(gen_event(), 0..12),
    ) {
        let mut events = build_events("sA", &a);
        events.extend(build_events("sB", &b));

        let nodes = DefaultPipeline::without_redaction().prepare_events(&events);

        for node in &nodes {
            if let PreparedNode::Binding(edge) = node {
                prop_assert!(edge.prov.is_temporally_valid());
                // The binder binds within a session only.
                prop_assert_eq!(
                    &edge.prov.used_session,
                    &edge.prov.was_generated_by_session
                );
            }
        }
    }
}
