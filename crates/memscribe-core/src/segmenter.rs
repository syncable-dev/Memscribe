//! The segmenter: arc / turn-span bounds (whitepaper §3).
//!
//! Given the normalized event stream and the gate, the segmenter bounds
//! turn-spans, elevates gated turns to [`ConversationSpan`]s, seeds candidate
//! [`DecisionRecord`]s by parsing the turn text deterministically, and collects
//! the file edits as [`crate::node::CodeEpisode`]s. It performs no inference —
//! every field is a verbatim span or a deterministic function of one.

use crate::gate::{CommitmentGate, Tier};
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

                    // Seed a candidate Decision, SCORED not gated: every committal
                    // turn that anchors on a committal clause is scored (gate tier +
                    // marker category + resolved-choice + edit + hygiene penalties),
                    // then emitted at the FactStatus tier its score earns. Low-
                    // confidence turns survive at a LOWER tier instead of being
                    // dropped — recall recovery, since the old binary gate collapsed
                    // to ~7. Only sub-threshold noise is discarded. The verbatim
                    // Conversation above is kept regardless.
                    let session_has_edit = sessions_with_edits.contains(&ev.session_id);
                    let Some(epi_offset) = best_committal_offset(&prose, &markers, gate) else {
                        continue; // no committal clause to anchor the epitome
                    };
                    // Carry the antecedent when the committal sentence opens with an
                    // unbound pronoun, so the stored epitome is self-contained
                    // ("It must be idempotent." → "We rebuilt the cache. It …").
                    let epitome = epitome_with_antecedent(&prose, epi_offset);
                    // Authoritative reject for context-free pronoun fragments: if
                    // the committal sentence opened with an unbound pronoun AND the
                    // (antecedent-extended) epitome STILL carries no concrete
                    // referent, an agent can't learn from it — seed no Decision (the
                    // verbatim Conversation is still kept). Cross-turn antecedents
                    // are unreachable here by design → left to MemCortex's dreaming.
                    if opens_with_unbound_subject(&epitome_of(&prose, epi_offset))
                        && !antecedent_resolved(&epitome)
                    {
                        continue;
                    }
                    // Polarity (ban) from the scoped negation layer judged on the
                    // EPITOME — consistent with the git oracle and the read layer;
                    // a removal merely mentioned elsewhere, or an "X instead of Y"
                    // substitution, no longer mis-flags a ban. (Marker-based
                    // `gate.is_ban` was over/under-eager.)
                    let is_ban = crate::polarity::analyze_polarity(&epitome).is_ban;
                    let score =
                        score_decision_candidacy(&epitome, &markers, gate, is_ban, session_has_edit);
                    let Some(fact_status) = tier_for(score) else {
                        continue; // below the keep threshold — genuine noise
                    };
                    // A decision whose source turn was rewound away or compacted
                    // out is superseded in the current view.
                    let superseded_by = supersedes.get(&(ev.session_id.clone(), ev.seq)).cloned();
                    let record = DecisionRecord {
                        epitome,
                        considered_options: parse_options(&prose),
                        is_ban,
                        superseded_by,
                        confirmation: None,
                        source_span: turn_range,
                        // The scored tier: Observed (high-confidence) down through
                        // DeterministicallyDerived to StatisticallyRanked (recovered
                        // low-confidence). Retrieval down-weights the lower tiers.
                        fact_status,
                        // The originating turn's real wall-clock time, carried on
                        // the record so it survives ingest (else it defaults).
                        timestamp: ev.timestamp,
                        // Conversation capture doesn't know the author; the read
                        // layer falls back to the store owner. Git-mined decisions
                        // set this to the commit author (real per-engineer Teams).
                        decided_by: None,
                    };
                    seg.decisions.push(DecisionCandidate {
                        node_id: NodeId::new(format!("decision:{}:{}", ev.session_id, ev.seq)),
                        record,
                        turn_seq: ev.seq,
                        timestamp: ev.timestamp,
                        session_id: ev.session_id.clone(),
                    });
                }
                EventKind::AssistantTurn { text, model, .. } => {
                    // AI-authored decisions (Q2 "by whom"): the assistant's committal
                    // statements ("switch to X", "use Y") ARE real decisions — capture
                    // them but attribute them to the model via `decided_by`, so the UI
                    // reads "suggested by the assistant", not "you" (the prior bug:
                    // only UserTurn was captured + decided_by hardcoded None, so every
                    // decision fell back to the store owner). Same scored gate + tier;
                    // no Conversation node (that lane is the human's prose context).
                    let Some(prose) = gate.human_prose(text) else {
                        continue;
                    };
                    let markers = gate.evaluate(&prose);
                    if markers.is_empty() {
                        continue;
                    }
                    let session_has_edit = sessions_with_edits.contains(&ev.session_id);
                    let Some(epi_offset) = best_committal_offset(&prose, &markers, gate) else {
                        continue;
                    };
                    let epitome = epitome_with_antecedent(&prose, epi_offset);
                    if opens_with_unbound_subject(&epitome_of(&prose, epi_offset))
                        && !antecedent_resolved(&epitome)
                    {
                        continue;
                    }
                    let is_ban = crate::polarity::analyze_polarity(&epitome).is_ban;
                    let score =
                        score_decision_candidacy(&epitome, &markers, gate, is_ban, session_has_edit);
                    let Some(fact_status) = tier_for(score) else {
                        continue;
                    };
                    let superseded_by = supersedes.get(&(ev.session_id.clone(), ev.seq)).cloned();
                    let record = DecisionRecord {
                        epitome,
                        considered_options: parse_options(&prose),
                        is_ban,
                        superseded_by,
                        confirmation: None,
                        source_span: ev.seq..ev.seq + 1,
                        fact_status,
                        timestamp: ev.timestamp,
                        // The model that produced the turn — real per-author attribution.
                        decided_by: Some(model.clone().unwrap_or_else(|| "assistant".to_string())),
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
        // A ban is the headline of its turn — anchor the epitome on it so a
        // prohibition ("never add X. use Y instead.") is judged on the ban
        // clause, not on the trailing substitution sentence.
        Ban => 0,
        DecisionVerb | ActionRequest | Rejection => 1,
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

/// Max length of an antecedent-extended epitome before we fall back to the bare
/// committal sentence — a runaway turn must never produce a paragraph-long epitome.
const MAX_EPITOME_CHARS: usize = 320;
/// How many preceding sentences (same turn/paragraph) we absorb to resolve a
/// leading unbound pronoun.
const MAX_ANTECEDENT_SENTENCES: usize = 2;

/// The committal sentence, extended to carry its antecedent when it opens with an
/// unbound pronoun ("It must be idempotent." → "We rebuilt the cache. It must be
/// idempotent."). Walks back up to [`MAX_ANTECEDENT_SENTENCES`] within the SAME
/// turn, never across a blank-line/paragraph break, capped at
/// [`MAX_EPITOME_CHARS`]. A cross-turn antecedent is out of reach by design — the
/// segmenter's unit is one turn; resolving across turns is MemCortex's job.
fn epitome_with_antecedent(text: &str, offset: usize) -> String {
    let base = epitome_of(text, offset);
    if !opens_with_unbound_subject(&base) {
        return base;
    }
    let bytes = text.as_bytes();
    let offset = offset.min(text.len());
    // Start of the committal (base) sentence.
    let mut base_start = 0usize;
    for i in (0..offset).rev() {
        if is_sentence_break(bytes, i) {
            base_start = i + 1;
            break;
        }
    }
    // End of the committal sentence.
    let mut end = text.len();
    for i in offset..bytes.len() {
        if is_sentence_break(bytes, i) {
            end = i + 1;
            break;
        }
    }
    // Walk back over preceding sentences within the same paragraph.
    let mut span_start = base_start;
    for _ in 0..MAX_ANTECEDENT_SENTENCES {
        if span_start == 0 {
            break;
        }
        let mut prev_start = 0usize;
        for i in (0..span_start - 1).rev() {
            if is_sentence_break(bytes, i) {
                prev_start = i + 1;
                break;
            }
        }
        // Stop at a paragraph boundary (blank line) between the two sentences.
        let between = &text[prev_start..span_start];
        if between.contains("\n\n") || between.trim().is_empty() {
            break;
        }
        span_start = prev_start;
    }
    while span_start < text.len() && !text.is_char_boundary(span_start) {
        span_start += 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let extended = text[span_start..end].trim();
    if extended.chars().count() > MAX_EPITOME_CHARS {
        base
    } else {
        extended.to_string()
    }
}

/// The leading run of ASCII-alphabetic chars, lowercased ("that's" → "that",
/// "(it" → "it"). Used for subject/conjunction head detection.
fn lead_alpha(w: &str) -> String {
    w.chars()
        .skip_while(|c| !c.is_ascii_alphabetic())
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn is_leading_conjunction(w: &str) -> bool {
    matches!(
        lead_alpha(w).as_str(),
        "but" | "and" | "so" | "then" | "also" | "plus" | "yet" | "however" | "ok" | "okay" | "well"
    )
}

/// The sentence's grammatical subject is a bare, unbound pronoun / demonstrative
/// ("it has to…", "this should…", "they need…"), optionally after a leading
/// conjunction ("but it…"). Such a sentence can't stand alone — its referent
/// lived elsewhere. Elided-subject imperatives ("Use X") are NOT flagged.
fn opens_with_unbound_subject(s: &str) -> bool {
    let mut words = s.split_whitespace();
    let Some(mut first) = words.next() else {
        return false;
    };
    if is_leading_conjunction(first) {
        let Some(next) = words.next() else {
            return false;
        };
        first = next;
    }
    matches!(
        lead_alpha(first).as_str(),
        // include no-apostrophe contractions ("thats", "theres", "theyre")
        "it" | "its" | "this" | "that" | "thats" | "these" | "those" | "they" | "theyre"
            | "them" | "their" | "there" | "theres"
    )
}

/// A concrete referent is present in the span, so a leading pronoun is grounded:
/// a CamelCase/acronym identifier, a quoted term, a code token (`()`/backtick),
/// or a named alternative (`is_name_like`). First/second-person presence does NOT
/// count — "I"/"we" don't resolve what "it" refers to.
fn antecedent_resolved(s: &str) -> bool {
    if s.contains('"') || s.contains('`') || s.contains("()") {
        return true;
    }
    if s.split(|c: char| !c.is_ascii_alphanumeric()).any(is_camelcase) {
        return true;
    }
    // A named token — but SKIP the first word: sentence-initial capitalization
    // ("But …", "The …", "It …") is grammar, not a proper noun, and would
    // otherwise make every capitalized fragment look "resolved".
    s.split_whitespace()
        .skip(1)
        .any(|w| is_name_like(w.trim_matches(|c: char| !c.is_ascii_alphanumeric())))
}

/// Deterministically parse considered options from decision prose: the chosen
/// option from a use/go-with verb, and the rejected option from "instead of X".
fn parse_options(text: &str) -> Vec<Opt> {
    let mut opts = Vec::new();

    // Rejected: "instead of X" / "rather than X". Keep only NAMED alternatives —
    // a marker that grabbed a bare word ("instead of covering") is dropped.
    for marker in ["instead of", "rather than", "as opposed to"] {
        if let Some(opt) = capture_after(text, marker) {
            if is_name_like(&opt) {
                opts.push(Opt {
                    text: opt,
                    chosen: false,
                });
            }
        }
    }

    // Chosen: "use X" / "go with X" / "switch to X" / "adopt X".
    for marker in ["go with", "switch to", "migrate to", "adopt", "use"] {
        if let Some(opt) = capture_after(text, marker) {
            // Named alternatives only, and avoid double-listing the rejected one.
            if is_name_like(&opt) && !opts.iter().any(|o| o.text.eq_ignore_ascii_case(&opt)) {
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
    // possibly hyphenated/dotted like `left-pad`). Skip a leading article so
    // "use the X" yields `X`, not `the`. Keeping it to one token stays
    // deterministic and avoids swallowing the rest of the sentence.
    let mut toks = phrase.split_whitespace();
    let mut tok = toks.next()?;
    if matches!(tok.to_ascii_lowercase().as_str(), "the" | "a" | "an") {
        tok = toks.next()?;
    }
    Some(tok.to_string())
}

/// A captured option is kept only if it looks like a NAMED alternative — a
/// proper noun (`Postgres`, `MySQL`) or a technical identifier (`left-pad`,
/// `v2`, `std::fs`). Bare lowercase words (`the`, `it`, `covering`, `floating`)
/// are sentence fragments the markers happened to grab, never real choices —
/// dropping them keeps "considered options" meaningful instead of noise.
fn is_name_like(token: &str) -> bool {
    let t = token.trim();
    let n = t.chars().count();
    if !(2..=40).contains(&n) {
        return false;
    }
    // Must START with a letter — rules out captured code/log fragments like
    // "(repo_id", "_rebuild`)", "[USER".
    if !t.chars().next().map_or(false, |c| c.is_ascii_alphabetic()) {
        return false;
    }
    // ONLY clean identifier chars — any bracket/paren/quote/backtick/slash/comma
    // betrays a code or log fragment, never a real alternative name.
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return false;
    }
    // "Named": Title/CamelCase (`Postgres`, `MySQL`) or an internal identifier
    // marker (`left-pad`, `web3`, `std.fs`) — never a bare lowercase word
    // ("covering", "floating", "the").
    let starts_upper = t.chars().next().map_or(false, |c| c.is_ascii_uppercase());
    let has_marker = t.chars().any(|c| c.is_ascii_digit() || matches!(c, '-' | '.'));
    starts_upper || has_marker
}

/// Precision gate (audit 2026-06-26): the commitment-marker gate is recall-biased
/// — it elevates ANY committal-sounding prose, so ~88% of captured "decisions"
/// were instructions / questions / status remarks / fragments / pasted code, not
/// decisions. A stored decision must read as a RESOLVED CHOICE. This is the
/// precision counterweight: hard-reject the non-decision classes, then require an
/// explicit choice signal. Deterministic, zero-LLM. Trades recall (a terse
/// mechanism-only decision with no choice verb may be dropped) for precision.
/// Hard noise — never a decision regardless of any choice/ban signal:
/// questions, skill/goal text, pasted code/log, ALL-CAPS rants, and multi-quote
/// concatenations of several captured turns. (Test oracle; the live scored gate
/// applies these as graded penalties — see [`score_decision_candidacy`].)
#[cfg(test)]
fn is_hard_noise(s: &str, lower: &str) -> bool {
    is_question(s, lower)
        || is_skill_or_goal(lower)
        || looks_like_code_or_log(s)
        || is_shouting(s)
        || s.matches('"').count() >= 4
}

/// Keep a candidate as a decision iff it clears the hard-noise filters AND is
/// either a resolved choice or an explicit ban. Retained as a TEST oracle for
/// the hygiene/resolved-choice predicates the scored gate reuses; the live path
/// now scores instead (see [`score_decision_candidacy`]).
#[cfg(test)]
fn keep_decision(epitome: &str, is_ban: bool) -> bool {
    let decoded = decode_entities(epitome);
    let s = decoded.trim();
    if s.chars().count() < 3 {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if is_hard_noise(s, &lower) {
        return false;
    }
    is_ban || has_resolved_choice(s, &lower)
}

/// The precision keep-test with no ban context — thin wrapper used by the tests.
#[cfg(test)]
fn is_decisive_epitome(epitome: &str) -> bool {
    keep_decision(epitome, false)
}

/// Decode the few HTML entities that ride in on captured prose, so code markers
/// like `-&gt;` are detectable and quotes count correctly.
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// An ALL-CAPS rant / log shout is not a decision.
fn is_shouting(s: &str) -> bool {
    let letters: Vec<char> = s.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    if letters.len() < 12 {
        return false;
    }
    let upper = letters.iter().filter(|c| c.is_ascii_uppercase()).count();
    (upper as f64) / (letters.len() as f64) > 0.6
}

/// A question is a request for information, never a resolved choice.
fn is_question(s: &str, lower: &str) -> bool {
    if s.ends_with('?') {
        return true;
    }
    const LEADS: [&str; 16] = [
        "how ", "what ", "whats ", "what's ", "why ", "when ", "where ", "which ", "can you",
        "could you", "should we", "should i", "do we ", "does ", "is it", "are we",
    ];
    if LEADS.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // Embedded question buried mid-text.
    const EMBED: [&str; 6] = ["could you", "can you", "what other", "how do we", "do i need", "is it ready"];
    EMBED.iter().any(|p| lower.contains(p))
}

/// `/goal` text, skill, and system boilerplate are not decisions.
fn is_skill_or_goal(lower: &str) -> bool {
    lower.contains("/goal")
        || lower.contains("your goal is to")
        || lower.contains("system reminder")
        || lower.contains("memtrace-first")
        || lower.starts_with("i'll consider using")
        || lower.starts_with("active /goal")
}

/// Pasted code / shell / log lines that slipped past prose hygiene.
fn looks_like_code_or_log(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    // Strong code/log signals. A bare file mention ("…into src/worker/email.rs")
    // is NOT one of them — a decision that names the file it touches is normal
    // prose; the punctuation-density check below catches actual pasted code/logs.
    if lower.starts_with("echo ")
        || s.contains("####")
        || s.contains("::")
        || s.contains("->")
    {
        return true;
    }
    // Dominated by code punctuation rather than prose.
    let total = s.chars().count().max(1);
    let codey = s
        .chars()
        .filter(|c| matches!(c, '{' | '}' | '[' | ']' | '(' | ')' | '<' | '>' | '=' | '|' | '\\' | '`' | '#' | ';'))
        .count();
    (codey as f64) / (total as f64) > 0.12
}

/// Does the text show a RESOLVED CHOICE — a commitment / adoption / contrastive
/// pick of a NAMED thing — rather than a plan, request, or observation?
fn has_resolved_choice(s: &str, lower: &str) -> bool {
    // Commitment to a settled choice (NOT weak future-intent like "we will …",
    // which reads as a plan, not a resolved decision).
    for m in ["decided to", "chose ", "choosing ", "going with "] {
        if lower.contains(m) {
            return true;
        }
    }
    // Strong adoption / default verbs.
    for m in ["switch to ", "adopt ", "migrate to ", "default to ", " by default"] {
        if lower.contains(m) {
            return true;
        }
    }
    // Adoption/retention of a NAMED thing ("use X" / "keep X" / "go with X").
    for m in ["use ", "keep ", "go with "] {
        if let Some(opt) = capture_after(s, m) {
            if is_name_like(&opt) {
                return true;
            }
        }
    }
    // Contrastive choice with a NAMED alternative ("X instead of Y").
    for m in ["instead of", "rather than"] {
        if let Some(opt) = capture_after(s, m) {
            if is_name_like(&opt) {
                return true;
            }
        }
    }
    // A named technical mechanism + a behavioural verb = a design decision about
    // HOW something works (recovers terse specs the choice-verb rules miss, e.g.
    // "decide() returns DeepenAmbiguous on a small score-gap").
    has_named_mechanism(s, lower)
}

/// A named mechanism (`CamelCase` identifier or a `call()`) paired with a
/// behavioural verb — a decision about behaviour, not a request.
fn has_named_mechanism(s: &str, lower: &str) -> bool {
    const VERBS: [&str; 11] = [
        "returns ", " maps ", " routes ", "defaults to", " caps ", " bounds ", "dedup",
        " emits ", " yields ", " resolves ", " falls back",
    ];
    if !VERBS.iter().any(|v| lower.contains(v)) {
        return false;
    }
    s.contains("()") || s.split(|c: char| !c.is_ascii_alphanumeric()).any(is_camelcase)
}

/// `DeepenAmbiguous`, `RRF`, `ID` — a token with ≥2 uppercase letters that reads
/// as a named type / acronym, not an English word.
fn is_camelcase(w: &str) -> bool {
    w.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && w.chars().filter(|c| c.is_ascii_uppercase()).count() >= 2
}

/// The candidacy score in [0,1] for a marker-firing committal turn — a pure,
/// deterministic blend of gate tier, marker category, resolved-choice /
/// named-mechanism signals, edit confirmation, and hygiene penalties. Replaces
/// the binary keep/reject gate: lower scores still EMIT (at a lower FactStatus
/// tier — recall recovery) instead of being dropped. (Scored gate 2026-06-26.)
fn score_decision_candidacy(
    epitome: &str,
    markers: &[CommitmentMarker],
    gate: &CommitmentGate,
    is_ban: bool,
    session_has_edit: bool,
) -> f32 {
    let decoded = decode_entities(epitome);
    let s = decoded.trim();
    let lower = s.to_ascii_lowercase();
    // Structurally never a decision — no tier rescues these: skill/goal text,
    // pasted code/log, ALL-CAPS rants, multi-turn quote-blobs, and (L0) clear
    // non-language garbage (hashes/base64/JSON/log blobs that slipped past
    // line-level hygiene looking like prose).
    if is_skill_or_goal(&lower)
        || looks_like_code_or_log(s)
        || is_shouting(s)
        || s.matches('"').count() >= 4
        || crate::languageness::is_garbage(s)
    {
        return 0.0;
    }
    // L1a. Strong agent/process narration ("I'll take this as…", "I'm going to…",
    // "the developer notes…") is never a decision — even a planning monologue that
    // names a tool ("…then use Memtrace…") is narration, so no resolved-choice
    // rescue here.
    if crate::speechact::is_process_narration(s) {
        return 0.0;
    }
    // L1a'. Soft narration ("let me …", "I'll …") is dropped only when it carries
    // no resolved named choice — so a genuine "let me switch to Postgres" survives.
    if crate::speechact::is_agent_narration(s) && !has_resolved_choice(s, &lower) {
        return 0.0;
    }
    // L1b. A sentence whose subject is an unbound pronoun with NO in-span referent
    // is a context-free fragment ("it has to be fluently") — unlearnable. When the
    // antecedent IS present (absorbed by `epitome_with_antecedent`, or natively),
    // it survives, at a lower tier via the pronoun penalty in step 7.
    if opens_with_unbound_subject(s) && !antecedent_resolved(s) {
        return 0.0;
    }
    // 1. Rule-tier base.
    let mut score: f32 = match gate.strongest_tier(markers) {
        Some(Tier::Strong) => 0.60,
        Some(Tier::Soft) => 0.35,
        None => 0.30,
    };
    // 2. Marker-category primacy (best across fired categories).
    let cat_bonus = markers
        .iter()
        .map(|m| match m.category {
            MarkerCategory::DecisionVerb | MarkerCategory::Rejection => 0.20,
            MarkerCategory::Ban => 0.18,
            MarkerCategory::ActionRequest => 0.12,
            MarkerCategory::Imperative | MarkerCategory::Memory => 0.10,
            MarkerCategory::Confirmation => 0.0,
        })
        .fold(0.0_f32, f32::max);
    score += cat_bonus;
    // 3. Resolved-choice / named-mechanism.
    if has_resolved_choice(s, &lower) {
        score += 0.15;
    }
    if has_named_mechanism(s, &lower) {
        score += 0.10;
    }
    // 3b. (L2) Speech-act confirmation: a fired marker that is ALSO a decisional
    //     commitment (declarative/imperative with a commissive verb, not a
    //     question or an instruction to the agent) is a stronger decision. Boost
    //     only — never drops a candidate (the structural hard-zeros do that).
    if crate::speechact::is_decisional_act(s) {
        score += 0.12;
    }
    // 4. Edit confirmation — a signal, not a gate.
    score += if session_has_edit { 0.25 } else { 0.05 };
    // 5. A ban is a decision.
    if is_ban {
        score += 0.10;
    }
    // 6. Question is decision-ADJACENT at best ("should we use X?") → soft
    //    penalty, not a hard drop (the structural noise above is already gone).
    if is_question(s, &lower) {
        score -= 0.30;
    }
    // 7. A resolved-but-pronoun-led epitome (antecedent absorbed, so it survived
    //    L1b) is a notch less crisp than a natively self-contained decision —
    //    demote so it lands a tier lower.
    if opens_with_unbound_subject(s) {
        score -= 0.15;
    }
    score.clamp(0.0, 1.0)
}

/// Map a candidacy score to a FactStatus tier, or `None` to drop. The top tier
/// stays high-precision (≥0.85 ⇒ Observed); the recovered tier (0.45–0.65 ⇒
/// StatisticallyRanked) is where the previously-dropped real decisions land.
fn tier_for(score: f32) -> Option<FactStatus> {
    if score >= 0.85 {
        Some(FactStatus::Observed)
    } else if score >= 0.65 {
        Some(FactStatus::DeterministicallyDerived)
    } else if score >= 0.45 {
        Some(FactStatus::StatisticallyRanked)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_narration_never_seeds_a_decision() {
        let gate = CommitmentGate::default_table();
        for s in [
            "I'll take this as the plan to recall memories",
            "Let me use it:",
            "the developer notes that they aren't listed",
            "It was a user asking so I need to answer them",
        ] {
            let m = gate.evaluate(s);
            let sc = score_decision_candidacy(s, &m, &gate, false, true);
            assert_eq!(tier_for(sc), None, "narration must drop: {s:?} (score {sc})");
        }
    }

    #[test]
    fn first_person_named_choice_survives_narration_guard() {
        let gate = CommitmentGate::default_table();
        let s = "let me switch to Postgres for the store";
        let m = gate.evaluate(s);
        let sc = score_decision_candidacy(s, &m, &gate, false, true);
        assert!(tier_for(sc).is_some(), "a real named choice must survive (score {sc})");
    }

    #[test]
    fn unbound_pronoun_fragment_drops_but_resolved_one_survives() {
        let gate = CommitmentGate::default_table();
        let frag = "it has to be fluently and not something I discover";
        let m = gate.evaluate(frag);
        let sc = score_decision_candidacy(frag, &m, &gate, false, true);
        assert_eq!(tier_for(sc), None, "context-free pronoun fragment must drop (score {sc})");

        let resolved = "We rebuilt the Cache. It must stay idempotent.";
        let m2 = gate.evaluate(resolved);
        if !m2.is_empty() {
            let sc2 = score_decision_candidacy(resolved, &m2, &gate, false, true);
            assert!(tier_for(sc2).is_some(), "pronoun with a resolved antecedent survives (score {sc2})");
        }

        // The antecedent-EXTENDED form must still drop: a leading capitalized
        // conjunction ("But …") is NOT a resolved referent.
        let extended = "But thats not working man? it shouldn't be like that ... it has to be fluently and not something I discover";
        let m3 = gate.evaluate(extended);
        let sc3 = score_decision_candidacy(extended, &m3, &gate, false, true);
        assert_eq!(tier_for(sc3), None, "capitalized-conjunction fragment must still drop (score {sc3})");
    }

    #[test]
    fn scored_gate_emits_tiers_not_binary() {
        let gate = CommitmentGate::default_table();
        // A strong resolved choice with an edit → top tier (Observed).
        let strong = "Let's use Postgres instead of MySQL for the orders service.";
        let m = gate.evaluate(strong);
        let sc = score_decision_candidacy(strong, &m, &gate, false, true);
        assert_eq!(tier_for(sc), Some(FactStatus::Observed), "strong choice → Observed (score {sc})");

        // A bare imperative with NO edit — the old binary gate dropped this; the
        // scored gate must keep it at a LOWER tier (the recall recovery).
        let soft = "we should always validate auth tokens on the server";
        let m2 = gate.evaluate(soft);
        if !m2.is_empty() {
            let sc2 = score_decision_candidacy(soft, &m2, &gate, false, false);
            assert!(tier_for(sc2).is_some(), "soft imperative should survive at a tier (score {sc2})");
            assert_ne!(tier_for(sc2), Some(FactStatus::Observed), "but not at the top tier");
        }

        // Skill/goal boilerplate is genuine noise — dropped at any tier.
        let noise = "/goal your goal is to build the initial version";
        let m3 = gate.evaluate(noise);
        assert_eq!(tier_for(score_decision_candidacy(noise, &m3, &gate, false, false)), None);
    }

    #[test]
    fn options_parse_chosen_and_rejected() {
        let opts = parse_options("Let's use Stripe instead of PayPal for billing.");
        assert!(opts.iter().any(|o| o.text == "Stripe" && o.chosen));
        assert!(opts.iter().any(|o| o.text == "PayPal" && !o.chosen));
    }

    #[test]
    fn options_reject_bare_word_fragments() {
        // A non-decision fragment: the markers grab bare words ("covering",
        // "the wrapper") that are NOT named alternatives — emit nothing.
        let opts =
            parse_options("downward keeps it off the title instead of covering it. use the wrapper.");
        assert!(opts.is_empty(), "bare sentence words must not become options");
        // A genuine named choice still parses (proper nouns are name-like).
        let real = parse_options("switch to Redis instead of Memcached");
        assert!(real.iter().any(|o| o.text == "Redis" && o.chosen));
        assert!(real.iter().any(|o| o.text == "Memcached" && !o.chosen));
    }

    #[test]
    fn is_name_like_accepts_names_rejects_fragments() {
        for ok in ["Postgres", "MySQL", "left-pad", "web3", "std.fs", "Redis", "RaBitQ"] {
            assert!(is_name_like(ok), "{ok} should read as a named alternative");
        }
        // bare words, and captured code/log fragments
        for bad in [
            "the", "a", "it", "covering", "floating", "(repo_id", "R_INITIATED]", "memdb/",
            "_rebuild`)", "[USER",
        ] {
            assert!(!is_name_like(bad), "{bad} must be rejected");
        }
    }

    #[test]
    fn precision_gate_keeps_real_decisions() {
        // Resolved choices (drawn from the real audit's genuine bucket).
        for keep in [
            "keep RaBitQ; fix the recovery",
            "Use graphMode: strict by default",
            "switch to Redis instead of Memcached",
            "use the property index instead of full-DB scans",
            "then chose Build the robust auto-trigger",
            "Let's use Postgres instead of MySQL for the orders service.",
            "we decided to adopt RaBitQ two-tier compression",
            "decide() returns DeepenAmbiguous on a small RRF top-2 score-gap",
        ] {
            assert!(is_decisive_epitome(keep), "should KEEP a resolved choice: {keep:?}");
        }
    }

    #[test]
    fn precision_gate_rejects_non_decisions() {
        // Instructions, questions, status, fragments, skill/goal, pasted code —
        // all real titles the recall-biased gate wrongly captured as decisions.
        for drop in [
            "We need to fix this bug ...",
            "downward keeps it off the title instead of covering it.",
            "How do we speed up this query data 1000 times man",
            "Build and tests pass.",
            "Spin up as many needed agents to fix codex, cursor, VS Code",
            "/goal your goal is to build the initial version of MemCortex",
            "echo \"############ RUN 2\"",
            "and ensure nothing is lost, then we will flip it next.",
            "Could you investigate that",
            "I need to make a discord announcement",
        ] {
            assert!(!is_decisive_epitome(drop), "should REJECT a non-decision: {drop:?}");
        }
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
            user(2, "m2", "Use SQLx instead of Diesel for queries."),
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
