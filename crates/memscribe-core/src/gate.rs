//! The deterministic commitment-marker gate (whitepaper Appendix B).
//!
//! A small, inspectable rule table over user turns. Each rule is a category plus
//! a regular expression. Evaluating a turn is a **pure function of the turn
//! text** — no global state — which is the property the gate-purity test
//! asserts. A match elevates the turn-span to a Conversation node and seeds a
//! candidate Decision; a non-match retains the verbatim turn at low salience but
//! creates no node.

use crate::intent::IntentFilter;
use crate::node::{CommitmentMarker, MarkerCategory};
use crate::prose::ProseFilter;
use regex::Regex;

/// How strongly a fired marker should elevate. The gate stays a pure lexical
/// matcher; this only governs whether a match *seeds a candidate Decision* or
/// merely elevates a Conversation (the precision lever for high-recall action
/// verbs — research round #36, the commitment-synonym panel).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// High-precision commitment: seeds a candidate Decision unconditionally
    /// (the historical behavior of every shipped rule).
    Strong,
    /// High-recall but ambiguous (action requests, demoted modals, soft
    /// rejections): always elevates a Conversation, but seeds a Decision only
    /// when an edit lands in the same session to confirm it. A
    /// [`MarkerCategory::Confirmation`] marker never seeds a Decision even then
    /// (the single-turn gate cannot see the proposal it confirms).
    Soft,
}

/// One rule in the commitment-marker table.
pub struct GateRule {
    /// The rule id (e.g. `decision_verb.use`).
    pub id: String,
    /// The category the rule expresses.
    pub category: MarkerCategory,
    /// Whether a match seeds a Decision outright or only when an edit confirms.
    pub tier: Tier,
    /// The case-insensitive pattern.
    pub pattern: Regex,
}

impl std::fmt::Debug for GateRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateRule")
            .field("id", &self.id)
            .field("category", &self.category)
            .field("tier", &self.tier)
            .field("pattern", &self.pattern.as_str())
            .finish()
    }
}

/// The default rule table, as `(id, category, tier, pattern)` quads. Patterns
/// are compiled case-insensitively. Kept here so the table is inspectable and
/// unit-tested per rule.
///
/// Tiers (see [`Tier`]): `Strong` rules seed a Decision outright; `Soft` rules
/// (action requests, demoted bare modals, soft rejections, confirmations)
/// elevate a Conversation and seed a Decision only when an edit confirms them.
/// The patterns reflect the commitment-synonym research panel + its adversarial
/// precision critic: action verbs are **imperative base forms only** (no
/// past-tense / nominalization narrative arms), bans split by negator strength,
/// and the action request is lead-in-guarded within a tight same-clause window.
#[must_use]
pub fn default_rules() -> Vec<(&'static str, MarkerCategory, Tier, &'static str)> {
    use MarkerCategory::*;
    use Tier::*;
    vec![
        // ---- Explicit decision verbs (selection / adoption) ----
        (
            "decision_verb.use",
            DecisionVerb,
            Strong,
            r"\b(?:use|using|adopt|adopts|go with|let'?s go with|switch to|migrate to)\b",
        ),
        (
            "decision_verb.decide",
            DecisionVerb,
            Strong,
            r"\b(?:decide(?:d)?|we(?:'ll| will) choose|choose|chose|settle on|going to use|pick(?:ed)?)\b",
        ),
        (
            "decision_verb.select_prep",
            DecisionVerb,
            Strong,
            r"\b(?:opt(?:s|ed|ing)? for|default(?:s|ed|ing)? to|standardi[sz](?:e|es|ed|ing) on|(?:stick|sticking|stuck|stay|staying) with|lean(?:s|ing|ed)? towards?)\b",
        ),
        (
            "decision_verb.polite_select",
            DecisionVerb,
            Strong,
            r"\b(?:let'?s|let us|we(?:'ll| will| should| could|'re going to|'re going)|i(?:'ll| will| want to|'d like to)|can we|why don'?t we|how about we|plan(?:ning)? to|intend to)\s+(?:use|adopt|switch to|migrate to|go with|pick|choose|prefer)\b",
        ),
        // ---- Rejected alternatives / replacement / deprecation ----
        ("rejection.instead_of", Rejection, Strong, r"\binstead of\b"),
        (
            "rejection.rather_than",
            Rejection,
            Strong,
            r"\b(?:rather than|as opposed to|in favou?r of|in preference to)\b",
        ),
        (
            "rejection.replace_swap",
            Rejection,
            Strong,
            r"\b(?:replac(?:e|es|ed|ing|ement)|swap(?:s|ped|ping)?|substitut(?:e|es|ed|ing|ion))\s+(?:\w+\s+){0,4}?(?:with|for|out|over\s+to)\b",
        ),
        (
            "rejection.move_away_from",
            Rejection,
            Strong,
            r"\b(?:mov(?:e|es|ed|ing)|migrat(?:e|es|ed|ing)|shift(?:s|ed|ing)?|transition(?:s|ed|ing)?|pivot(?:s|ed|ing)?|step(?:s|ped|ping)?)\s+away\s+from\b",
        ),
        (
            "rejection.do_away_with",
            Rejection,
            Strong,
            r"\b(?:do(?:es|ing)?\s+away\s+with|gets?\s+rid\s+of|got\s+rid\s+of)\b",
        ),
        (
            "rejection.deprecate",
            Rejection,
            Soft,
            r"\b(?:deprecat(?:e|es|ed|ing|ion)|sunset(?:s|ted|ting)?|retir(?:e|es|ed|ing|ement)|obsolet(?:e|es|ed|ing)|ditch(?:es|ed|ing)?|scrap(?:ped|ping)|supersed(?:e|es|ed|ing))\b",
        ),
        // ---- Bans (Kruchten anticrisis) ----
        // Strong prohibitive negators take the wide verb set; ability negators
        // (can't / won't) are restricted to the commitment set, so "can't run the
        // tests" (a bug report) does not read as a ban.
        (
            "ban.negated_use",
            Ban,
            Strong,
            r"\b(?:(?:never|do not|don'?t|must not|mustn'?t|should not|shouldn'?t|may not|no longer)\s+(?:use|using|add|adopt|depend|introduce|rely|install|import|require|pull in|bump|upgrade|downgrade|pin|vendor|enable|disable|expose|hardcode|hard-code|commit|merge|push|call|invoke|touch|modify|change|edit|create|run|execute|bypass|skip|mock|stub|patch|override)|(?:cannot|can'?t|won'?t|will not)\s+(?:use|using|add|adopt|depend|introduce|rely))\b",
        ),
        (
            "ban.no_dependency",
            Ban,
            Strong,
            r"\bno (?:new |more |further |additional |extra )?(?:dependenc(?:y|ies)|deps?|packages?|crates?|imports?|libraries?|libs?|frameworks?|abstractions?)\b",
        ),
        (
            "ban.forbid",
            Ban,
            Strong,
            r"\b(?:forbid(?:s)?|prohibit(?:s|ed)?|disallow(?:s|ed)?|(?:is|are|strictly|expressly)\s+forbidden|not permitted|not allowed|off[- ]limits|out of the question|under no circumstances?|verboten)\b",
        ),
        (
            "ban.stop_using",
            Ban,
            Strong,
            r"\b(?:stop (?:using|importing|depending on|relying on|calling)|discontinue using|cease using|steer clear of|stay clear of|stay away from|keep away from)\b",
        ),
        (
            "ban.avoid",
            Ban,
            Soft,
            r"\b(?:please |let'?s |you should |we should |try to |try and |best to )?avoid(?:ing)?\s+(?:using|adding|introducing|depending|the use of|importing|installing|calling|reaching for|a (?:new )?dependency)\b",
        ),
        // ---- Imperatives (modal obligation) ----
        // Bare modals are demoted to Soft: high recall, but "I've never seen…" /
        // "I need to be on the waiting list" are not decisions — they seed a
        // Decision only when an edit confirms intent.
        (
            "imperative.must_always_never",
            Imperative,
            Soft,
            r"\b(?:must|always|never|shall|required to|need to|needs to|ha(?:s|ve)\s+to|ought to|supposed to|mandatory|non-?negotiable)\b",
        ),
        (
            "imperative.ensure",
            Imperative,
            Strong,
            r"\b(?:ensure(?:\s+that)?|ensuring|make\s+sure(?:\s+that)?|making\s+sure|be\s+sure\s+to|guarantee(?:\s+that)?)\b",
        ),
        (
            "imperative.standing_subject",
            Imperative,
            Soft,
            r"\b(?:the|every|each|all|any|no)\s+\w+(?:\s+\w+){0,2}\s+(?:must|should|shall|needs?\s+to|ha(?:s|ve)\s+to)\b",
        ),
        // ---- Memory / preference directives ----
        (
            "memory.remember",
            Memory,
            Strong,
            r"\b(?:remember(?:\s+(?:that|to))?|keep in mind|bear in mind|note that|please note|for future reference|don'?t forget)\b",
        ),
        (
            "memory.standing_directive",
            Memory,
            Strong,
            r"\b(?:going forward|moving forward|from now on|from here on(?: out)?|from this point (?:on|forward)|henceforth|as a (?:rule|convention|policy)|by convention|(?:our|the)\s+(?:convention|policy|standard|default)\s+(?:here\s+)?is)\b",
        ),
        // ---- Confirmation (never seeds a Decision: single-turn-blind) ----
        (
            "confirmation.idiom",
            Confirmation,
            Soft,
            r"\b(?:lgtm|lg2m|ship\s+(?:it|that|this)|sounds good|works for me|let'?s do (?:it|that)|make it so|\+1|plus[- ]one)\b",
        ),
        // ---- Action requests (imperative code-change intent) ----
        // The headline recall fix AND the headline precision risk. Base-form
        // verbs only + a lead-in within a tight same-clause window; Soft tier so a
        // bare "can you fix that" with no resulting edit never manufactures a
        // phantom Decision.
        (
            "action.request_guarded",
            ActionRequest,
            Soft,
            r"\b(?:can you|could you|can u|would you|will you|please|pls|let'?s|let us|we (?:should|need to|have to)|you should|go ahead and|i(?:'d| would)? (?:want|need|like) you to|how about you)\b[^.\n]{0,12}?\b(?:fix|change|update|modify|adjust|tweak|patch|correct|resolve|handle|address|add|create|write|generate|remove|delete|drop|strip|refactor|rename|extract|split|simplify|clean up|optimi[sz]e|improve|reduce|secure)\b",
        ),
        (
            "action.create_strong",
            ActionRequest,
            Strong,
            r"\b(?:implement(?:s|ed|ing)?|scaffold(?:s|ed|ing)?|bootstrap(?:s|ped|ping)?|wir(?:e|es|ed|ing) (?:up|it up|this up|that up|them up)|hook(?:s|ed|ing)? (?:up|it up|this up|that up)|spin(?:s|ning)? up|spun up|stub(?:s|bed|bing)?(?: out| it out| this out)?)\b",
        ),
        (
            "action.restructure_strong",
            ActionRequest,
            Strong,
            r"\b(?:refactor(?:s|ed|ing)?|re-factor(?:s|ed|ing)?|restructure(?:s|d|ing)?|reorgani[sz]e(?:s|d|ing)?|deduplicate(?:s|d|ing)?|dedupe?(?:s|d|ing)?|de-dupe(?:s|d|ing)?|DRY\s+(?:this\s+|it\s+|that\s+)?up)\b",
        ),
        (
            "action.restructure_into",
            ActionRequest,
            Soft,
            r"\b(?:extract|factor|split|break)(?:s|ed|ing)?\s+(?:\w+\s+){0,3}?(?:into|out)\b",
        ),
        (
            "action.value_change",
            ActionRequest,
            Strong,
            r"\b(?:bump(?:s|ed|ing)?|upgrad(?:e|es|ed|ing)|downgrad(?:e|es|ed|ing)|set\s+(?:it|the\s+\w+|[a-z_][\w.]*)\s+to|(?:switch|flip|toggle)\s+(?:it|the\s+\w+|[a-z_][\w.]*)\s+(?:to|on|off)|make\s+(?:it|this|that|them|the\s+\w+)\s+(?:work|pass|fail|return|use|do|stop|handle|faster|slower|smaller|bigger|async|sync|optional|required|robust|secure|simpler|better|quicker))\b",
        ),
        (
            "action.optimize_strong",
            ActionRequest,
            Strong,
            r"\b(?:optimi[sz](?:e|es|ed|ing)|harden(?:s|ed|ing)?|robustif(?:y|ies|ied)|saniti[sz](?:e|es|ed|ing)|memoi[sz](?:e|es|ed|ing)|paralleli[sz](?:e|es|ed|ing)|debounce(?:s|d)?|debouncing|throttl(?:e|es|ed|ing)|speed(?:s|ing)?\s+(?:it\s+|this\s+|that\s+)?up|sped\s+up)\b",
        ),
    ]
}

/// The commitment gate: a compiled, ordered rule table plus the turn-source
/// hygiene filter applied to a turn before its markers are evaluated.
#[derive(Debug)]
pub struct CommitmentGate {
    rules: Vec<GateRule>,
    prose: ProseFilter,
    intent: IntentFilter,
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
        Self::from_quads(default_rules()).expect("default gate rules must compile")
    }

    /// Build a gate from `(id, category, pattern)` triples (config-driven). Every
    /// configured rule is treated as [`Tier::Strong`] (it seeds a Decision on a
    /// match) — the tier lever is an internal refinement of the default table, not
    /// part of the on-disk config schema.
    ///
    /// # Errors
    /// Returns the underlying regex error if any pattern fails to compile.
    pub fn from_triples<S: AsRef<str>>(
        triples: impl IntoIterator<Item = (S, MarkerCategory, S)>,
    ) -> Result<Self, regex::Error> {
        Self::from_quads(
            triples
                .into_iter()
                .map(|(id, category, pattern)| (id, category, Tier::Strong, pattern)),
        )
    }

    /// Build a gate from `(id, category, tier, pattern)` quads.
    ///
    /// # Errors
    /// Returns the underlying regex error if any pattern fails to compile.
    pub fn from_quads<S: AsRef<str>>(
        quads: impl IntoIterator<Item = (S, MarkerCategory, Tier, S)>,
    ) -> Result<Self, regex::Error> {
        let mut rules = Vec::new();
        for (id, category, tier, pattern) in quads {
            let pattern = Regex::new(&format!("(?i){}", pattern.as_ref()))?;
            rules.push(GateRule {
                id: id.as_ref().to_string(),
                category,
                tier,
                pattern,
            });
        }
        Ok(CommitmentGate {
            rules,
            prose: ProseFilter::default_filter(),
            intent: IntentFilter::default_filter(),
        })
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

    /// Project a raw user turn down to gateable human prose, dropping injected /
    /// tool / log / code lines. Returns `None` when the turn carries no human
    /// prose worth gating (it should not be elevated to a node). See
    /// [`ProseFilter`].
    #[must_use]
    pub fn human_prose(&self, text: &str) -> Option<String> {
        self.prose.clean(text)
    }

    /// Whether `text` carries any gateable human prose.
    #[must_use]
    pub fn is_human_prose(&self, text: &str) -> bool {
        self.prose.is_human_prose(text)
    }

    /// Whether `prose` expresses a real commitment worth seeding a Decision (vs.
    /// a bare question, a fragment, or third-person analysis prose). See
    /// [`IntentFilter`]. A non-committal turn still elevates a Conversation.
    #[must_use]
    pub fn is_committal(&self, prose: &str) -> bool {
        self.intent.is_committal(prose)
    }

    /// Whether any fired marker is a ban.
    #[must_use]
    pub fn is_ban(&self, markers: &[CommitmentMarker]) -> bool {
        markers.iter().any(|m| m.category == MarkerCategory::Ban)
    }

    /// Whether these fired markers should **seed a candidate Decision**, given
    /// whether the marker's session also produced an edit.
    ///
    /// - A [`Tier::Strong`] marker seeds a Decision unconditionally (the
    ///   historical behavior).
    /// - A [`Tier::Soft`] marker seeds a Decision only when `session_has_edit`
    ///   is `true` — an action request / demoted modal is confirmed by a diff,
    ///   not manufactured from chatter.
    /// - A [`MarkerCategory::Confirmation`] marker never seeds a Decision (the
    ///   single-turn gate cannot see the proposal it confirms); it still elevates
    ///   a Conversation.
    ///
    /// A marker whose `rule_id` is unknown to this gate (e.g. from a hand-built
    /// marker in a test) is treated as `Strong`, preserving prior behavior.
    #[must_use]
    pub fn seeds_decision(&self, markers: &[CommitmentMarker], session_has_edit: bool) -> bool {
        for m in markers {
            if m.category == MarkerCategory::Confirmation {
                continue;
            }
            match self.rules.iter().find(|r| r.id == m.rule_id) {
                Some(rule) => match rule.tier {
                    Tier::Strong => return true,
                    Tier::Soft => {
                        if session_has_edit {
                            return true;
                        }
                    }
                },
                // Unknown rule id → assume Strong (back-compat with hand-built markers).
                None => return true,
            }
        }
        false
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

    #[test]
    fn seeds_decision_respects_tier_and_edit() {
        let gate = CommitmentGate::default_table();

        // A Strong marker (decision_verb.use) seeds a Decision regardless of edit.
        let strong = gate.evaluate("let's use postgres for storage");
        assert!(strong.iter().any(|m| m.rule_id == "decision_verb.use"));
        assert!(gate.seeds_decision(&strong, false));
        assert!(gate.seeds_decision(&strong, true));

        // A Soft-only marker (bare modal "never") seeds ONLY when an edit confirms.
        let soft = gate.evaluate("we have never seen anything like it");
        assert!(soft.iter().any(|m| m.rule_id == "imperative.must_always_never"));
        assert!(!soft.iter().any(|m| m.category == MarkerCategory::DecisionVerb));
        assert!(!gate.seeds_decision(&soft, false), "soft + no edit must not seed");
        assert!(gate.seeds_decision(&soft, true), "soft + edit may seed");

        // A Confirmation marker never seeds a Decision, even with an edit.
        let confirm = gate.evaluate("lgtm ship it");
        assert!(confirm
            .iter()
            .all(|m| m.category == MarkerCategory::Confirmation));
        assert!(!confirm.is_empty());
        assert!(!gate.seeds_decision(&confirm, true), "confirmation never seeds");
    }
}
