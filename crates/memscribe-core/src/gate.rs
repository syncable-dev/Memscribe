//! The deterministic commitment-marker gate (whitepaper Appendix B).
//!
//! A small, inspectable rule table over user turns. Each rule is a category plus
//! a regular expression. Evaluating a turn is a **pure function of the turn
//! text** — no global state — which is the property the gate-purity test
//! asserts. A match elevates the turn-span to a Conversation node and seeds a
//! candidate Decision; a non-match retains the verbatim turn at low salience but
//! creates no node.

use crate::node::{CommitmentMarker, MarkerCategory};
use regex::Regex;

/// One rule in the commitment-marker table.
pub struct GateRule {
    /// The rule id (e.g. `decision_verb.use`).
    pub id: String,
    /// The category the rule expresses.
    pub category: MarkerCategory,
    /// The case-insensitive pattern.
    pub pattern: Regex,
}

impl std::fmt::Debug for GateRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateRule")
            .field("id", &self.id)
            .field("category", &self.category)
            .field("pattern", &self.pattern.as_str())
            .finish()
    }
}

/// The default rule table, as `(id, category, pattern)` triples. Patterns are
/// compiled case-insensitively. Kept here so the table is inspectable and
/// unit-tested per rule.
#[must_use]
pub fn default_rules() -> Vec<(&'static str, MarkerCategory, &'static str)> {
    use MarkerCategory::*;
    vec![
        // Explicit decision verbs.
        (
            "decision_verb.use",
            DecisionVerb,
            r"\b(?:use|using|adopt|adopts|go with|let'?s go with|switch to|migrate to)\b",
        ),
        (
            "decision_verb.decide",
            DecisionVerb,
            r"\b(?:decide(?:d)?|we(?:'ll| will) choose|choose|chose|settle on|going to use|pick(?:ed)?)\b",
        ),
        // Rejected alternatives.
        ("rejection.instead_of", Rejection, r"\binstead of\b"),
        (
            "rejection.rather_than",
            Rejection,
            r"\b(?:rather than|as opposed to|in favor of)\b",
        ),
        // Bans (Kruchten anticrisis).
        (
            "ban.negated_use",
            Ban,
            r"\b(?:never|do not|don'?t|won'?t|will not|must not|should not|shouldn'?t|no longer)\s+(?:use|add|adopt|depend|introduce|rely)\b",
        ),
        (
            "ban.no_dependency",
            Ban,
            r"\bno (?:new )?dependenc(?:y|ies)\b",
        ),
        // Imperatives.
        (
            "imperative.must_always_never",
            Imperative,
            r"\b(?:must|always|never|shall|required to|need to)\b",
        ),
        // Memory directives.
        (
            "memory.remember",
            Memory,
            r"\b(?:remember that|keep in mind|note that|for future reference|don'?t forget)\b",
        ),
    ]
}

/// The commitment gate: a compiled, ordered rule table.
#[derive(Debug)]
pub struct CommitmentGate {
    rules: Vec<GateRule>,
}

impl Default for CommitmentGate {
    fn default() -> Self {
        Self::default_table()
    }
}

impl CommitmentGate {
    /// Build the gate from the default rule table.
    ///
    /// # Panics
    /// Never in practice — the default patterns are compile-time constants and
    /// are exercised by tests; a malformed default is a build-breaking bug.
    #[must_use]
    pub fn default_table() -> Self {
        Self::from_triples(default_rules()).expect("default gate rules must compile")
    }

    /// Build a gate from `(id, category, pattern)` triples (config-driven).
    ///
    /// # Errors
    /// Returns the underlying regex error if any pattern fails to compile.
    pub fn from_triples<S: AsRef<str>>(
        triples: impl IntoIterator<Item = (S, MarkerCategory, S)>,
    ) -> Result<Self, regex::Error> {
        let mut rules = Vec::new();
        for (id, category, pattern) in triples {
            let pattern = Regex::new(&format!("(?i){}", pattern.as_ref()))?;
            rules.push(GateRule {
                id: id.as_ref().to_string(),
                category,
                pattern,
            });
        }
        Ok(CommitmentGate { rules })
    }

    /// The number of rules in the table.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Evaluate a turn's text against the rule table. **Pure**: depends only on
    /// `text`. Returns the markers that fired, in rule order (deterministic).
    #[must_use]
    pub fn evaluate(&self, text: &str) -> Vec<CommitmentMarker> {
        let mut out = Vec::new();
        for rule in &self.rules {
            if let Some(m) = rule.pattern.find(text) {
                out.push(CommitmentMarker {
                    rule_id: rule.id.clone(),
                    category: rule.category,
                    matched_text: m.as_str().to_string(),
                    offset: m.start(),
                });
            }
        }
        out
    }

    /// Whether the turn is admitted (any marker fired).
    #[must_use]
    pub fn admits(&self, text: &str) -> bool {
        self.rules.iter().any(|r| r.pattern.is_match(text))
    }

    /// Whether any fired marker is a ban.
    #[must_use]
    pub fn is_ban(&self, markers: &[CommitmentMarker]) -> bool {
        markers.iter().any(|m| m.category == MarkerCategory::Ban)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_table_compiles_and_has_rules() {
        let gate = CommitmentGate::default_table();
        assert!(gate.rule_count() >= 8);
    }

    #[test]
    fn decision_verb_fires() {
        let gate = CommitmentGate::default_table();
        let m = gate.evaluate("Let's go with Postgres for storage.");
        assert!(!m.is_empty());
        assert!(m.iter().any(|x| x.category == MarkerCategory::DecisionVerb));
    }

    #[test]
    fn rejection_fires() {
        let gate = CommitmentGate::default_table();
        let m = gate.evaluate("Use Stripe instead of PayPal.");
        assert!(m.iter().any(|x| x.category == MarkerCategory::Rejection));
        assert!(m.iter().any(|x| x.category == MarkerCategory::DecisionVerb));
    }

    #[test]
    fn ban_fires_and_is_detected() {
        let gate = CommitmentGate::default_table();
        let m = gate.evaluate("We will never add a dependency on left-pad.");
        assert!(gate.is_ban(&m), "expected a ban marker: {m:?}");
    }

    #[test]
    fn plain_chatter_does_not_admit() {
        let gate = CommitmentGate::default_table();
        assert!(!gate.admits("Thanks, that looks good to me."));
    }

    #[test]
    fn evaluate_is_pure_and_repeatable() {
        let gate = CommitmentGate::default_table();
        let t = "We must always use prepared statements instead of string concatenation.";
        assert_eq!(gate.evaluate(t), gate.evaluate(t));
    }

    #[test]
    fn case_insensitive() {
        let gate = CommitmentGate::default_table();
        assert!(gate.admits("LET'S GO WITH redis"));
    }
}
