//! The segmenter: arc / turn-span bounds (whitepaper §3).
//!
//! Given the normalized event stream and the gate, the segmenter bounds
//! turn-spans, elevates gated turns to [`ConversationSpan`]s, seeds candidate
//! [`DecisionRecord`]s by parsing the turn text deterministically, and collects
//! the file edits as [`crate::node::CodeEpisode`]s. It performs no inference —
//! every field is a verbatim span or a deterministic function of one.

use crate::gate::CommitmentGate;
use crate::model::{content_id, CaptureEvent, EventKind};
use crate::node::{
    CodeEpisode, CommitmentMarker, ConversationSpan, DecisionRecord, FactStatus, MarkerCategory,
    NodeId, Opt,
};
use std::collections::{HashMap, HashSet};
use time::OffsetDateTime;

/// A candidate decision seeded from a gated turn, with the metadata the binder
/// needs to wire PROV edges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionCandidate {
    /// The parsed decision record.
    pub record: DecisionRecord,
    /// The deterministic node id.
    pub node_id: NodeId,
    /// The turn seq this decision was parsed from.
    pub turn_seq: u64,
    /// The decision's timestamp (used as `t_use`).
    pub timestamp: OffsetDateTime,
    /// The session the decision belongs to.
    pub session_id: String,
}

/// A code edit episode with the metadata the binder needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeRecord {
    /// The prepared episode.
    pub episode: CodeEpisode,
    /// The deterministic node id.
    pub node_id: NodeId,
    /// The seq of the originating edit event.
    pub seq: u64,
    /// The episode timestamp (used as `t_gen`).
    pub timestamp: OffsetDateTime,
    /// The session the episode belongs to.
    pub session_id: String,
}

/// The result of segmentation: conversations, decision candidates, episodes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Segmentation {
    /// Gated, verbatim dialogue spans.
    pub conversations: Vec<ConversationSpan>,
    /// Candidate decisions seeded from gated turns.
    pub decisions: Vec<DecisionCandidate>,
    /// Code edit episodes.
    pub episodes: Vec<EpisodeRecord>,
}

/// The segmenter stage.
pub trait Segmenter {
    /// Segment a per-session event stream (sorted by `seq`) into spans.
    fn segment(&self, events: &[CaptureEvent], gate: &CommitmentGate) -> Segmentation;
}

/// The default deterministic segmenter.
#[derive(Debug, Default)]
pub struct DefaultSegmenter;

impl Segmenter for DefaultSegmenter {
    fn segment(&self, events: &[CaptureEvent], gate: &CommitmentGate) -> Segmentation {
        let mut seg = Segmentation::default();

        // A FileEdit whose tool call failed (ToolResult.ok == false) must not
        // become an episode — "a tool failure → no spurious episode" (§8.2).
        let mut call_ok: HashMap<String, bool> = HashMap::new();
        for ev in events {
            if let EventKind::ToolResult { call_id, ok, .. } = &ev.kind {
                call_ok.insert(call_id.clone(), *ok);
            }
        }

        // Which sessions produced at least one *successful* edit. A Soft marker
        // (action request, demoted bare modal, soft rejection) seeds a candidate
        // Decision only when an edit in the same session confirms the intent —
        // this is the precision lever that lets high-recall action verbs in
        // without manufacturing phantom decisions from chatter (§ gate Tier).
        let mut sessions_with_edits: HashSet<String> = HashSet::new();
        for ev in events {
            if let EventKind::FileEdit { call_id, .. } = &ev.kind {
                if let Some(cid) = call_id {
                    if call_ok.get(cid) == Some(&false) {
                        continue; // a failed edit is no edit
                    }
                }
                sessions_with_edits.insert(ev.session_id.clone());
            }
        }

        // Rewind / Compaction supersede markers (§8.2): the verbatim history of
        // the affected turns is still emitted, but in the *current view* any
        // decision whose source turn falls in a rewound-away region or inside a
        // Compaction.replaced range no longer governs current edits. We resolve
        // those regions here, keyed by `(session_id, turn_seq)`, so each parsed
        // decision can be stamped with `superseded_by` deterministically.
        let supersedes = resolve_supersede_markers(events);

        for ev in events {
            match &ev.kind {
                EventKind::UserTurn { text, .. } => {
                    // Turn-source hygiene: gate only the human-prose PROJECTION of
                    // the turn, so pasted tool plumbing, injected system/skill
                    // text, log lines, and code dumps never elevate to a node. The
                    // verbatim turn is untouched at the event layer (lossless) —
                    // only what we elevate is cleaned, which is also what fixes the
                    // junk-epitome problem (the marker can no longer land on a
                    // plumbing line).
                    let Some(prose) = gate.human_prose(text) else {
                        continue; // no human prose worth gating
                    };
                    let markers = gate.evaluate(&prose);
                    if markers.is_empty() {
                        continue; // retained verbatim at the event layer; no node
                    }
                    let turn_range = ev.seq..ev.seq + 1;
                    seg.conversations.push(ConversationSpan {
                        session_id: ev.session_id.clone(),
                        turn_range: turn_range.clone(),
                        text: prose.clone(),
                        markers: markers.clone(),
                        fact_status: FactStatus::Observed,
                        provenance: vec![ev.provenance.clone()],
                    });

                    // Seed a candidate Decision only when the gate's tier policy
                    // admits it: Strong markers always; Soft markers only when an
                    // edit in the same session confirms the intent; Confirmation
                    // markers never. The verbatim Conversation above is kept
                    // regardless, so the dialogue signal is never lost — only the
                    // (heavier) Decision elevation is withheld for unconfirmed,
                    // ambiguous turns.
                    let session_has_edit = sessions_with_edits.contains(&ev.session_id);
                    if !gate.seeds_decision(&markers, session_has_edit) {
                        continue;
                    }
                    // Semantic intent gate at the SENTENCE level: seed a Decision
                    // only if some fired marker sits in a committal clause — not a
                    // bare question, a fragment, or third-person analysis prose (the
                    // residual junk class after turn-source hygiene) — and anchor
                    // the epitome on that clause. This also fixes multi-sentence
                    // turns where a marker fired inside a pasted analysis sentence:
                    // the epitome follows the human commitment, not the analysis.
                    // The Conversation above is kept regardless.
                    let Some(epi_offset) = best_committal_offset(&prose, &markers, gate) else {
                        continue;
                    };

                    let is_ban = gate.is_ban(&markers);
                    // A decision whose source turn was rewound away or compacted
                    // out is superseded in the current view.
                    let superseded_by = supersedes.get(&(ev.session_id.clone(), ev.seq)).cloned();
                    let record = DecisionRecord {
                        epitome: epitome_of(&prose, epi_offset),
                        considered_options: parse_options(&prose),
                        is_ban,
                        superseded_by,
                        confirmation: None,
                        source_span: turn_range,
                        // Observed for the verbatim text; uncertain element
                        // typing is flagged downstream, never guessed here.
                        fact_status: FactStatus::Observed,
                    };
                    seg.decisions.push(DecisionCandidate {
                        node_id: NodeId::new(format!("decision:{}:{}", ev.session_id, ev.seq)),
                        record,
                        turn_seq: ev.seq,
                        timestamp: ev.timestamp,
                        session_id: ev.session_id.clone(),
                    });
                }
                EventKind::FileEdit { call_id, diff } => {
                    // Drop edits from a failed tool call.
                    if let Some(cid) = call_id {
                        if call_ok.get(cid) == Some(&false) {
                            continue;
                        }
                    }
                    let episode_id = content_id(
                        format!("{}:{}:{}", ev.session_id, ev.seq, diff.path.display()).as_bytes(),
                    );
                    let git = ev.project.git.clone();
                    seg.episodes.push(EpisodeRecord {
                        episode: CodeEpisode {
                            path: diff.path.clone(),
                            diff: diff.clone(),
                            git,
                            episode_id: episode_id.clone(),
                        },
                        node_id: NodeId::new(format!("episode:{}", episode_id)),
                        seq: ev.seq,
                        timestamp: ev.timestamp,
                        session_id: ev.session_id.clone(),
                    });
                }
                _ => {}
            }
        }

        seg
    }
}

/// Resolve every `(session_id, turn_seq)` that is rewound away or compacted out
/// to a deterministic supersede-marker [`NodeId`] (§8.2).
///
/// Semantics:
/// - **Rewind**: a `Rewind { to_event }` at seq `R` logically truncates the
///   session back to the event whose `event_id == to_event`, at seq `T`. Every
///   turn strictly after the target and up to (and including) the rewind point —
///   the half-open interval `(T, R]` in seq terms — is rewound away. The target
///   turn itself survives (we rewound *to* it). The marker id is
///   `rewind:<session>:<R>`. If `to_event` cannot be resolved to a seq in the
///   same session, the rewind is a flagged no-op (no turns superseded) — never
///   a panic.
/// - **Compaction**: a `Compaction { replaced }` carries a `[start, end)` seq
///   range directly; every turn seq in that range is compacted out. The marker
///   id is `compaction:<session>:<start>-<end>`.
///
/// When a turn is covered by multiple markers, the *last* event in stream order
/// wins (the most recent truncation governs the current view), keeping the
/// result a deterministic function of event order.
fn resolve_supersede_markers(events: &[CaptureEvent]) -> HashMap<(String, u64), NodeId> {
    // event_id → seq, per session, so a Rewind target resolves to a turn seq.
    let mut seq_of: HashMap<(&str, &str), u64> = HashMap::new();
    for ev in events {
        seq_of
            .entry((ev.session_id.as_str(), ev.event_id.as_str()))
            .or_insert(ev.seq);
    }

    let mut out: HashMap<(String, u64), NodeId> = HashMap::new();
    for ev in events {
        match &ev.kind {
            EventKind::Rewind { to_event } => {
                let rewind_seq = ev.seq;
                let Some(&target_seq) = seq_of.get(&(ev.session_id.as_str(), to_event.as_str()))
                else {
                    // Unknown target → flagged no-op, panic-free.
                    continue;
                };
                if target_seq >= rewind_seq {
                    // Target at or after the rewind point: nothing to truncate.
                    continue;
                }
                let marker = NodeId::new(format!("rewind:{}:{}", ev.session_id, rewind_seq));
                for other in events {
                    if other.session_id == ev.session_id
                        && other.seq > target_seq
                        && other.seq <= rewind_seq
                    {
                        out.insert((other.session_id.clone(), other.seq), marker.clone());
                    }
                }
            }
            EventKind::Compaction { replaced } => {
                let marker = NodeId::new(format!(
                    "compaction:{}:{}-{}",
                    ev.session_id, replaced.start, replaced.end
                ));
                for other in events {
                    if other.session_id == ev.session_id && replaced.contains(&other.seq) {
                        out.insert((other.session_id.clone(), other.seq), marker.clone());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The byte offset of the marker that should anchor the decision epitome:
/// among markers whose containing **sentence** is committal (not a bare
/// question / fragment / third-person analysis clause), the one with the
/// strongest category. Returns `None` when no fired marker sits in a committal
/// sentence — the turn elevates a Conversation but seeds no Decision.
/// Deterministic: among equal-priority committal markers the first in rule
/// order wins.
fn best_committal_offset(
    prose: &str,
    markers: &[CommitmentMarker],
    gate: &CommitmentGate,
) -> Option<usize> {
    markers
        .iter()
        .filter(|m| gate.is_committal(&epitome_of(prose, m.offset)))
        .min_by_key(|m| epitome_priority(m.category))
        .map(|m| m.offset)
}

/// Lower = better anchor for the epitome.
fn epitome_priority(category: MarkerCategory) -> u8 {
    use MarkerCategory::*;
    match category {
        DecisionVerb | ActionRequest => 0,
        Rejection | Ban => 1,
        Memory => 2,
        Imperative => 3,
        Confirmation => 4,
    }
}

/// Whether byte `i` is a sentence break: a newline, or a `.`/`!`/`?` that
/// terminates a sentence (end of text, or followed by whitespace / a closing
/// quote or paren). A period inside an identifier or number — `module.exports`,
/// `v0.11`, `fts.rs` — is **not** a break, so the epitome is never truncated
/// mid-token.
fn is_sentence_break(bytes: &[u8], i: usize) -> bool {
    match bytes[i] {
        b'\n' => true,
        b'.' | b'!' | b'?' => match bytes.get(i + 1) {
            None => true,
            Some(c) => c.is_ascii_whitespace() || matches!(c, b'"' | b'\'' | b')'),
        },
        _ => false,
    }
}

/// Extract the decision sentence containing `offset` — a verbatim span, bounded
/// by sentence terminators. Deterministic.
fn epitome_of(text: &str, offset: usize) -> String {
    let bytes = text.as_bytes();
    let offset = offset.min(text.len());
    // Walk back to the start of the sentence.
    let mut start = 0usize;
    for i in (0..offset).rev() {
        if is_sentence_break(bytes, i) {
            start = i + 1;
            break;
        }
    }
    // Walk forward to the end of the sentence.
    let mut end = text.len();
    for i in offset..bytes.len() {
        if is_sentence_break(bytes, i) {
            end = i + 1;
            break;
        }
    }
    // Snap to char boundaries to stay panic-free on multibyte input.
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    text[start..end].trim().to_string()
}

/// Deterministically parse considered options from decision prose: the chosen
/// option from a use/go-with verb, and the rejected option from "instead of X".
fn parse_options(text: &str) -> Vec<Opt> {
    let mut opts = Vec::new();

    // Rejected: "instead of X" / "rather than X".
    for marker in ["instead of", "rather than", "as opposed to"] {
        if let Some(opt) = capture_after(text, marker) {
            opts.push(Opt {
                text: opt,
                chosen: false,
            });
        }
    }

    // Chosen: "use X" / "go with X" / "switch to X" / "adopt X".
    for marker in ["go with", "switch to", "migrate to", "adopt", "use"] {
        if let Some(opt) = capture_after(text, marker) {
            // Avoid double-listing the rejected phrase.
            if !opts.iter().any(|o| o.text.eq_ignore_ascii_case(&opt)) {
                opts.push(Opt {
                    text: opt,
                    chosen: true,
                });
                break;
            }
        }
    }

    opts
}

/// Capture the noun phrase immediately after a marker phrase, up to a clause or
/// sentence boundary. Case-insensitive match, verbatim capture. Deterministic.
fn capture_after(text: &str, marker: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let pos = lower.find(marker)?;
    let after = pos + marker.len();
    // Walk to a char boundary.
    let mut start = after;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let rest = &text[start..];
    let trimmed_start = rest.len() - rest.trim_start().len();
    let phrase: String = rest[trimmed_start..]
        .chars()
        .take_while(|c| !matches!(c, '.' | ',' | '!' | '?' | ';' | ':' | '\n'))
        .collect();
    let phrase = phrase.trim();
    // Capture just the option token (typically a single library/service name,
    // possibly hyphenated/dotted like `left-pad`). Keeping it to one token stays
    // deterministic and avoids swallowing the rest of the sentence.
    phrase.split_whitespace().next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_chosen_and_rejected() {
        let opts = parse_options("Let's use Stripe instead of PayPal for billing.");
        assert!(opts.iter().any(|o| o.text == "Stripe" && o.chosen));
        assert!(opts.iter().any(|o| o.text == "PayPal" && !o.chosen));
    }

    #[test]
    fn epitome_is_the_containing_sentence() {
        let t = "Some preamble. We must use prepared statements. Thanks.";
        let e = epitome_of(t, t.find("must").unwrap());
        assert_eq!(e, "We must use prepared statements.");
    }

    #[test]
    fn epitome_is_panic_free_on_multibyte() {
        let t = "café — we will use Postgres ☕";
        let _ = epitome_of(t, 3);
    }

    // ---- Task A: rewind / compaction supersede-marking ----

    use crate::binder::{Binder, DefaultBinder};
    use crate::model::{
        CaptureEvent, Diff, EventKind, ProjectRef, SourceKind, SourceLocation, SCHEMA_VERSION,
    };
    use time::OffsetDateTime;

    fn ts(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
    }

    fn ev(seq: u64, event_id: &str, kind: EventKind) -> CaptureEvent {
        CaptureEvent {
            schema_version: SCHEMA_VERSION,
            source: SourceKind::ClaudeCode,
            session_id: "s1".to_string(),
            seq,
            event_id: event_id.to_string(),
            parent_id: None,
            timestamp: ts(seq as i64),
            project: ProjectRef::from_cwd("/repo"),
            kind,
            provenance: SourceLocation::new("t.jsonl", 0, seq + 1),
        }
    }

    fn user(seq: u64, event_id: &str, text: &str) -> CaptureEvent {
        ev(
            seq,
            event_id,
            EventKind::UserTurn {
                text: text.to_string(),
                parts: vec![],
            },
        )
    }

    fn edit(seq: u64, path: &str) -> CaptureEvent {
        ev(
            seq,
            &format!("edit-{seq}"),
            EventKind::FileEdit {
                call_id: None,
                diff: Diff::for_path(path),
            },
        )
    }

    #[test]
    fn rewind_supersedes_decisions_in_rewound_region_and_binds_only_to_survivor() {
        // Decision A at turn 1; rewind back to turn 1 happens at turn 3 (so turn 2
        // is rewound away); decision B at turn 5; edit at turn 6.
        let events = vec![
            user(1, "m1", "Let's use Postgres for storage."),
            user(2, "m2", "Actually we must use MySQL instead."),
            ev(
                3,
                "r1",
                EventKind::Rewind {
                    to_event: "m1".into(),
                },
            ),
            user(5, "m5", "We will use Redis for the cache."),
            edit(6, "cache.rs"),
        ];
        let gate = CommitmentGate::default_table();
        let seg = DefaultSegmenter.segment(&events, &gate);

        // Turn 2's decision is in the rewound region (2 in (1, 3]) → superseded.
        let d2 = seg
            .decisions
            .iter()
            .find(|d| d.turn_seq == 2)
            .expect("turn-2 decision exists");
        assert_eq!(
            d2.record.superseded_by,
            Some(NodeId::new("rewind:s1:3")),
            "turn-2 decision must be superseded by the rewind marker"
        );

        // The rewound-to target (turn 1) survives.
        let d1 = seg.decisions.iter().find(|d| d.turn_seq == 1).unwrap();
        assert!(d1.record.superseded_by.is_none());

        // Decision B (turn 5) survives.
        let d5 = seg.decisions.iter().find(|d| d.turn_seq == 5).unwrap();
        assert!(d5.record.superseded_by.is_none());

        // Verbatim conversation span for the superseded turn 2 is still present.
        assert!(
            seg.conversations.iter().any(|c| c.turn_range.start == 2),
            "verbatim span for the rewound turn must be preserved"
        );

        // The edit binds to the most-recent NON-superseded decision (B), never A2.
        let edges = DefaultBinder.bind(&seg);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, NodeId::new("decision:s1:5"));
    }

    #[test]
    fn compaction_range_supersedes_contained_decisions() {
        // Compaction replaces seqs [1, 3): turns 1 and 2 are compacted out.
        let events = vec![
            user(1, "m1", "Let's use Postgres for storage."),
            user(2, "m2", "We must always use prepared statements."),
            ev(3, "c1", EventKind::Compaction { replaced: 1..3 }),
            user(4, "m4", "We will use Redis for the cache."),
        ];
        let gate = CommitmentGate::default_table();
        let seg = DefaultSegmenter.segment(&events, &gate);

        let marker = NodeId::new("compaction:s1:1-3");
        for seq in [1u64, 2] {
            let d = seg.decisions.iter().find(|d| d.turn_seq == seq).unwrap();
            assert_eq!(
                d.record.superseded_by,
                Some(marker.clone()),
                "turn {seq} should be compacted out"
            );
        }
        // Turn 4 is outside the range → survives.
        let d4 = seg.decisions.iter().find(|d| d.turn_seq == 4).unwrap();
        assert!(d4.record.superseded_by.is_none());

        // Verbatim spans for the compacted turns are still present (lossless).
        assert!(seg.conversations.iter().any(|c| c.turn_range.start == 1));
        assert!(seg.conversations.iter().any(|c| c.turn_range.start == 2));
    }

    #[test]
    fn rewind_to_unknown_target_is_a_flagged_no_op() {
        // The rewind target does not exist → no decision is superseded; panic-free.
        let events = vec![
            user(1, "m1", "Let's use Postgres for storage."),
            ev(
                2,
                "r1",
                EventKind::Rewind {
                    to_event: "does-not-exist".into(),
                },
            ),
        ];
        let gate = CommitmentGate::default_table();
        let seg = DefaultSegmenter.segment(&events, &gate);
        assert!(seg
            .decisions
            .iter()
            .all(|d| d.record.superseded_by.is_none()));
    }

    #[test]
    fn supersede_marking_is_deterministic_across_runs() {
        let events = vec![
            user(1, "m1", "Let's use Postgres."),
            user(2, "m2", "We must use MySQL instead."),
            ev(
                3,
                "r1",
                EventKind::Rewind {
                    to_event: "m1".into(),
                },
            ),
            ev(4, "c1", EventKind::Compaction { replaced: 2..3 }),
        ];
        let gate = CommitmentGate::default_table();
        let a = DefaultSegmenter.segment(&events, &gate);
        let b = DefaultSegmenter.segment(&events, &gate);
        assert_eq!(a, b);
    }

    #[test]
    fn epitome_does_not_split_mid_identifier() {
        // A `.` inside an identifier must not truncate the epitome.
        let t = "please use module.exports here for the config";
        let off = t.find("use").unwrap();
        let e = epitome_of(t, off);
        assert!(
            e.contains("module.exports"),
            "epitome truncated mid-identifier: {e}"
        );
    }

    #[test]
    fn best_committal_anchors_on_the_request_not_the_analysis() {
        let gate = CommitmentGate::default_table();
        // Sentence 1 is pasted analysis (marker "using"); sentence 2 is the real
        // request (marker "please add"). The epitome must follow the request.
        let prose =
            "this enumerates rows using a kind_label filter. please add a healthcheck endpoint";
        let markers = gate.evaluate(prose);
        let off =
            best_committal_offset(prose, &markers, &gate).expect("a committal sentence exists");
        let e = epitome_of(prose, off).to_lowercase();
        assert!(e.contains("healthcheck"), "epitome should follow the request: {e}");
        assert!(
            !e.contains("enumerates"),
            "epitome must not be the analysis clause: {e}"
        );
    }

    #[test]
    fn bare_question_seeds_no_decision_even_with_a_strong_marker() {
        let gate = CommitmentGate::default_table();
        // "migrate to" is a Strong DecisionVerb, but the whole turn is a question.
        let prose = "do we really need to migrate to postgres ?";
        let markers = gate.evaluate(prose);
        assert!(!markers.is_empty());
        assert!(
            best_committal_offset(prose, &markers, &gate).is_none(),
            "a bare question must not seed a decision"
        );
    }
}
