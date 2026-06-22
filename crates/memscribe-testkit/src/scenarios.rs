//! The cross-tool conformance scenario catalog (whitepaper §8.2).
//!
//! A canonical set of scenarios — authored once, captured from every tool — that
//! must normalize to the same shape regardless of which tool produced them. Each
//! scenario names the invariant the conformance suite asserts.

/// One canonical conformance scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scenario {
    /// A stable slug used as the fixture `<case>` name.
    pub slug: &'static str,
    /// What the scenario exercises and the shape it must normalize to.
    pub expectation: &'static str,
}

/// The canonical scenarios every adapter's fixtures must cover.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        slug: "happy_path_decision_then_edits",
        expectation: "a decision turn followed by edits to N files → 1 Decision, N Episodes, N Bindings",
    },
    Scenario {
        slug: "rejected_alternative",
        expectation: "\"use Stripe instead of PayPal\" → considered_options populated, the unchosen one marked",
    },
    Scenario {
        slug: "ban",
        expectation: "\"we will NOT add a dependency on X\" → is_ban = true",
    },
    Scenario {
        slug: "interleaved_arcs",
        expectation: "two decisions, edits to overlapping files → correct per-decision binding",
    },
    Scenario {
        slug: "multi_edit_single_commit",
        expectation: "a single commit touching several files → several Episodes",
    },
    Scenario {
        slug: "tool_failure",
        expectation: "edit rejected (ToolResult.ok = false) → no spurious Episode",
    },
    Scenario {
        slug: "rewind_compaction",
        expectation: "rewind/compaction flagged, verbatim history preserved, current view honors it",
    },
    Scenario {
        slug: "subagent_thread",
        expectation: "a subagent thread → attributed, not merged",
    },
    Scenario {
        slug: "no_commitment_marker",
        expectation: "a turn with no marker → no Conversation node elevated, verbatim turn still retained",
    },
];

/// The scenario slugs, for iterating fixtures.
#[must_use]
pub fn slugs() -> Vec<&'static str> {
    SCENARIOS.iter().map(|s| s.slug).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_complete_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for s in SCENARIOS {
            assert!(seen.insert(s.slug), "duplicate scenario slug {}", s.slug);
        }
        assert_eq!(SCENARIOS.len(), 9);
    }
}
