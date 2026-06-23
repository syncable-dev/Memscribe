//! Cross-tool conformance suite (whitepaper §8.2).
//!
//! The thesis Memscribe sells is *adapter interchangeability*: a decision turn,
//! a rejection, a ban, or a failed edit should normalize to the **same
//! structural shape** no matter which of the nine tools produced the transcript.
//! These tests prove that against the canonical fixtures by driving every tool's
//! adapter through the real pipeline (`testkit::prepare_nodes`) and comparing the
//! resulting node shapes.
//!
//! All nine §8.2 scenarios are now asserted across all nine tools. Where a
//! scenario's fixtures were authored *identically* across tools
//! (`happy_path_decision_then_edits`), we assert the full shape is byte-identical
//! across all nine. Where fixture content legitimately differs per tool (the
//! `rejected_alternative` corpus uses different examples), we assert the weaker
//! cross-tool invariant the scenario actually guarantees. For `tool_failure`, all
//! nine tools uphold the same zero-episode invariant: a failed edit
//! (`ToolResult.ok = false`, linked to its `FileEdit` by `call_id`) produces no
//! spurious `CodeEpisode` and therefore no binding. See
//! `tool_failure_yields_no_spurious_episode`.
//!
//! The five extended scenarios (`interleaved_arcs`, `multi_edit_single_commit`,
//! `rewind_compaction`, `subagent_thread`, `no_commitment_marker`) are asserted
//! over every tool too. Two of them carry a genuine, *pinned* per-tool divergence
//! — only Gemini supersedes a pre-rewind decision (the rest route the notice to
//! `Unknown`), and no tool mints a distinct subagent session — so we assert the
//! weaker invariant that DOES hold and pin the divergence with an explicit
//! assertion + comment, never silently weakening to nothing.

use memscribe_core::node::{BindingEdge, DecisionRecord, PreparedNode};
use memscribe_core::SourceKind;
use memscribe_testkit::golden::fixtures_dir;
use memscribe_testkit::{parse_events, prepare_nodes};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every tool and the version slug its fixtures live under. Driven dynamically
/// against the fixtures on disk so a new tool/version is picked up automatically.
const TOOLS: &[(SourceKind, &str)] = &[
    (SourceKind::ClaudeCode, "2.0"),
    (SourceKind::Codex, "v2"),
    (SourceKind::Gemini, "v1"),
    (SourceKind::Otel, "genai"),
    (SourceKind::Cursor, "v1"),
    (SourceKind::Windsurf, "v1"),
    (SourceKind::Zed, "v1"),
    (SourceKind::VsCode, "v1"),
    (SourceKind::Copilot, "v1"),
];

/// Every tool *except* Gemini. Gemini is the one adapter whose transcript format
/// carries a machine-resolvable rewind target (`$rewindTo`), so it is the sole
/// tool that exercises the *full* supersede-and-skip invariant in
/// `rewind_compaction`. Every other tool routes its compaction/rewind notice to
/// `Unknown` (no resolvable replaced-range) and is asserted against the weaker —
/// but still genuinely held — "history preserved + pivot honored" invariant. See
/// the two `rewind_compaction_*` tests below.
const NON_GEMINI_TOOLS: &[(SourceKind, &str)] = &[
    (SourceKind::ClaudeCode, "2.0"),
    (SourceKind::Codex, "v2"),
    (SourceKind::Otel, "genai"),
    (SourceKind::Cursor, "v1"),
    (SourceKind::Windsurf, "v1"),
    (SourceKind::Zed, "v1"),
    (SourceKind::VsCode, "v1"),
    (SourceKind::Copilot, "v1"),
];

/// A structural fingerprint of a prepared-node stream: counts per variant plus
/// the decision-level flags the conformance contract pins. Two tools that
/// normalize the same scenario to the same `Shape` are interchangeable behind the
/// contract for that scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shape {
    conversations: usize,
    decisions: usize,
    episodes: usize,
    bindings: usize,
    is_ban: bool,
    chosen: BTreeSet<String>,
    rejected: BTreeSet<String>,
}

impl Shape {
    fn of(nodes: &[PreparedNode]) -> Self {
        let mut shape = Shape {
            conversations: 0,
            decisions: 0,
            episodes: 0,
            bindings: 0,
            is_ban: false,
            chosen: BTreeSet::new(),
            rejected: BTreeSet::new(),
        };
        for n in nodes {
            match n {
                PreparedNode::Conversation(_) => shape.conversations += 1,
                PreparedNode::Decision(d) => {
                    shape.decisions += 1;
                    shape.fold_decision(d);
                }
                PreparedNode::Episode(_) => shape.episodes += 1,
                PreparedNode::Binding(_) => shape.bindings += 1,
            }
        }
        shape
    }

    fn fold_decision(&mut self, d: &DecisionRecord) {
        if d.is_ban {
            self.is_ban = true;
        }
        for opt in &d.considered_options {
            if opt.chosen {
                self.chosen.insert(opt.text.clone());
            } else {
                self.rejected.insert(opt.text.clone());
            }
        }
    }
}

/// Resolve a fixture's input path and the stable *relative* path we feed the
/// pipeline (so provenance and any path-derived ids are machine-independent).
fn fixture_paths(tool: SourceKind, version: &str, case: &str) -> (PathBuf, PathBuf) {
    let file = format!("{case}.jsonl");
    let abs = fixtures_dir().join(tool.as_str()).join(version).join(&file);
    let rel = Path::new("fixtures")
        .join(tool.as_str())
        .join(version)
        .join(&file);
    (abs, rel)
}

/// Drive a tool's adapter + pipeline over a fixture and return the prepared
/// nodes, or `None` when the fixture is absent for that tool.
fn nodes_for(tool: SourceKind, version: &str, case: &str) -> Option<Vec<PreparedNode>> {
    let (abs, rel) = fixture_paths(tool, version, case);
    let bytes = std::fs::read(abs).ok()?;
    Some(prepare_nodes(tool, &bytes, &rel))
}

/// The shape a tool normalizes a scenario to, or `None` if the fixture is absent.
fn shape_for(tool: SourceKind, version: &str, case: &str) -> Option<Shape> {
    nodes_for(tool, version, case).map(|n| Shape::of(&n))
}

/// Drive a tool's adapter over a fixture and return the normalized event stream,
/// or `None` when the fixture is absent. Used to assert event-layer invariants
/// (verbatim retention) that hold *below* the gate.
fn events_for(
    tool: SourceKind,
    version: &str,
    case: &str,
) -> Option<Vec<memscribe_core::CaptureEvent>> {
    let (abs, rel) = fixture_paths(tool, version, case);
    let bytes = std::fs::read(abs).ok()?;
    Some(parse_events(tool, &bytes, &rel))
}

/// Collect every binding edge in a prepared-node stream, in stream order.
fn bindings_of(nodes: &[PreparedNode]) -> Vec<&BindingEdge> {
    nodes
        .iter()
        .filter_map(|n| match n {
            PreparedNode::Binding(b) => Some(b),
            _ => None,
        })
        .collect()
}

/// Collect every decision record in a prepared-node stream, in stream order.
fn decisions_of(nodes: &[PreparedNode]) -> Vec<&DecisionRecord> {
    nodes
        .iter()
        .filter_map(|n| match n {
            PreparedNode::Decision(d) => Some(d),
            _ => None,
        })
        .collect()
}

/// Whether the node stream marks at least one decision as superseded
/// (`superseded_by` is `Some`) — the structural signature of a rewind/compaction
/// the adapter surfaced to the segmenter as a typed `Rewind`/`Compaction` event.
fn has_superseded_decision(nodes: &[PreparedNode]) -> bool {
    decisions_of(nodes)
        .iter()
        .any(|d| d.superseded_by.is_some())
}

// ---------------------------------------------------------------------------
// happy_path_decision_then_edits — authored identically across all nine tools.
// This is the strongest interchangeability claim: byte-identical shape.
// ---------------------------------------------------------------------------

#[test]
fn happy_path_normalizes_to_identical_shape_across_every_tool() {
    let case = "happy_path_decision_then_edits";

    // The canonical shape: one gated conversation, one decision (Postgres chosen
    // over MySQL, not a ban), two file-edit episodes, two bindings.
    let mut chosen = BTreeSet::new();
    chosen.insert("Postgres".to_string());
    let mut rejected = BTreeSet::new();
    rejected.insert("MySQL".to_string());
    let canonical = Shape {
        conversations: 1,
        decisions: 1,
        episodes: 2,
        bindings: 2,
        is_ban: false,
        chosen,
        rejected,
    };

    let mut seen = 0;
    for &(tool, version) in TOOLS {
        let Some(shape) = shape_for(tool, version, case) else {
            panic!("missing {case} fixture for {tool}");
        };
        assert_eq!(
            shape, canonical,
            "{tool} normalized {case} to a different shape than the contract; \
             adapters must be interchangeable for this scenario"
        );
        seen += 1;
    }
    assert_eq!(
        seen,
        TOOLS.len(),
        "every tool must carry the {case} fixture"
    );
}

// ---------------------------------------------------------------------------
// ban — fixture content differs per tool, but the *ban flag* is the invariant
// the scenario guarantees, and it must hold for all nine. We also assert the
// gate elevated exactly one conversation + one decision per tool.
// ---------------------------------------------------------------------------

#[test]
fn ban_sets_is_ban_true_for_every_tool() {
    let case = "ban";
    let mut seen = 0;
    for &(tool, version) in TOOLS {
        let Some(shape) = shape_for(tool, version, case) else {
            panic!("missing {case} fixture for {tool}");
        };
        assert!(
            shape.is_ban,
            "{tool} failed to flag the {case} scenario as a ban (is_ban must be true)"
        );
        assert_eq!(
            shape.conversations, 1,
            "{tool} {case}: expected exactly one gated conversation"
        );
        assert_eq!(
            shape.decisions, 1,
            "{tool} {case}: expected exactly one decision carrying the ban"
        );
        seen += 1;
    }
    assert_eq!(
        seen,
        TOOLS.len(),
        "every tool must carry the {case} fixture"
    );
}

// ---------------------------------------------------------------------------
// rejected_alternative — the scenario invariant is that *when* the gate
// elevates a decision from a rejection-marked turn, the considered options carry
// both a chosen and a rejected alternative. The canonical "Stripe instead of
// PayPal" corpus is shared by the four tools that authored it identically; the
// rest use different examples (and codex's phrasing intentionally does not trip
// the gate). We assert the shared sub-corpus is identical, and that no tool
// fabricates an option set out of thin air.
// ---------------------------------------------------------------------------

#[test]
fn rejected_alternative_shared_corpus_is_identical() {
    let case = "rejected_alternative";
    // Tools whose fixtures use the canonical "Stripe instead of PayPal" text.
    let canonical_corpus = [
        SourceKind::ClaudeCode,
        SourceKind::Gemini,
        SourceKind::Otel,
        SourceKind::Copilot,
    ];

    let mut chosen = BTreeSet::new();
    chosen.insert("Stripe".to_string());
    let mut rejected = BTreeSet::new();
    rejected.insert("PayPal".to_string());

    let mut reference: Option<Shape> = None;
    for tool in canonical_corpus {
        let version = version_of(tool);
        let Some(shape) = shape_for(tool, version, case) else {
            panic!("missing {case} fixture for {tool}");
        };
        assert_eq!(
            shape.chosen, chosen,
            "{tool} {case}: chosen option set diverged from the shared corpus"
        );
        assert_eq!(
            shape.rejected, rejected,
            "{tool} {case}: rejected option set diverged from the shared corpus"
        );
        assert!(!shape.is_ban, "{tool} {case}: a rejection is not a ban");
        match &reference {
            None => reference = Some(shape),
            Some(r) => assert_eq!(
                &shape, r,
                "{tool} {case}: full shape diverged from the shared corpus"
            ),
        }
    }
}

/// Across *all* tools, a `rejected_alternative` fixture must never invent an
/// option set without an originating decision: any chosen/rejected option
/// implies at least one decision node. This is the contract that keeps the
/// option lists `Observed`, never guessed.
#[test]
fn rejected_alternative_options_imply_a_decision() {
    let case = "rejected_alternative";
    for &(tool, version) in TOOLS {
        let Some(nodes) = nodes_for(tool, version, case) else {
            panic!("missing {case} fixture for {tool}");
        };
        let shape = Shape::of(&nodes);
        if !shape.chosen.is_empty() || !shape.rejected.is_empty() {
            assert!(
                shape.decisions >= 1,
                "{tool} {case}: produced options with no decision node — options must \
                 derive from an observed decision, never be fabricated"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// tool_failure — the scenario invariant: a rejected edit (ToolResult.ok = false)
// must NOT produce a spurious Episode, and therefore no binding either. This now
// holds *uniformly* for all nine tools: every adapter links a failed edit's
// `FileEdit` to its failing `ToolResult` by `call_id`, so the segmenter drops it.
// ---------------------------------------------------------------------------

#[test]
fn tool_failure_yields_no_spurious_episode() {
    let case = "tool_failure";

    let mut seen = 0;
    for &(tool, version) in TOOLS {
        let Some(shape) = shape_for(tool, version, case) else {
            panic!("missing {case} fixture for {tool}");
        };
        assert_eq!(
            shape.episodes, 0,
            "{tool} {case}: a failed edit minted a spurious Episode — the adapter \
             must link the FileEdit to its failing ToolResult (ok=false) by call_id \
             so the segmenter drops it"
        );
        assert_eq!(
            shape.bindings, 0,
            "{tool} {case}: no episode but a binding survived — bindings must not \
             outlive their episode"
        );
        seen += 1;
    }
    assert_eq!(
        seen,
        TOOLS.len(),
        "every tool must carry the {case} fixture"
    );
}

/// Look up a tool's fixture version slug from the driving table.
fn version_of(tool: SourceKind) -> &'static str {
    TOOLS
        .iter()
        .find(|(t, _)| *t == tool)
        .map(|(_, v)| *v)
        .unwrap_or_else(|| panic!("no version registered for {tool}"))
}

// ===========================================================================
// The five additional §8.2 scenarios, now asserted across **all nine tools**
// (the three first-class CLIs plus OTel and the five IDE adapters — all of which
// now author these fixtures). Two scenarios (`interleaved_arcs`,
// `multi_edit_single_commit`, `no_commitment_marker`) hold their full structural
// invariant uniformly for every tool, so those tests iterate `TOOLS`.
//
// Two scenarios carry a real, *pinned* divergence and are split accordingly:
//
//   * `rewind_compaction`: only Gemini's `$rewindTo` resolves to a typed Rewind,
//     so only Gemini supersedes the pre-rewind decision (full invariant). The
//     other eight tools route their compaction/rewind *notice* to `Unknown`
//     (no machine-resolvable replaced-range) and therefore supersede NOTHING; we
//     assert the weaker-but-genuine invariant they DO uphold (verbatim history
//     preserved + the final edit binds to the latest post-pivot decision) and
//     PIN the absence of the supersede marker.
//
//   * `subagent_thread`: none of the nine adapters surface a *distinct* session
//     id for delegated work — every one co-attributes the subagent's edit to a
//     single normalized session. We assert the invariant that genuinely holds
//     (the subagent edit is captured and binds to its own in-session decision,
//     never dropped or cross-attributed) and PIN the single-session merge.
//
// Where a tool genuinely cannot express a scenario natively we assert the weaker
// invariant that DOES hold and pin the divergence in an explicit assertion +
// comment — never silently weakened to "anything goes".
// ===========================================================================

// ---------------------------------------------------------------------------
// interleaved_arcs — two decisions, edits to overlapping files. The invariant:
// *each edit binds to its own decision*, i.e. the most-recent decision that
// precedes it in time, so the two arcs do not collapse into one. Concretely:
//   - at least two distinct decisions each govern at least one edit;
//   - the very first edit binds to the FIRST decision (the earlier arc), and the
//     final edit binds to the SECOND decision (the later arc);
//   - no binding points at a decision that does not exist in the stream.
// ---------------------------------------------------------------------------

#[test]
fn interleaved_arcs_each_edit_binds_to_its_own_decision() {
    let case = "interleaved_arcs";
    let mut seen = 0;
    for &(tool, version) in TOOLS {
        let Some(nodes) = nodes_for(tool, version, case) else {
            panic!("missing {case} fixture for {tool}");
        };
        let decisions = decisions_of(&nodes);
        let bindings = bindings_of(&nodes);

        assert!(
            decisions.len() >= 2,
            "{tool} {case}: expected at least two decisions (two arcs), got {}",
            decisions.len()
        );
        assert!(
            bindings.len() >= 2,
            "{tool} {case}: expected at least two bindings across the two arcs, got {}",
            bindings.len()
        );

        // Every binding's source must be an observed decision, never fabricated:
        // a binding's PROV `used_decision` must equal its `from`.
        for b in &bindings {
            assert_eq!(
                b.prov.used_decision.as_ref(),
                Some(&b.from),
                "{tool} {case}: a binding's PROV used_decision must equal its `from` \
                 (the governing decision), never a fabricated source"
            );
        }

        // The set of distinct governing decisions must be > 1: the arcs are
        // genuinely interleaved, not all folded onto a single decision.
        let governing: BTreeSet<&str> = bindings.iter().map(|b| b.from.as_str()).collect();
        assert!(
            governing.len() >= 2,
            "{tool} {case}: every edit collapsed onto a single decision — the two \
             arcs were not bound independently (governing = {governing:?})"
        );

        // The first edit binds to the earliest arc and the last edit to the
        // latest arc: ordering is by `t_gen` (the edit time), then by `from`.
        let mut by_time = bindings.clone();
        by_time.sort_by(|a, b| {
            a.prov
                .t_gen
                .cmp(&b.prov.t_gen)
                .then_with(|| a.from.as_str().cmp(b.from.as_str()))
        });
        let first_from = by_time.first().unwrap().from.as_str();
        let last_from = by_time.last().unwrap().from.as_str();
        assert_ne!(
            first_from, last_from,
            "{tool} {case}: the earliest and latest edits must bind to *different* \
             decisions (the interleaving invariant)"
        );
        // And `t_use <= t_gen` must hold for every arc.
        for b in &bindings {
            assert!(
                b.prov.is_temporally_valid(),
                "{tool} {case}: a binding violated t_use <= t_gen"
            );
        }
        seen += 1;
    }
    assert_eq!(
        seen,
        TOOLS.len(),
        "every tool must carry the {case} fixture"
    );
}

// ---------------------------------------------------------------------------
// multi_edit_single_commit — one decision, N file edits. The invariant:
// 1 Decision / N Episodes / N Bindings, with every binding sourced from that one
// decision. This is the canonical "one commitment fans out to many files" shape.
// ---------------------------------------------------------------------------

#[test]
fn multi_edit_single_commit_one_decision_n_episodes_n_bindings() {
    let case = "multi_edit_single_commit";
    let mut seen = 0;
    for &(tool, version) in TOOLS {
        let Some(nodes) = nodes_for(tool, version, case) else {
            panic!("missing {case} fixture for {tool}");
        };
        let shape = Shape::of(&nodes);

        assert_eq!(
            shape.decisions, 1,
            "{tool} {case}: a single-commit fan-out must elevate exactly one decision"
        );
        assert!(
            shape.episodes >= 2,
            "{tool} {case}: expected several episodes (one per edited file), got {}",
            shape.episodes
        );
        assert_eq!(
            shape.bindings, shape.episodes,
            "{tool} {case}: N episodes must produce N bindings (1 decision → N edits)"
        );
        assert!(
            !shape.is_ban,
            "{tool} {case}: a fan-out commit is not a ban"
        );

        // All bindings share the one decision as their source.
        let bindings = bindings_of(&nodes);
        let governing: BTreeSet<&str> = bindings.iter().map(|b| b.from.as_str()).collect();
        assert_eq!(
            governing.len(),
            1,
            "{tool} {case}: every edit must bind to the *same* single decision, \
             found {} distinct sources",
            governing.len()
        );
        seen += 1;
    }
    assert_eq!(
        seen,
        TOOLS.len(),
        "every tool must carry the {case} fixture"
    );
}

// ---------------------------------------------------------------------------
// rewind_compaction — a decision is made, then rewound/compacted away, then a
// replacement decision is made and the edit lands.
//
// The full structural invariant (pre-rewind decision is *superseded* —
// `superseded_by = Some` — and does NOT bind; verbatim history preserved; the
// final edit binds to the post-rewind decision) requires the adapter to surface
// the rewind/compaction as a typed `Rewind { to_event }` / `Compaction
// { replaced }` event that the segmenter can resolve to a turn-seq region.
//
//   * gemini's `$rewindTo` carries a resolvable target event id → the segmenter
//     supersedes the rewound decision. We assert the FULL invariant here.
//
//   * the other EIGHT tools (claude_code's `summary` line, codex's `compacted`
//     `replaced_response_ids`, OTel's compaction span notice, and the five IDE
//     adapters' rewind/checkpoint notices) are context-compaction *notices* with
//     no machine-resolvable replaced-range, so every one routes the notice to
//     `Unknown` and NOTHING is superseded. This is a real, pinned divergence. The
//     invariant that still genuinely holds for all eight: the verbatim
//     conversation of every pivot turn is preserved, and the FINAL edit binds to
//     the LATEST (post-pivot) decision, never to the stale earlier one — so the
//     current view honors the pivot even without a structural supersede marker.
//     We assert exactly that and PIN the absence of the marker.
// ---------------------------------------------------------------------------

#[test]
fn rewind_compaction_gemini_supersedes_pre_rewind_decision() {
    // Gemini is the one CLI whose format carries a resolvable rewind target, so
    // it is the tool that exercises the full supersede-and-skip invariant.
    let nodes = nodes_for(SourceKind::Gemini, "v1", "rewind_compaction")
        .expect("gemini rewind_compaction fixture present");
    let decisions = decisions_of(&nodes);
    let bindings = bindings_of(&nodes);

    // Exactly one decision is superseded (the pre-rewind MongoDB choice), and at
    // least one survives (the post-rewind Postgres choice).
    let superseded: Vec<&&DecisionRecord> = decisions
        .iter()
        .filter(|d| d.superseded_by.is_some())
        .collect();
    assert_eq!(
        superseded.len(),
        1,
        "gemini rewind_compaction: exactly one (pre-rewind) decision must be superseded"
    );
    let marker = superseded[0].superseded_by.as_ref().unwrap().as_str();
    assert!(
        marker.starts_with("rewind:"),
        "gemini rewind_compaction: the supersede marker must be a rewind marker, got {marker:?}"
    );

    // The superseded decision must NOT govern any edit: the binder skips
    // superseded decisions and falls through to the survivor. We derive the
    // superseded turn-seq (the `decision:<session>:<seq>` suffix) and assert no
    // binding sources from it.
    let superseded_seqs: BTreeSet<u64> = decisions
        .iter()
        .filter(|d| d.superseded_by.is_some())
        .map(|d| d.source_span.start)
        .collect();
    assert!(
        !superseded_seqs.is_empty(),
        "gemini rewind_compaction: expected at least one superseded turn-seq"
    );
    for b in &bindings {
        let from_is_superseded = superseded_seqs
            .iter()
            .any(|seq| b.from.as_str().ends_with(&format!(":{seq}")));
        assert!(
            !from_is_superseded,
            "gemini rewind_compaction: binding {} sources a superseded decision",
            b.from
        );
    }

    // Verbatim history is preserved: a conversation span exists for BOTH the
    // rewound turn and the surviving turn (losslessness across the rewind).
    let convo_count = nodes
        .iter()
        .filter(|n| matches!(n, PreparedNode::Conversation(_)))
        .count();
    assert!(
        convo_count >= 2,
        "gemini rewind_compaction: verbatim spans for both the rewound and the \
         surviving turn must be preserved, found {convo_count}"
    );
}

#[test]
fn rewind_compaction_non_gemini_tools_preserve_history_and_honor_the_pivot() {
    // PINNED DIVERGENCE: none of the eight non-Gemini adapters resolve their
    // compaction/rewind notice to a typed Rewind/Compaction (claude_code's
    // `summary`, codex's `compacted`, OTel's compaction span, and the five IDE
    // adapters' rewind/checkpoint notices all route to `Unknown`), so NO decision
    // is superseded. We assert the invariants that genuinely hold and explicitly
    // pin the absence of the structural supersede marker.
    let mut seen = 0;
    for &(tool, version) in NON_GEMINI_TOOLS {
        let nodes = nodes_for(tool, version, "rewind_compaction")
            .unwrap_or_else(|| panic!("{tool} rewind_compaction fixture present"));

        // Pinned: the compaction notice did not mint a supersede marker.
        assert!(
            !has_superseded_decision(&nodes),
            "{tool} rewind_compaction: this adapter is expected to route its \
             context-compaction notice to Unknown (no resolvable replaced-range), \
             so NO decision should be superseded. If this fires, the adapter began \
             emitting a typed Rewind/Compaction and this pin must be revisited."
        );

        // Verbatim history preserved: every decision turn kept its conversation
        // span (at least the pre- and post-pivot decisions are both present).
        let convos = nodes
            .iter()
            .filter(|n| matches!(n, PreparedNode::Conversation(_)))
            .count();
        let decisions = decisions_of(&nodes);
        assert!(
            convos >= decisions.len() && decisions.len() >= 2,
            "{tool} rewind_compaction: both pivot turns must be retained verbatim \
             (convos={convos}, decisions={})",
            decisions.len()
        );

        // The current view honors the pivot: the FINAL edit binds to the LATEST
        // decision (largest source-turn seq), never to the stale earlier one.
        let bindings = bindings_of(&nodes);
        assert!(
            !bindings.is_empty(),
            "{tool} rewind_compaction: the post-pivot edit must still bind"
        );
        let latest_decision_seq = decisions
            .iter()
            .map(|d| d.source_span.start)
            .max()
            .expect("at least one decision");
        let final_binding = bindings
            .iter()
            .max_by_key(|b| b.prov.t_gen)
            .expect("at least one binding");
        assert!(
            final_binding
                .from
                .as_str()
                .ends_with(&format!(":{latest_decision_seq}")),
            "{tool} rewind_compaction: the final edit must bind to the latest \
             (post-pivot) decision :{latest_decision_seq}, got {}",
            final_binding.from
        );

        // And no binding may source a superseded decision (vacuously true here
        // since none are superseded, but it keeps the invariant explicit for the
        // day an adapter starts emitting typed Rewind events).
        for b in &bindings {
            let from = decisions.iter().find(|d| {
                b.from
                    .as_str()
                    .ends_with(&format!(":{}", d.source_span.start))
            });
            if let Some(d) = from {
                assert!(
                    d.superseded_by.is_none(),
                    "{tool} rewind_compaction: a binding sourced a superseded decision"
                );
            }
        }
        seen += 1;
    }
    assert_eq!(
        seen,
        NON_GEMINI_TOOLS.len(),
        "every non-Gemini tool must carry the rewind_compaction fixture"
    );
}

// ---------------------------------------------------------------------------
// subagent_thread — work is delegated to a subagent / nested thread.
//
// PINNED DIVERGENCE: the documented goal is "subagent nodes carry the *distinct*
// session id, not merged". In practice none of the nine adapters surface a
// separate `CaptureEvent.session_id` for the delegated work:
//   * claude_code MERGES the `isSidechain:true` session (`sess-subagent-008b`)
//     into the parent `sess-main-008`;
//   * codex carries the `thread_id` only inside an Unknown `turn_context` record,
//     not as a session;
//   * gemini drops the nested `threadId` entirely;
//   * OTel and the five IDE adapters (cursor/windsurf/zed/vscode/copilot) model
//     the delegated work as further turns on the SAME conversation/thread id,
//     never minting a child session.
// So at the node layer the subagent work is co-attributed to ONE session id
// across every tool.
//
// The invariant that genuinely holds (and that we assert): the subagent's edit
// is attributed to the same single session as its *own* governing decision and
// binds to THAT decision (the in-thread commitment), not to the dispatch turn or
// to a foreign session. The delegated work is captured, never dropped or
// cross-attributed. We pin the single-session-merge explicitly.
// ---------------------------------------------------------------------------

#[test]
fn subagent_thread_is_captured_and_bound_within_one_session() {
    let case = "subagent_thread";
    let mut seen = 0;
    for &(tool, version) in TOOLS {
        let Some(nodes) = nodes_for(tool, version, case) else {
            panic!("missing {case} fixture for {tool}");
        };

        // The subagent's edit is captured (not dropped) and binds.
        let episodes = nodes
            .iter()
            .filter(|n| matches!(n, PreparedNode::Episode(_)))
            .count();
        assert!(
            episodes >= 1,
            "{tool} {case}: the subagent's edit must be captured as an episode"
        );
        let bindings = bindings_of(&nodes);
        assert!(
            !bindings.is_empty(),
            "{tool} {case}: the subagent edit must bind to its governing decision"
        );

        // Each binding stays within ONE session (its own), binds to an observed
        // decision, and is temporally valid — whether the adapter merges the
        // subagent into the parent session or keeps it distinct.
        for b in &bindings {
            assert_eq!(
                b.prov.used_session, b.prov.was_generated_by_session,
                "{tool} {case}: a subagent binding must stay within a single session"
            );
            assert_eq!(
                b.prov.used_decision.as_ref(),
                Some(&b.from),
                "{tool} {case}: the subagent edit must bind to an observed decision"
            );
            assert!(
                b.prov.is_temporally_valid(),
                "{tool} {case}: subagent binding violated t_use <= t_gen"
            );
        }

        // Session separation, pinned per tool. Cursor reads each composer as its
        // own session, so the subagent thread is kept DISTINCT — "attributed, not
        // merged" (§8.2), the stronger, preferred behavior. The other tools thread
        // a single ParseCtx and currently merge the subagent into the parent.
        let events = events_for(tool, version, case).expect("events parse");
        let sessions: BTreeSet<&str> = events.iter().map(|e| e.session_id.as_str()).collect();
        let expected = if tool == SourceKind::Cursor { 2 } else { 1 };
        assert_eq!(
            sessions.len(),
            expected,
            "{tool} {case}: expected {expected} session id(s) (cursor keeps the \
             subagent distinct; others merge) — got {sessions:?}"
        );
        seen += 1;
    }
    assert_eq!(
        seen,
        TOOLS.len(),
        "every tool must carry the {case} fixture"
    );
}

// ---------------------------------------------------------------------------
// no_commitment_marker — a turn with no commitment marker, followed by an edit.
// The invariant: 0 Conversation nodes and 0 Decision nodes are elevated (the gate
// did not fire), but the edit episode is STILL present, and the verbatim user
// turn is still retained at the event layer (losslessness below the gate).
// ---------------------------------------------------------------------------

#[test]
fn no_commitment_marker_elevates_nothing_but_keeps_the_edit() {
    let case = "no_commitment_marker";
    let mut seen = 0;
    for &(tool, version) in TOOLS {
        let Some(nodes) = nodes_for(tool, version, case) else {
            panic!("missing {case} fixture for {tool}");
        };
        let shape = Shape::of(&nodes);

        assert_eq!(
            shape.conversations, 0,
            "{tool} {case}: an unmarked turn must not elevate a Conversation node"
        );
        assert_eq!(
            shape.decisions, 0,
            "{tool} {case}: an unmarked turn must not elevate a Decision node"
        );
        assert!(
            shape.episodes >= 1,
            "{tool} {case}: the edit episode must still be present despite no marker"
        );
        // With no decision to govern it, the lone edit produces no binding.
        assert_eq!(
            shape.bindings, 0,
            "{tool} {case}: an unbound edit must not fabricate a binding"
        );

        // Losslessness below the gate: the verbatim user turn is still retained as
        // a normalized event even though it produced no node.
        let events = events_for(tool, version, case).expect("events parse");
        let user_turns = events
            .iter()
            .filter(|e| matches!(e.kind, memscribe_core::EventKind::UserTurn { .. }))
            .count();
        assert!(
            user_turns >= 1,
            "{tool} {case}: the unmarked user turn must still be retained verbatim \
             at the event layer (lossless capture below the gate)"
        );
        seen += 1;
    }
    assert_eq!(
        seen,
        TOOLS.len(),
        "every tool must carry the {case} fixture"
    );
}

// ---------------------------------------------------------------------------
// Determinism guard: prepared-node output is a pure function of the input bytes.
// Re-running every fixture twice must yield byte-identical node streams. This is
// the property the golden snapshots silently depend on.
// ---------------------------------------------------------------------------

#[test]
fn prepared_nodes_are_deterministic_across_runs() {
    for &(tool, version) in TOOLS {
        for case in [
            "happy_path_decision_then_edits",
            "rejected_alternative",
            "ban",
            "tool_failure",
            // The five additional §8.2 scenarios, now authored for every tool.
            // The `continue` below is retained as a defensive skip for any tool
            // that has not yet authored a given fixture, so this guard never
            // becomes the thing that fails a partial corpus.
            "interleaved_arcs",
            "multi_edit_single_commit",
            "rewind_compaction",
            "subagent_thread",
            "no_commitment_marker",
        ] {
            let Some(first) = nodes_for(tool, version, case) else {
                continue;
            };
            let second = nodes_for(tool, version, case).expect("fixture read twice");
            assert_eq!(
                first, second,
                "{tool} {case}: prepared-node output is not deterministic across runs"
            );
        }
    }
}
