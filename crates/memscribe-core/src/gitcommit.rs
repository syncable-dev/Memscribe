//! The git-commit decision oracle.
//!
//! A commit message is a deterministic, high-signal decision source — arguably
//! the *strongest* one, because the touched files ARE the code the decision
//! shaped. Yet a plain git-replay only produces code episodes; the decisions an
//! engineer wrote down in their commit messages are dropped on the floor. This
//! module catches them.
//!
//! Two zero-LLM signals, both pure functions of `(subject, body)`:
//! - **Conventional Commits** semantics — a `feat`/`refactor`/`perf` type is an
//!   architectural choice; a `!` breaking marker, a `BREAKING CHANGE:` footer, or
//!   a `revert:` are *explicit* decisions (the strongest tier).
//! - **Decision phrasing** anywhere in the message — "X instead of Y", "switch
//!   to", "migrate to", "adopt", "drop", "decided to", "in favor of". These are
//!   exactly the words people use when they record a decision, conventional
//!   prefix or not.
//!
//! Housekeeping (`chore`/`docs`/`style`/`test`/`ci`/`build`), merges, WIP, and
//! bare `fix:`-without-rationale commits are deliberately *not* decisions: the
//! whole point is precision, not recall-at-any-cost.
//!
//! The miner reuses the existing [`DefaultBinder`] + [`DefaultNodePrep`] by
//! building a [`Segmentation`] directly — one synthetic session per commit
//! (`git:<sha>`), so a commit's decision binds only to that commit's own file
//! episodes and never leaks across commits.

use crate::binder::{Binder, DefaultBinder};
use crate::model::{Diff, GitRef};
use crate::node::{
    CodeEpisode, DecisionRecord, FactStatus, NodeId, Opt, PreparedNode,
};
use crate::nodeprep::{DefaultNodePrep, NodePrep};
use crate::segmenter::{DecisionCandidate, EpisodeRecord, Segmentation};
use std::path::PathBuf;
use time::{Duration, OffsetDateTime};

/// A single mined commit: identity, message, touched files, committer epoch.
/// This is the git2/`git log`-agnostic input to the oracle — the I/O layer
/// (CLI) fills it; the oracle stays pure and unit-testable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInput {
    /// Full commit SHA (the stable id).
    pub sha: String,
    /// The commit subject (first line of the message).
    pub subject: String,
    /// The commit body (everything after the subject), possibly empty.
    pub body: String,
    /// Forward-slash file paths touched by the commit, sorted, deduped.
    pub files: Vec<String>,
    /// Committer epoch seconds (the decision's wall-clock anchor).
    pub epoch: i64,
    /// Commit author identity ("Name <email>") — the per-decision attribution
    /// (Teams "who"). Empty when unknown; the decision then carries no author.
    pub author: String,
}

/// The deterministic classification of a commit as a decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDecision {
    /// The decision sentence (conventional prefix stripped; verbatim otherwise).
    pub epitome: String,
    /// The tier the signal justifies.
    pub fact_status: FactStatus,
    /// True when the decision removes/forbids something ("drop", "no longer").
    pub is_ban: bool,
    /// Options parsed from "X instead of Y" / "from Y to X" / "replace Y with X".
    pub options: Vec<Opt>,
}

/// Cap the file episodes a single commit contributes, so one mega-refactor does
/// not bind a decision to hundreds of files (the over-attribution failure mode).
/// Files are pre-sorted, so the kept set is deterministic.
const MAX_FILES_PER_COMMIT: usize = 40;

/// The decision-phrasing lexicon. Lowercased substring match against the whole
/// message. Ordered longest-intent-first only for readability; matching is a
/// plain `contains`, so order is irrelevant to the result.
const DECISION_PHRASES: &[&str] = &[
    "instead of",
    "rather than",
    "in favor of",
    "switch to",
    "switched to",
    "switching to",
    "switch from",
    "switched from",
    "switching from",
    "migrate to",
    "migrated to",
    "migrating to",
    "migrate from",
    "migrated from",
    "migrating from",
    "move to ",
    "moved to ",
    "moved from",
    "ported to",
    "ported from",
    "replace ",
    "replaced ",
    "adopt",
    "decided to",
    "decision:",
    "we will not",
    "we won't",
    "won't use",
    "will no longer",
    "no longer use",
    "chose ",
    "choose ",
    "going with",
    "go with ",
    "deprecate",
    "standardize on",
    "settle on",
    "drop ",
    "dropped ",
];

/// Conventional-commit *types* that are decisions on their own (architectural).
const ARCH_TYPES: &[&str] = &["feat", "refactor", "perf"];

/// Conventional-commit *types* that are housekeeping — never a decision unless
/// the message also carries explicit decision phrasing.
const HOUSEKEEPING_TYPES: &[&str] = &["chore", "docs", "style", "test", "ci", "build"];

/// Prefixes that disqualify a subject outright (not decisions).
const SKIP_PREFIXES: &[&str] = &[
    "merge ", "merge branch", "merge pull", "wip", "bump ", "release ", "version ",
    "v0.", "v1.", "v2.", "fixup!", "squash!", "amend",
];

/// Classify a commit message as a decision, or `None` for housekeeping/trivial.
///
/// Pure: the result depends only on `(subject, body)`.
#[must_use]
pub fn classify_commit(subject: &str, body: &str) -> Option<GitDecision> {
    let subject = subject.trim();
    if subject.is_empty() {
        return None;
    }
    let subject_lc = subject.to_ascii_lowercase();

    // Hard skips: merges, WIP, version bumps, fixup/squash.
    if SKIP_PREFIXES.iter().any(|p| subject_lc.starts_with(p)) {
        return None;
    }

    let (commit_type, breaking_bang, has_scope, cleaned_subject) = parse_conventional(subject);
    let whole_lc = format!("{}\n{}", subject_lc, body.to_ascii_lowercase());

    let breaking_footer = body.contains("BREAKING CHANGE") || body.contains("BREAKING-CHANGE");
    let is_revert = commit_type.as_deref() == Some("revert")
        || subject_lc.starts_with("revert ")
        || subject_lc.starts_with("revert\"")
        || body.contains("This reverts commit");
    let has_phrase = DECISION_PHRASES.iter().any(|p| whole_lc.contains(p));

    let arch = commit_type
        .as_deref()
        .is_some_and(|t| ARCH_TYPES.contains(&t));
    let housekeeping = commit_type
        .as_deref()
        .is_some_and(|t| HOUSEKEEPING_TYPES.contains(&t));

    // Tiering — strongest signal wins.
    let fact_status = if breaking_bang || breaking_footer || is_revert || has_phrase {
        FactStatus::Observed
    } else if arch && !housekeeping && arch_is_substantive(&cleaned_subject, has_scope) {
        // A `feat`/`refactor`/`perf` is an architectural decision — but only when
        // it actually says something. A bare "feat: updat" / "feat: update
        // changes" is dev churn, not a decision; precision over recall here.
        FactStatus::DeterministicallyDerived
    } else {
        // Plain `fix:`, housekeeping-without-rationale, vague feats, and
        // non-conventional prose-without-decision-phrasing are dropped.
        return None;
    };

    // The epitome: prefer the cleaned subject when it itself carries the
    // decision; otherwise pull the first body line that does.
    let subject_has_phrase = DECISION_PHRASES
        .iter()
        .any(|p| cleaned_subject.to_ascii_lowercase().contains(p));
    let epitome_src = if subject_has_phrase || arch || breaking_bang || is_revert {
        cleaned_subject.clone()
    } else {
        first_decision_line(body).unwrap_or_else(|| cleaned_subject.clone())
    };
    let epitome = truncate_clean(&epitome_src);
    if epitome.len() < 6 {
        return None;
    }

    // Polarity (ban vs positive choice) + oriented chosen/rejected come from the
    // deterministic negation-scope layer — judged on the DECISION sentence, so a
    // removal merely mentioned elsewhere can't escalate it to a ban, and "instead
    // of" stays a positive substitution.
    let pol = crate::polarity::analyze_polarity(&epitome_src);

    Some(GitDecision {
        epitome,
        fact_status,
        is_ban: pol.is_ban,
        options: pol.options,
    })
}

/// Parse a Conventional-Commits header `type(scope)!: subject`. Returns
/// `(type, breaking_bang, has_scope, subject_without_prefix)`. Non-conventional
/// subjects return `(None, false, false, <subject verbatim>)`.
fn parse_conventional(subject: &str) -> (Option<String>, bool, bool, String) {
    let Some(colon) = subject.find(':') else {
        return (None, false, false, subject.to_string());
    };
    let (head, rest) = subject.split_at(colon);
    let rest = rest[1..].trim().to_string(); // drop the ':' and surrounding space
    // head = "type" | "type(scope)" | "type!" | "type(scope)!"
    let breaking_bang = head.ends_with('!');
    let head_no_bang = head.trim_end_matches('!');
    let has_scope = head_no_bang.contains('(') && head_no_bang.contains(')');
    let type_part = head_no_bang
        .split('(')
        .next()
        .unwrap_or(head_no_bang)
        .trim();
    // A valid conventional type is a short, all-lowercase-ASCII word. Anything
    // else (e.g. "Fixes #12" or "WTF: nope") is treated as plain prose.
    let looks_typeish = !type_part.is_empty()
        && type_part.len() <= 12
        && type_part.chars().all(|c| c.is_ascii_lowercase());
    if looks_typeish && !rest.is_empty() {
        (Some(type_part.to_string()), breaking_bang, has_scope, rest)
    } else {
        (None, false, false, subject.to_string())
    }
}

/// Whether an architectural-type commit (`feat`/`refactor`/`perf`) without a
/// strong signal says enough to be a decision. A conventional **scope**
/// (`feat(cortex): …`) signals intent on its own; otherwise we require a few
/// meaningful words and reject vague-verb churn ("update", "fixing", "wip").
fn arch_is_substantive(cleaned_subject: &str, has_scope: bool) -> bool {
    const VAGUE_FIRST: &[&str] = &[
        "update", "updates", "updated", "updat", "updating", "fixing", "fix",
        "fixes", "fixed", "tweak", "tweaks", "tweaked", "wip", "misc", "cleanup",
        "various", "minor", "more", "progress", "stuff", "changes", "change",
        "improve", "improving", "improvements", "improvement", "tidy", "polish",
        "rework", "force", "bump", "bumping",
    ];
    let words: Vec<&str> = cleaned_subject.split_whitespace().collect();
    if words.len() < 2 {
        return false;
    }
    let first = words[0]
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_ascii_lowercase();
    if VAGUE_FIRST.contains(&first.as_str()) {
        return false;
    }
    has_scope || words.len() >= 3
}

/// The first body line that contains a decision phrase, trimmed of list markers.
fn first_decision_line(body: &str) -> Option<String> {
    for raw in body.lines() {
        let line = raw.trim().trim_start_matches(['-', '*', '•']).trim();
        if line.is_empty() {
            continue;
        }
        let lc = line.to_ascii_lowercase();
        if DECISION_PHRASES.iter().any(|p| lc.contains(p)) {
            return Some(line.to_string());
        }
    }
    None
}

/// Collapse whitespace and cap the epitome length.
fn truncate_clean(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(240).collect()
}

/// Build a [`Segmentation`] from mined commits. Each commit becomes its own
/// session so binding is commit-scoped; decisions get a deterministic node id
/// keyed on the SHA, and each touched file becomes a code episode one second
/// after the decision (so `t_use ≤ t_gen` and they bind).
#[must_use]
pub fn git_segmentation(commits: &[CommitInput]) -> Segmentation {
    let mut decisions: Vec<DecisionCandidate> = Vec::new();
    let mut episodes: Vec<EpisodeRecord> = Vec::new();

    for (i, c) in commits.iter().enumerate() {
        let Some(gd) = classify_commit(&c.subject, &c.body) else {
            continue;
        };
        let t_use = OffsetDateTime::from_unix_timestamp(c.epoch)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let t_gen = t_use + Duration::seconds(1);
        // Leave a wide seq gap per commit so episodes sort just after their
        // decision and never collide with the next commit's seqs.
        let base_seq = (i as u64) * 1_000;
        let short = c.sha.chars().take(12).collect::<String>();
        let session = format!("git:{short}");

        // L2 — split a bundled subject ("use X instead of Y, drop Z, and add W")
        // into one decision per part; a non-bundle returns a single part unchanged.
        // Node ids MUST follow the segmenter's scheme so the MemCortex ingest's
        // binding-endpoint reconciliation (`decision:{session}:{source_span.start}`,
        // `episode:{episode_id}`) resolves them to concrete nodes — otherwise the
        // decision→file edges dangle and the shaped-code link is lost.
        let parts = crate::polarity::split_coordinated(&gd.epitome);
        for (k, part) in parts.iter().enumerate() {
            let seq = base_seq + k as u64;
            // Re-derive polarity per part (so each split decision has its own
            // correct ban/options); fall back to the bundle's polarity for a
            // single-part (unsplit) decision.
            let pol = if parts.len() > 1 {
                crate::polarity::analyze_polarity(part)
            } else {
                crate::polarity::Polarity {
                    is_ban: gd.is_ban,
                    options: gd.options.clone(),
                }
            };
            decisions.push(DecisionCandidate {
                record: DecisionRecord {
                    epitome: part.clone(),
                    considered_options: pol.options,
                    is_ban: pol.is_ban,
                    superseded_by: None,
                    confirmation: None,
                    source_span: seq..seq + 1,
                    fact_status: gd.fact_status,
                    timestamp: t_use,
                    // The commit author — real per-engineer attribution for Teams.
                    decided_by: {
                        let a = c.author.trim();
                        (!a.is_empty()).then(|| a.to_string())
                    },
                },
                node_id: NodeId::new(format!("decision:{session}:{seq}")),
                turn_seq: seq,
                timestamp: t_use,
                session_id: session.clone(),
            });
        }

        for (j, f) in c.files.iter().take(MAX_FILES_PER_COMMIT).enumerate() {
            let episode_id = format!("git:ep:{}:{}", c.sha, j);
            episodes.push(EpisodeRecord {
                episode: CodeEpisode {
                    path: PathBuf::from(f),
                    diff: Diff::for_path(f),
                    git: Some(GitRef {
                        sha: c.sha.clone(),
                        branch: None,
                    }),
                    episode_id: episode_id.clone(),
                },
                node_id: NodeId::new(format!("episode:{episode_id}")),
                seq: base_seq + 1 + j as u64,
                timestamp: t_gen,
                session_id: session.clone(),
            });
        }
    }

    Segmentation {
        conversations: Vec::new(),
        decisions,
        episodes,
    }
}

/// Mine commits straight into prepared nodes (decisions + file episodes +
/// decision→episode bindings), reusing the standard binder and node-prep.
#[must_use]
pub fn mine_commit_nodes(commits: &[CommitInput]) -> Vec<PreparedNode> {
    let seg = git_segmentation(commits);
    let bindings = DefaultBinder.bind(&seg);
    DefaultNodePrep.prepare(&seg, bindings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ci(sha: &str, subject: &str, body: &str, files: &[&str], epoch: i64) -> CommitInput {
        CommitInput {
            sha: sha.to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
            files: files.iter().map(|s| s.to_string()).collect(),
            epoch,
            author: String::new(),
        }
    }

    #[test]
    fn feat_type_is_a_derived_decision() {
        let d = classify_commit("feat: add scored decision gate", "").unwrap();
        assert_eq!(d.fact_status, FactStatus::DeterministicallyDerived);
        assert_eq!(d.epitome, "add scored decision gate");
        assert!(!d.is_ban);
    }

    #[test]
    fn breaking_bang_is_observed_and_ban() {
        let d = classify_commit("feat!: drop the legacy v1 REST API", "").unwrap();
        assert_eq!(d.fact_status, FactStatus::Observed);
        assert!(d.is_ban, "‘drop’ should mark a ban");
    }

    #[test]
    fn breaking_change_footer_is_observed() {
        let d = classify_commit(
            "refactor: rework store init",
            "BREAKING CHANGE: open_and_register is now the only entry point",
        )
        .unwrap();
        assert_eq!(d.fact_status, FactStatus::Observed);
    }

    #[test]
    fn decision_phrasing_in_plain_commit_is_observed_with_options() {
        let d = classify_commit("Use Postgres instead of MySQL for the orders service", "")
            .unwrap();
        assert_eq!(d.fact_status, FactStatus::Observed);
        let chosen: Vec<_> = d.options.iter().filter(|o| o.chosen).collect();
        let rejected: Vec<_> = d.options.iter().filter(|o| !o.chosen).collect();
        assert_eq!(chosen.len(), 1);
        assert_eq!(rejected.len(), 1);
        assert_eq!(chosen[0].text, "Postgres"); // chosen-head extracted after "use"
        assert_eq!(rejected[0].text, "MySQL");
    }

    #[test]
    fn switch_from_to_parses_options() {
        let d = classify_commit("perf: switch from Myers to histogram diff", "").unwrap();
        // perf type + "switch ... to" — Observed via phrase, options from/to.
        assert_eq!(d.fact_status, FactStatus::Observed);
        let chosen: Vec<_> = d.options.iter().filter(|o| o.chosen).map(|o| &o.text).collect();
        let rejected: Vec<_> = d.options.iter().filter(|o| !o.chosen).map(|o| &o.text).collect();
        assert_eq!(rejected, vec!["Myers"]);
        assert_eq!(chosen, vec!["histogram diff"]);
    }

    #[test]
    fn revert_is_observed() {
        let d = classify_commit("Revert \"feat: add caching layer\"", "This reverts commit abc123.")
            .unwrap();
        assert_eq!(d.fact_status, FactStatus::Observed);
    }

    #[test]
    fn housekeeping_types_are_not_decisions() {
        for s in [
            "chore: bump deps",
            "docs: fix typo in readme",
            "style: cargo fmt",
            "test: add coverage for gate",
            "ci: cache the cargo registry",
            "build: enable lto",
        ] {
            assert!(classify_commit(s, "").is_none(), "{s} must be skipped");
        }
    }

    #[test]
    fn housekeeping_with_decision_phrasing_is_kept() {
        // A docs commit that records a real decision in its body still counts.
        let d = classify_commit(
            "docs: explain storage choice",
            "We decided to adopt RaBitQ for vector compression.",
        )
        .unwrap();
        assert_eq!(d.fact_status, FactStatus::Observed);
        assert_eq!(d.epitome, "We decided to adopt RaBitQ for vector compression.");
    }

    #[test]
    fn vague_feats_are_dropped_for_precision() {
        // Real low-information commit subjects that must NOT become decisions.
        for s in [
            "feat: updat",
            "feat: update changes",
            "feat: ingestion added",
            "feat: fixing graph issues",
            "refactor: cleanup",
            "perf: tweaks",
        ] {
            assert!(
                classify_commit(s, "").is_none(),
                "{s} is dev churn, not a decision"
            );
        }
    }

    #[test]
    fn substantive_and_scoped_feats_are_kept() {
        // A conventional scope signals intent on its own.
        assert!(classify_commit("feat(cortex): wire the sidecar", "").is_some());
        // A few meaningful words clear the bar.
        let d = classify_commit(
            "feat: added engines, code parsing, http parsing, cli and mcp",
            "",
        )
        .unwrap();
        assert_eq!(d.fact_status, FactStatus::DeterministicallyDerived);
    }

    #[test]
    fn plain_fix_and_trivial_are_skipped() {
        assert!(classify_commit("fix typo", "").is_none());
        assert!(classify_commit("fix: correct off-by-one in loop", "").is_none());
        assert!(classify_commit("Update README", "").is_none());
        assert!(classify_commit("Merge branch 'main' into dev", "").is_none());
        assert!(classify_commit("wip", "").is_none());
        assert!(classify_commit("bump version to 0.7.6", "").is_none());
    }

    #[test]
    fn segmentation_binds_decision_to_its_files_only() {
        let commits = vec![
            ci(
                "aaa111",
                "feat: add Postgres store instead of MySQL",
                "",
                &["db/config.rs", "db/schema.rs"],
                1_700_000_000,
            ),
            ci("bbb222", "chore: fmt", "", &["x.rs"], 1_700_000_100),
            ci(
                "ccc333",
                "refactor: split the segmenter",
                "",
                &["segmenter.rs"],
                1_700_000_200,
            ),
        ];
        let nodes = mine_commit_nodes(&commits);

        let decisions: Vec<_> = nodes
            .iter()
            .filter(|n| matches!(n, PreparedNode::Decision(_)))
            .collect();
        let bindings: Vec<_> = nodes
            .iter()
            .filter_map(|n| match n {
                PreparedNode::Binding(b) => Some(b),
                _ => None,
            })
            .collect();

        // Two decision commits survive (the chore is dropped).
        assert_eq!(decisions.len(), 2);
        // The feat commit bound to its 2 files; the refactor to its 1 → 3 edges.
        assert_eq!(bindings.len(), 3);
        // Node ids follow the segmenter scheme so MemCortex ingest can reconcile
        // the binding endpoints (decision:{session}:{seq} / episode:{episode_id}).
        for b in &bindings {
            assert!(
                b.from.as_str().starts_with("decision:git:"),
                "decision endpoint {} must use the segmenter scheme",
                b.from.as_str()
            );
            assert!(
                b.to.as_str().starts_with("episode:git:ep:"),
                "episode endpoint {} must use the segmenter scheme",
                b.to.as_str()
            );
            // The binding's provenance carries the commit session both sides share.
            assert!(b.prov.used_session.starts_with("git:"));
            assert_eq!(b.prov.used_session, b.prov.was_generated_by_session);
        }
    }

    #[test]
    fn degenerate_option_idioms_do_not_panic() {
        // Real commit subjects that triggered byte-range panics: an idiom keyword
        // with no operand. Must classify (or skip) without slicing out of bounds.
        for s in [
            "refactor: replace with new impl",
            "feat: move from to staging",
            "chore: instead of nothing",
            "perf: switch from to",
        ] {
            let _ = classify_commit(s, ""); // must not panic
        }
    }

    #[test]
    fn mining_is_deterministic() {
        let commits = vec![
            ci(
                "aaa111",
                "feat: adopt RaBitQ instead of int8 quantization",
                "",
                &["ann.rs", "quant.rs"],
                1_700_000_000,
            ),
            ci(
                "ccc333",
                "perf: migrate from Myers to histogram diff",
                "",
                &["mine.rs"],
                1_700_000_200,
            ),
        ];
        let a = serde_json::to_string(&mine_commit_nodes(&commits)).unwrap();
        let b = serde_json::to_string(&mine_commit_nodes(&commits)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn file_episodes_are_capped() {
        let files: Vec<String> = (0..100).map(|i| format!("f{i:03}.rs")).collect();
        let frefs: Vec<&str> = files.iter().map(String::as_str).collect();
        let commits = vec![ci(
            "aaa111",
            "refactor: sweeping rename across the engine",
            "",
            &frefs,
            1_700_000_000,
        )];
        let nodes = mine_commit_nodes(&commits);
        let episodes = nodes
            .iter()
            .filter(|n| matches!(n, PreparedNode::Episode(_)))
            .count();
        assert_eq!(episodes, MAX_FILES_PER_COMMIT);
    }
}
