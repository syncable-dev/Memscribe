//! Deterministic polarity & contrast analysis — the "negation-scope" layer (L3).
//!
//! Decides whether a decision is a PROHIBITION (a ban: "drop X", "we will not use
//! Y") or a POSITIVE choice ("use X instead of Y", "switch to Z"), and orients the
//! chosen-vs-rejected alternatives for contrast constructs. Zero-LLM: every signal
//! is a hand-authored cue table, and the result is a pure function of the text.
//!
//! Three ideas from the classical literature, without a parser:
//! - **Contrast pivot** ("X instead of Y", "from Y to X", "replace Y with X") is a
//!   SUBSTITUTION — a positive choice of X with Y as the rejected alternative —
//!   NEVER a ban. (This is the bug that flipped "resolve cargo to rustup-init
//!   instead of the installed cargo" into a ban.)
//! - **NegEx-style scope**: a ban exists only when a removal verb LEADS the clause
//!   (the primary act is removal) or an explicit prohibition phrase is present; the
//!   banned thing is the verb/phrase's object.
//! - **Pseudo-negation suppression**: phrases that look negative but assert no
//!   prohibition ("no change to", "not sure if", "cannot rule out") never produce a
//!   ban — the single biggest precision lever against false bans.

use crate::node::Opt;

/// The polarity verdict for one decision sentence.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Polarity {
    /// True iff the decision's primary act is removal/prohibition.
    pub is_ban: bool,
    /// Chosen/rejected alternatives, correctly oriented. For a ban this is the
    /// single ruled-out target; for a positive contrast it is [chosen, rejected];
    /// empty when no clean structure is found.
    pub options: Vec<Opt>,
}

/// Contrast pivots: `<chosen> PIVOT <rejected>` — a positive substitution.
const INSTEAD_PIVOTS: &[&str] = &[
    " instead of ",
    " rather than ",
    " in favor of ",
    " in place of ",
    " over using ",
];

/// Explicit prohibitions — a ban regardless of position.
const PROHIBIT: &[&str] = &[
    "will not use",
    "won't use",
    "will no longer use",
    "no longer use",
    "stop using",
    "never use",
    "must not use",
    "do not use",
    "we will not",
    "we won't",
];

/// Removal verbs that make a ban only when they LEAD the clause (primary action).
const REMOVAL_LEAD: &[&str] = &[
    "drop", "dropped", "remove", "removed", "delete", "deleted", "deprecate",
    "deprecated", "disable", "disabled", "ban", "banned", "kill", "killed",
    "strip", "stripped", "purge", "purged", "revert", "reverted", "eliminate",
    "eliminated", "forbid", "forbidden", "retire", "retired",
];

/// Phrases that look negative but assert no prohibition — suppress false bans.
const PSEUDO_NEGATION: &[&str] = &[
    "no change", "not certain", "not sure", "cannot rule out", "can't rule out",
    "not clear", "no need to remove", "without removing", "instead of removing",
    "rather than removing", "no longer needed?",
];

/// Imperative decision verbs that can HEAD an independent decision clause. Used by
/// the bundle splitter to decide whether a coordinated segment is itself a
/// complete decision (so "use X, drop Y, add Z" splits into three) versus a mere
/// object in a list ("add auth client, Convex client, …" stays one decision).
const DECISION_VERB_HEAD: &[&str] = &[
    "use", "add", "drop", "remove", "switch", "adopt", "replace", "migrate",
    "wire", "port", "rebuild", "rename", "split", "merge", "move", "enable",
    "disable", "introduce", "expose", "make", "keep", "cache", "gate", "persist",
    "bypass", "resolve", "fix", "update", "refactor", "implement", "create",
    "delete", "forward", "strip", "purge", "revert", "require", "allow", "ship",
    "default", "deprecate", "support", "store", "swap", "pin", "bound",
];

/// Is `seg` an independent decision clause (starts with a decision verb, or is an
/// explicit "X instead of Y" / "from A to B" / "replace … with" choice)?
fn is_decision_clause(seg: &str) -> bool {
    let s = seg.trim();
    if s.split_whitespace().count() < 2 {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    let head = lower
        .split(|c: char| !c.is_ascii_alphabetic())
        .find(|w| !w.is_empty())
        .unwrap_or("");
    if DECISION_VERB_HEAD.contains(&head) {
        return true;
    }
    lower.contains(" instead of ") || lower.contains(" rather than ")
}

/// **L2 — bundle splitter.** A single commit subject often bundles several
/// decisions ("use SystemTime instead of chrono, drop the cron job, and add a
/// retry"). Split a coordinated list (`,` / ` + ` / ` and `) into its parts ONLY
/// when EVERY part is independently a complete decision clause — otherwise it's
/// one decision with a detail list ("add auth client, Convex client, …") and is
/// returned whole. Deterministic, conservative: never invents or mangles.
#[must_use]
pub fn split_coordinated(text: &str) -> Vec<String> {
    let t = text.trim();
    // Don't split inside brackets/braces ("/api/{a,b,c}") — replace their commas.
    if t.contains('{') || t.contains('(') || t.contains('[') {
        return vec![t.to_string()];
    }
    // Segment on the three coordinators, in priority order.
    let mut segs: Vec<String> = Vec::new();
    for part in t.split(',') {
        for p2 in part.split(" + ") {
            for p3 in p2.split(" and ") {
                let s = p3.trim().trim_end_matches('.').trim();
                if !s.is_empty() {
                    segs.push(s.to_string());
                }
            }
        }
    }
    if segs.len() < 2 {
        return vec![t.to_string()];
    }
    if segs.iter().all(|s| is_decision_clause(s)) {
        segs
    } else {
        vec![t.to_string()]
    }
}

/// Analyse the polarity of a decision sentence. Pure.
#[must_use]
pub fn analyze_polarity(text: &str) -> Polarity {
    let lower = text.to_ascii_lowercase();

    // Pseudo-negation guard: looks negative but is not a prohibition.
    let pseudo = PSEUDO_NEGATION.iter().any(|p| lower.contains(p));

    // 1. Explicit prohibition anywhere (unless suppressed) → ban; target = object.
    if !pseudo {
        for p in PROHIBIT {
            if let Some(idx) = lower.find(p) {
                let after = &text[(idx + p.len()).min(text.len())..];
                let target = clip_clause(after.trim_start_matches([' ', ':']));
                return ban(target);
            }
        }
    }

    // 2. Contrast pivot → POSITIVE substitution (never a ban), oriented chosen/rejected.
    for pivot in INSTEAD_PIVOTS {
        if let Some(idx) = lower.find(pivot) {
            let left = &text[..idx];
            let right = &text[(idx + pivot.len()).min(text.len())..];
            let chosen = chosen_head(left);
            let rejected = clip_clause(right);
            return positive(chosen, rejected);
        }
    }
    // "<verb> from <rejected> to <chosen>" (switch/migrate/move from Y to X)
    if let Some(from) = lower.find(" from ") {
        let after = from + " from ".len();
        if let Some(to_rel) = lower[after.min(lower.len())..].find(" to ") {
            let to = after + to_rel;
            let rejected = clip_clause(&text[after.min(text.len())..to.min(text.len())]);
            let chosen = clip_clause(&text[(to + " to ".len()).min(text.len())..]);
            if !rejected.is_empty() && !chosen.is_empty() {
                return positive(chosen, rejected);
            }
        }
    }
    // "replace <rejected> with <chosen>"
    if let Some(rep) = lower.find("replace ") {
        let after = rep + "replace ".len();
        if let Some(with_rel) = lower[after.min(lower.len())..].find(" with ") {
            let with = after + with_rel;
            let rejected = clip_clause(&text[after.min(text.len())..with.min(text.len())]);
            let chosen = clip_clause(&text[(with + " with ".len()).min(text.len())..]);
            if !rejected.is_empty() && !chosen.is_empty() {
                return positive(chosen, rejected);
            }
        }
    }

    // 3. Primary-removal-verb lead (no contrast, no prohibition phrase) → ban.
    if !pseudo {
        let first = lower
            .split(|c: char| !c.is_ascii_alphabetic())
            .find(|w| !w.is_empty())
            .unwrap_or("");
        if REMOVAL_LEAD.contains(&first) {
            // The banned target = the rest of the clause after the lead verb.
            let rest = text
                .trim_start()
                .splitn(2, char::is_whitespace)
                .nth(1)
                .unwrap_or("");
            return ban(clip_clause(rest));
        }
    }

    // 4. No prohibition and no clean contrast — a plain positive decision.
    Polarity { is_ban: false, options: Vec::new() }
}

/// A ban verdict carrying its single ruled-out target (dropped if junk).
fn ban(target: String) -> Polarity {
    let options = if target.is_empty() || target.len() > 60 {
        Vec::new()
    } else {
        vec![Opt { text: target, chosen: true }]
    };
    Polarity { is_ban: true, options }
}

/// A positive-choice verdict with [chosen, rejected] (dropped if either is junk).
fn positive(chosen: String, rejected: String) -> Polarity {
    let options = if chosen.is_empty() || rejected.is_empty() || chosen.len() > 60 || rejected.len() > 60 {
        Vec::new()
    } else {
        vec![
            Opt { text: chosen, chosen: true },
            Opt { text: rejected, chosen: false },
        ]
    };
    Polarity { is_ban: false, options }
}

/// The chosen alternative on the LEFT of a contrast pivot. Prefer the object of a
/// trailing "to"/"with" ("resolves cargo to rustup-init" → "rustup-init"); else the
/// object of a leading choose-verb ("use Postgres" → "Postgres"); else the clause.
fn chosen_head(left: &str) -> String {
    let lc = left.to_ascii_lowercase();
    if let Some(i) = lc.rfind(" to ") {
        let tail = clip_clause(&left[(i + " to ".len()).min(left.len())..]);
        if !tail.is_empty() {
            return tail;
        }
    }
    if let Some(i) = lc.rfind(" with ") {
        let tail = clip_clause(&left[(i + " with ".len()).min(left.len())..]);
        if !tail.is_empty() {
            return tail;
        }
    }
    const CHOOSE_VERBS: &[&str] = &["use ", "adopt ", "switch to ", "choose ", "pick ", "go with "];
    for v in CHOOSE_VERBS {
        if let Some(i) = lc.find(v) {
            let tail = clip_clause(&left[(i + v.len()).min(left.len())..]);
            if !tail.is_empty() {
                return tail;
            }
        }
    }
    clip_clause(left)
}

/// Trim a clause to a short noun-ish phrase: cut at the first clause boundary and
/// strip wrapping punctuation. Keeps an option label tidy.
fn clip_clause(s: &str) -> String {
    let s = s.trim();
    let end = s
        .find([',', ';', '.', '('])
        .or_else(|| s.find(" for "))
        .or_else(|| s.find(" because "))
        .or_else(|| s.find(" since "))
        .or_else(|| s.find(" so "))
        .unwrap_or(s.len());
    s[..end].trim().trim_matches(['"', '`', '\'', ':']).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instead_of_is_a_positive_choice_not_a_ban() {
        let p = analyze_polarity("resolves `cargo` to `rustup-init` instead of the installed cargo");
        assert!(!p.is_ban, "‘instead of’ is a substitution, never a ban");
        let chosen: Vec<_> = p.options.iter().filter(|o| o.chosen).map(|o| o.text.as_str()).collect();
        let rejected: Vec<_> = p.options.iter().filter(|o| !o.chosen).map(|o| o.text.as_str()).collect();
        assert_eq!(chosen, vec!["rustup-init"], "chosen = the resolution target");
        assert_eq!(rejected, vec!["the installed cargo"]);
    }

    #[test]
    fn use_x_instead_of_y_orients_correctly() {
        let p = analyze_polarity("Use Postgres instead of MySQL for the orders service");
        assert!(!p.is_ban);
        assert_eq!(p.options[0], Opt { text: "Postgres".into(), chosen: true });
        assert_eq!(p.options[1], Opt { text: "MySQL".into(), chosen: false });
    }

    #[test]
    fn switch_from_to_orients_rejected_then_chosen() {
        let p = analyze_polarity("switch from Myers to histogram diff");
        assert!(!p.is_ban);
        assert_eq!(p.options[0].text, "histogram diff");
        assert!(p.options[0].chosen);
        assert_eq!(p.options[1].text, "Myers");
        assert!(!p.options[1].chosen);
    }

    #[test]
    fn replace_y_with_x() {
        let p = analyze_polarity("replace Clerk with Better Auth");
        assert!(!p.is_ban);
        assert_eq!(p.options[0].text, "Better Auth");
        assert_eq!(p.options[1].text, "Clerk");
    }

    #[test]
    fn leading_removal_verb_is_a_ban() {
        let p = analyze_polarity("drop cross-compilation targets (ort-sys incompatible)");
        assert!(p.is_ban);
        assert_eq!(p.options, vec![Opt { text: "cross-compilation targets".into(), chosen: true }]);
    }

    #[test]
    fn explicit_prohibition_is_a_ban() {
        for s in [
            "we will not use chrono for date parsing",
            "no longer use the Python parser",
            "stop using the global mutex",
        ] {
            assert!(analyze_polarity(s).is_ban, "prohibition should ban: {s}");
        }
    }

    #[test]
    fn pseudo_negation_does_not_ban() {
        for s in [
            "no change to the auth layer this release",
            "not sure if we should remove the cache",
            "cannot rule out a Postgres migration later",
        ] {
            assert!(!analyze_polarity(s).is_ban, "pseudo-negation must not ban: {s}");
        }
    }

    #[test]
    fn positive_non_contrast_has_no_ban_no_options() {
        let p = analyze_polarity("add deviceCodes and graphSnapshots tables to schema");
        assert!(!p.is_ban);
        assert!(p.options.is_empty());
    }

    #[test]
    fn deterministic() {
        let s = "Use Postgres instead of MySQL";
        assert_eq!(analyze_polarity(s), analyze_polarity(s));
    }

    #[test]
    fn bundle_splitter_splits_multi_clause_keeps_lists() {
        // Every segment is its own decision clause → split into N.
        let parts = split_coordinated("use SystemTime instead of chrono, drop the cron job, and add a retry");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "use SystemTime instead of chrono");
        assert_eq!(parts[2], "add a retry");
        // "add [X, Y, Z]" — only the first segment is verb-led → ONE decision.
        assert_eq!(
            split_coordinated("add auth client, Convex client, ConvexBetterAuthProvider"),
            vec!["add auth client, Convex client, ConvexBetterAuthProvider".to_string()]
        );
        // noun-phrase bundle → not split.
        assert_eq!(split_coordinated("BM25 + Tantivy + RRF update").len(), 1);
        // brace expansion never split.
        assert_eq!(split_coordinated("/api/device/{code,poll,auth,heartbeat} routes").len(), 1);
        // a plain single decision is returned as-is.
        assert_eq!(split_coordinated("drop cross-compilation targets").len(), 1);
    }
}
