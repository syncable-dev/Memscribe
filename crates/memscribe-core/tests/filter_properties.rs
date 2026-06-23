//! Property tests (`proptest`) for the deterministic turn-source hygiene
//! ([`ProseFilter`]) and commitment-intent ([`IntentFilter`]) stages, extending
//! the whitepaper §8.3 guarantees (purity, never-panic, idempotency) to the new
//! gate-admission filters the same way [`tests/pipeline_properties.rs`] covers
//! the pipeline.

use memscribe_core::{IntentFilter, ProseFilter};
use proptest::prelude::*;

/// A mix of fully-arbitrary unicode and structured multi-line turns (real human
/// requests interleaved with the junk classes the filters must drop), so the
/// properties are exercised on both noise and realistic shapes.
fn arbitrary_text() -> impl Strategy<Value = String> {
    prop_oneof![
        // Arbitrary single-line unicode (proptest's `.` excludes newline).
        Just(String::new()),
        ".{0,300}",
        // Structured, possibly-multi-line turns.
        proptest::collection::vec(
            prop_oneof![
                Just("can you fix that or add 2 new test cases".to_string()),
                Just("we will use Postgres instead of MySQL".to_string()),
                Just("<tool-use-id>toolu_01abc</tool-use-id>".to_string()),
                Just("the React layer never re-measures or calls resize".to_string()),
                Just("## Summary of fixes the applier should make".to_string()),
                Just("do I need to run db:push ?".to_string()),
                Just("74 | use redb::{Database};".to_string()),
                Just("Yes we need to do that .".to_string()),
                "[a-zA-Z0-9 ,.:`/_?!-]{0,48}".prop_map(|s| s),
            ],
            0..6
        )
        .prop_map(|lines| lines.join("\n")),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    // ---- ProseFilter ----

    /// Purity: the projection is an exact function of the input.
    #[test]
    fn prose_filter_is_pure(s in arbitrary_text()) {
        let f = ProseFilter::default_filter();
        prop_assert_eq!(f.clean(&s), f.clean(&s));
    }

    /// `is_human_prose` agrees with `clean().is_some()` for all input.
    #[test]
    fn prose_human_matches_clean(s in arbitrary_text()) {
        let f = ProseFilter::default_filter();
        prop_assert_eq!(f.is_human_prose(&s), f.clean(&s).is_some());
    }

    /// Idempotency: re-projecting an already-clean projection is a no-op — every
    /// kept line survives a second pass.
    #[test]
    fn prose_projection_is_idempotent(s in arbitrary_text()) {
        let f = ProseFilter::default_filter();
        if let Some(p) = f.clean(&s) {
            let again = f.clean(&p);
            prop_assert_eq!(again.as_deref(), Some(p.as_str()));
        }
    }

    /// Never panics on arbitrary input (§8.3 never-panic contract).
    #[test]
    fn prose_filter_never_panics(s in ".{0,600}") {
        let f = ProseFilter::default_filter();
        let _ = f.clean(&s);
        let _ = f.is_human_prose(&s);
    }

    // ---- IntentFilter ----

    /// Purity: the commitment verdict depends only on the text.
    #[test]
    fn intent_filter_is_pure(s in arbitrary_text()) {
        let f = IntentFilter::default_filter();
        prop_assert_eq!(f.is_committal(&s), f.is_committal(&s));
    }

    /// Never panics on arbitrary input.
    #[test]
    fn intent_filter_never_panics(s in ".{0,600}") {
        let f = IntentFilter::default_filter();
        let _ = f.is_committal(&s);
    }

    /// Recall guarantee as a property: a directed request to the assistant is
    /// always committal, regardless of what follows the lead-in (so the demotion
    /// rules can never swallow a real request).
    #[test]
    fn directed_request_always_commits(tail in "[a-zA-Z0-9 ,.]{1,60}") {
        let f = IntentFilter::default_filter();
        let can_you = format!("can you {}", tail);
        let please = format!("please {}", tail);
        let lets = format!("let's {}", tail);
        prop_assert!(f.is_committal(&can_you));
        prop_assert!(f.is_committal(&please));
        prop_assert!(f.is_committal(&lets));
    }
}
