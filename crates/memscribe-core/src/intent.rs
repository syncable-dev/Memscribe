//! Semantic commitment-intent gate (the layer below turn-source hygiene).
//!
//! [`crate::prose::ProseFilter`] removes content that is not human prose (tool
//! plumbing, injected system/skill text, logs, code dumps). What remains can
//! still be human-authored-but-non-decision text: a bare question with no
//! commitment ("do I need to run db:push?"), a sentence fragment ("we can use ."),
//! or third-person analysis prose describing code ("the React layer never
//! re-measures"). The real-data audit found this **semantic** class is the
//! residual precision bottleneck once plumbing is gone.
//!
//! [`IntentFilter`] is a **pure, deterministic** classifier: given the human-prose
//! projection of a gated turn it decides whether the turn expresses a real
//! commitment worth **seeding a Decision**. A non-committal turn still elevates a
//! Conversation (the dialogue is kept) — only the heavier Decision is withheld.
//!
//! The classifier is intentionally recall-protective: a directed request
//! ("can you …", "please …", "let's …") or a turn-initial imperative command
//! ("Add …", "Use …", "Fix …") always commits. Demotion only applies to the three
//! junk classes, and never to an explicit request — so the Zed case
//! ("… can you fix that or add 2 new test cases") is kept even though it contains
//! a question mark.

use regex::Regex;

/// A deterministic commitment-intent classifier.
#[derive(Debug)]
pub struct IntentFilter {
    /// A directed request to the assistant ("can you", "please", "let's", "i want you to").
    request_leadin: Regex,
    /// A turn that *begins* with an imperative action/decision verb ("Add", "Use", "Fix").
    imperative_start: Regex,
    /// A subordinate-clause opener ("but", "since", "so", "and", "because").
    subordinate_lead: Regex,
    /// First-person / collective subject ("I", "we", "us", "our", "let's").
    first_person: Regex,
    /// Second-person subject ("you", "your").
    second_person: Regex,
    /// A third-person narration opener ("the", "this", "it", "they", "both", "each").
    third_person_lead: Regex,
    /// A turn opening with a bare code/file-extension token ("rs …", "gl's …",
    /// "yml:…") — a fragment sliced out of a pasted code path / analysis line.
    code_lead: Regex,
    /// Code-identifier density (camelCase, snake_case, `::`, backticks, file refs).
    code_ident: Regex,
    /// A turn that OPENS by describing code behavior ("It now hands …", "this
    /// caps it …", "React will try to recreate …") — pasted analysis, not a decision.
    code_describe: Regex,
    /// A doc / list intro ("Four modes …:", "… add to your MCP config:", a bullet).
    doc_intro: Regex,
    /// A "Title — lowercase description" marketing/doc tagline.
    tagline: Regex,
    /// A vague affirmation that trails off on a bare demonstrative ("Yes we need
    /// to do that.", "Thats what we need to fix.").
    vague_affirm: Regex,
    /// An infinitive-fragment opener ("to ensure it works …") sliced from a
    /// larger sentence.
    infinitive_lead: Regex,
    /// Below this many alphabetic words a turn is treated as a fragment.
    min_alpha_words: usize,
    /// A subordinate-led turn shorter than this is a trailing-off fragment.
    subordinate_max_words: usize,
}

impl Default for IntentFilter {
    fn default() -> Self {
        Self::default_filter()
    }
}

impl IntentFilter {
    /// Build the default classifier.
    ///
    /// # Panics
    /// Never in practice — the patterns are compile-time constants exercised by
    /// tests; a malformed default is a build-breaking bug.
    #[must_use]
    pub fn default_filter() -> Self {
        let rx = |p: &str| Regex::new(p).expect("intent pattern must compile");
        IntentFilter {
            request_leadin: rx(
                r"(?i)\b(?:can|could|would|will)\s+you\b|\bplease\b|\bpls\b|\blet'?s\b|\blet us\b|\bgo ahead\b|\bi\s*(?:'?d like|want|need)\s+you\s+to\b|\bhow about you\b|\bcould we\b|\bwhy don'?t we\b",
            ),
            imperative_start: rx(
                r"(?i)^\s*(?:add|create|implement|build|write|generate|scaffold|stub|fix|change|update|modify|adjust|tweak|patch|correct|resolve|refactor|rename|extract|inline|remove|delete|drop|strip|prune|use|using|adopt|switch|migrate|standardi[sz]e|optimi[sz]e|improve|harden|saniti[sz]e|ensure|make|set|bump|upgrade|downgrade|test|run|help|investigate|check|review|keep|move|split|merge|consolidate|clean|wire|hook|install|enable|disable|swap|replace|substitute|handle|support|skip|deprecate|retire|imagine|spin|let'?s|please|just\s+make)\b",
            ),
            subordinate_lead: rx(
                r"(?i)^\s*(?:but|since|so|and|or|because|although|though|while|whereas|if|when|then)\b",
            ),
            first_person: rx(
                r"(?i)\b(?:i|i'?m|i'?ve|i'?ll|i'?d|we|we'?re|we'?ve|we'?ll|us|our|my|me|let'?s)\b",
            ),
            second_person: rx(r"(?i)\b(?:you|you'?re|you'?ve|you'?ll|your|yourself)\b"),
            third_person_lead: rx(
                r"(?i)^\s*(?:the|this|that|it|its|they|their|these|those|both|each|all|every|a|an)\b",
            ),
            code_lead: rx(
                r"(?i)^\s*(?:rs|ts|tsx|js|jsx|py|go|rb|kt|swift|yml|yaml|toml|json|md|vscdb|gl|fts|sql|ann|hnsw)\b",
            ),
            code_ident: rx(
                r"(?:::|`[^`]+`|\b[a-z][a-zA-Z0-9]*[A-Z][a-zA-Z0-9]*\b|\b[a-z][a-z0-9]*_[a-z0-9_]+\b|\b[a-z]{1,3}:\d|\.[a-z]{1,4}\b|/[A-Za-z])",
            ),
            // OPENS with a third-person subject + a code-behavior verb within a few
            // words. Anchored at start so it only fires on description-led turns,
            // not on a decision that incidentally mentions code.
            code_describe: rx(
                r"(?i)^\s*(?:the\s+[a-z]+|it|this|that|these|those|[A-Z][a-zA-Z]{2,})\s+(?:\w+\s+){0,3}?(?:hand|recreate|enumerate|cap|caps|disable|enable|return|render|implement|store|fetch|wrap|hold|emit|yield|reassign|process|allocate|freeze|re-?measure|fall\s+back)s?\b",
            ),
            doc_intro: rx(
                r"(?i)(?:^|\s)\*\s|\b(?:config|modes?|options?|steps?|the following|example)\b[^:\n]{0,40}:\s*$",
            ),
            tagline: rx(r"^\s*[A-Z][\w./-]*(?:\s+[A-Z][\w./-]*){0,3}\s+[—-]\s+[a-z]"),
            vague_affirm: rx(
                r"(?i)^\s*(?:(?:yes|yeah|yep|yup|thats?|that'?s|this is|so that'?s)\b[^.!?\n]{0,40}\b(?:that|this|it)\s*[.!]*\s*$|(?:thats?|that'?s|this is)\s+what\b)",
            ),
            infinitive_lead: rx(r"(?i)^\s*to\s+[a-z]"),
            min_alpha_words: 4,
            subordinate_max_words: 8,
        }
    }

    /// Whether `prose` expresses a real commitment worth seeding a Decision.
    /// **Pure**: depends only on `prose`.
    #[must_use]
    pub fn is_committal(&self, prose: &str) -> bool {
        let t = prose.trim();

        // 1. A directed request to the assistant always commits — even if the
        //    turn also contains a question ("…? can you fix that").
        if self.request_leadin.is_match(t) {
            return true;
        }

        // 2. A bare question (no directed request) is non-committal: the user is
        //    asking, not deciding. We only treat a trailing '?' as the reliable
        //    interrogative signal (leading "Do…"/"Is…" is ambiguous with
        //    imperatives like "Do NOT edit").
        if t.ends_with('?') {
            return false;
        }

        // 3. A turn that begins with an imperative action/decision verb is a
        //    command ("Add a healthcheck", "Use Postgres", "Do NOT edit").
        if self.imperative_start.is_match(t) {
            return true;
        }

        let words = alpha_word_count(t);

        // 4. Too short → a fragment, not a decision ("we can use .", "I need to ensure .").
        if words < self.min_alpha_words {
            return false;
        }

        // 5. A short subordinate-clause turn is a trailing-off fragment
        //    ("but the bottom should be code somehow", "since I need to scroll").
        if self.subordinate_lead.is_match(t) && words < self.subordinate_max_words {
            return false;
        }

        // 6. Third-person analysis prose: no first/second-person subject AND it
        //    either opens with a third-person determiner or is code-identifier
        //    dense ("the React layer never re-measures", "enumerates Repository
        //    rows using a kind_label filter"). This is pasted agent analysis, not
        //    a human decision.
        let has_person = self.first_person.is_match(t) || self.second_person.is_match(t);
        if !has_person
            && (self.third_person_lead.is_match(t)
                || self.code_lead.is_match(t)
                || self.code_ident.is_match(t))
        {
            return false;
        }

        // 7. Pasted agent-analysis that OPENS by describing code behavior
        //    ("It now hands the renderer …", "this caps it at Info …",
        //    "React will try to recreate this tree …"). Fires even with a person,
        //    because it is anchored to a description-led opening.
        if self.code_describe.is_match(t) {
            return false;
        }

        // 8. Doc / list intro ("Four modes …:", "… add to your MCP config:", a bullet).
        if self.doc_intro.is_match(t) {
            return false;
        }

        // 9. A "Title — lowercase description" marketing/doc tagline (no person).
        if !has_person && self.tagline.is_match(t) {
            return false;
        }

        // 10. A vague affirmation trailing off on a bare demonstrative
        //     ("Yes we need to do that.", "Thats what we need to fix.").
        if self.vague_affirm.is_match(t) {
            return false;
        }

        // 11. An infinitive-fragment opener sliced from a larger sentence
        //     ("to ensure it works …").
        if self.infinitive_lead.is_match(t) {
            return false;
        }

        // 12. A declarative first/second-person statement with substance commits
        //     ("we will use memtrace fleet", "We need to build a way to MINT keys").
        true
    }
}

/// Count whitespace tokens carrying at least two ASCII-alphabetic characters.
fn alpha_word_count(s: &str) -> usize {
    s.split_whitespace()
        .filter(|w| w.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 2)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f() -> IntentFilter {
        IntentFilter::default_filter()
    }

    /// Real captured turns that ARE commitments — must be kept (recall guard).
    #[test]
    fn keeps_genuine_commitments() {
        let g = f();
        for t in [
            // the Zed case — a question mark, but a directed request
            "Wasn't there a test case which just failed ? can you fix that or add 2 new test cases",
            "Can you write a quick guideline for how to use the workspace concept for memtrace ?",
            "I want you to install it so we can test the new GraphCanvas and make sure everything still works",
            "We need to build in a way for us to MINT license keys people can redeem / claim .",
            "now please git commit and push your changes so I can merge to main",
            "We need to fix that - and also I want a ROI or something page where we can get stats",
            "Will you please ultrathink and ultracode how you solve this ensuring best in class support for kotlin",
            "just make sure that the graph doesn't get weird to look at when zoomed out",
            // turn-initial imperatives
            "Add organization tables to drizzle schema OR use BA migrate/generate pattern",
            "Use memtrace service uninstall to remove autostart.",
            "remove the benchmarks keep it simple man and use a language everyone understands",
            "Test all these changes and ensure we have TDD and property testing on it",
            "Do NOT edit the plan file.",
            "Help me swap our whole Landing page with this new memtrace landing page .",
            "We would like to refactor swift AST parsing, can you investigate and how we could refactor this function ?",
            // declarative first-person decisions
            "we will use memtrace fleet which you have as skill",
            "We need to add another feature, a gift feature .",
            "I told you its one product this is so confusing and needs to be deleted",
            // recall guards for the new demotion rules
            "Yes, use Postgres for the orders service",
            "now please git commit and push your changes so I can merge to main",
            "Use `module.registerHooks()` instead.",
            "use TenantContext or AgentContext::from_env where easy",
        ] {
            assert!(g.is_committal(t), "should KEEP commitment: {t:?}");
        }
    }

    /// Real captured turns that are NOT commitments — must be demoted.
    #[test]
    fn demotes_questions_fragments_and_analysis() {
        let g = f();
        for t in [
            // class 1 — bare interrogatives with no directed request
            "do I need to run bun run db:push / db:push:prod can i do that ?",
            "So we don't need to do anything about it ?",
            "The top bar is not supposed to be shown on landing pages ?",
            "Seriously I've never seen such an ugly landing page ?",
            "So what this never got released or ?",
            // class 2 — fragments / trailing-off one-liners
            "we can use .",
            "I need to ensure .",
            "but the bottom should be code somehow .",
            "since I need to scroll on screen .",
            "rs just never adopted it.",
            // class 3 — third-person analysis prose describing code
            "gl's own internal resize handling; the React layer never re-measures or calls a resize on panel change",
            "rs:1342-1372)** enumerates Repository rows using a kind_label: Repository filter",
            "this apeared and never fucking resolves .",
            // class 4 — pasted code-description openers
            "It now hands the renderer zero-copy typed arrays.",
            "this caps it at Info and prevents auto-filed issues",
            "React will try to recreate this component tree from scratch using the error boundary you provided",
            // class 5 — doc / list intros, taglines, vague affirmations, infinitive fragments
            "Four modes, you pick how far it goes: * observe (default)",
            "To use with Claude / Cursor, add to your MCP config:",
            "Memtrace Rail — agents stop grepping, start using the graph",
            "Yes we need to do that .",
            "Thats what we need to fix .",
            "to ensure it fucking also works then",
        ] {
            assert!(!g.is_committal(t), "should DEMOTE non-commitment: {t:?}");
        }
    }

    #[test]
    fn is_pure_and_repeatable() {
        let g = f();
        let t = "can you fix that or add 2 new test cases";
        assert_eq!(g.is_committal(t), g.is_committal(t));
    }
}
