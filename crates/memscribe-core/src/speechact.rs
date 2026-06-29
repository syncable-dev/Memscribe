//! Speech-act classification (L2) — is an utterance a decision-bearing COMMITMENT,
//! or a question / instruction-to-the-agent / bare assertion? Deterministic,
//! lexicon-based: Searle's illocutionary classes read off surface mood + a
//! performative-verb list. No model, no training.
//!
//! Used as a POSITIVE confirmation signal in the scored gate (a fired marker that
//! is *also* a decisional speech act is a stronger decision); it never drops a
//! candidate on its own — the structural/garbage hard-zeros do the rejecting.

/// Whether the text reads as a decisional commitment (declarative or imperative
/// with a commissive/decisional head verb), as opposed to a question or a
/// directive aimed at the addressee ("you need to…", "please…").
#[must_use]
pub fn is_decisional_act(text: &str) -> bool {
    let s = text.trim();
    let lower = s.to_ascii_lowercase();
    if lower.is_empty() || is_interrogative(&lower) || is_directive_to_addressee(&lower) {
        return false;
    }
    has_decisional_verb(&lower)
}

/// Agent self-narration — the assistant narrating its OWN process ("Let me…",
/// "I'll take this as…", "looking at…", "I need to answer…") or reporting what
/// someone said ("the user is asking…", "the developer notes…"). Pure lexical,
/// deterministic. This text is never itself a recorded decision.
///
/// NOTE: matching here is necessary but not sufficient to reject — a genuine
/// first-person choice can share a head ("let me switch to Postgres"). The scored
/// gate applies this ONLY when there is no resolved named choice, so real
/// decisions survive (that exclusion lives at the call site, which has the
/// resolved-choice signal). `"the user chose…"` is deliberately NOT listed: it
/// wraps a real user decision rather than narrating process.
/// Normalize smart apostrophes/quotes to ASCII ("I'll" → "I'll", "…" untouched)
/// so the ASCII lexicons match conversational text that uses Unicode punctuation.
fn normalize_punct(text: &str) -> String {
    text.replace(['\u{2018}', '\u{2019}', '\u{02BC}'], "'")
        .replace(['\u{201C}', '\u{201D}'], "\"")
}

/// STRONG agent narration — process/meta commentary that is NEVER a recorded
/// decision regardless of what it mentions later ("I'll take this as…", "I'm
/// going to…", "looking at…", "the developer notes…"). Unconditional drop: a
/// planning monologue that happens to name a tool ("…then use Memtrace…") is
/// still narration, not a decision.
#[must_use]
pub fn is_process_narration(text: &str) -> bool {
    let lower = normalize_punct(text).trim().to_ascii_lowercase();
    const HEADS: &[&str] = &[
        "i'll take this as", "i'll take that as", "i'll take this", "i'm going to",
        "im going to", "i need to answer", "i should ", "looking at ", "here's what",
        "heres what", "let's see", "lets see", "let me see", "let me think",
        "now let me", "now i'll", "first i'll", "i'll start", "i'll begin",
        "i'll continue", "let me check", "let me verify", "let me look",
    ];
    if HEADS.iter().any(|h| lower.starts_with(h)) {
        return true;
    }
    const PHRASES: &[&str] = &[
        "the user is asking", "the user asked", "the user wants", "the user notes",
        "the developer notes", "the developer says", "the developer wants",
        "it was a user asking", "it was the user", "so i need to answer",
        "as a user asking",
    ];
    PHRASES.iter().any(|p| lower.contains(p))
}

/// SOFT agent narration — a head that COULD precede a genuine first-person choice
/// ("let me switch to Postgres", "I'll use Redis"). The scored gate drops these
/// only when there is no resolved named choice (that exclusion is applied at the
/// call site, which has the resolved-choice signal).
#[must_use]
pub fn is_agent_narration(text: &str) -> bool {
    let lower = normalize_punct(text).trim().to_ascii_lowercase();
    const SOFT_HEADS: &[&str] = &["let me ", "i'll "];
    SOFT_HEADS.iter().any(|h| lower.starts_with(h))
}

/// Interrogative mood: ends with '?' or opens with a wh-/aux- fronted question.
fn is_interrogative(lower: &str) -> bool {
    if lower.ends_with('?') {
        return true;
    }
    const OPENERS: &[&str] = &[
        "what ", "why ", "how ", "when ", "where ", "who ", "which ", "is ", "are ",
        "can ", "could ", "should ", "would ", "do ", "does ", "did ", "will ",
        "shall we", "should we", "can we", "could we",
    ];
    OPENERS.iter().any(|o| lower.starts_with(o))
}

/// A directive aimed at the addressee — an instruction, not a recorded decision.
fn is_directive_to_addressee(lower: &str) -> bool {
    const DIRECTIVE: &[&str] = &[
        "you need", "you should", "you must", "you can ", "you have to", "please ",
        "let's ", "lets ", "can you", "could you", "make sure", "go and ",
        "go ahead", "spin up", "i want you", "i need you", "we want you",
    ];
    DIRECTIVE.iter().any(|d| lower.contains(d))
}

/// A commissive/decisional head verb — the kind of verb that records a choice.
fn has_decisional_verb(lower: &str) -> bool {
    // Leading verb (imperative / commit-subject mood) OR anywhere as a phrase.
    const DECISIONAL: &[&str] = &[
        "decide", "decided", "adopt", "adopted", "choose", "chose", "chosen",
        "pick", "picked", "switch", "switched", "migrate", "migrated", "move to",
        "moved to", "replace", "replaced", "drop", "dropped", "remove", "removed",
        "deprecate", "deprecated", "disable", "disabled", "default to", "defaults to",
        "standardize on", "settle on", "go with", "went with", "use ", "using ",
        "introduce", "introduced", "enable", "enabled", "add ", "added", "implement",
        "implemented", "refactor", "refactored", "rework", "rewrite", "rebuild",
        "wire ", "wired", "port ", "ported", "ship ", "shipped", "land ", "gate ",
        "split ", "merge ", "rename", "renamed", "bound ", "cache ", "will use",
        "will not", "won't", "we will", "we'll", "resolves", "resolve ",
    ];
    DECISIONAL.iter().any(|v| {
        lower.starts_with(v) || lower.contains(&format!(" {v}")) || lower.contains(&format!(" {}", v.trim()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisional_statements_are_acts() {
        for s in [
            "Use Postgres instead of MySQL",
            "adopt RaBitQ for vector compression",
            "switch to pure Vite SPA",
            "drop the legacy v1 REST API",
            "we will not use chrono for date parsing",
            "rebuild the notify pipeline against MemDB",
        ] {
            assert!(is_decisional_act(s), "should be decisional: {s}");
        }
    }

    #[test]
    fn questions_and_instructions_are_not_acts() {
        for s in [
            "Should we use Postgres?",
            "why is the index slow",
            "you need to fix this",
            "please switch to Postgres",
            "can you add a doctor command",
            "let's drop MySQL",
            "I want you to be brutal",
        ] {
            assert!(!is_decisional_act(s), "should NOT be a decisional act: {s}");
        }
    }

    #[test]
    fn deterministic() {
        let s = "adopt RaBitQ for vector compression";
        assert_eq!(is_decisional_act(s), is_decisional_act(s));
    }
}
