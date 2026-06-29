//! The output contract: prepared nodes (whitepaper §6).
//!
//! Memscribe only ever produces nodes with `Observed` or
//! `DeterministicallyDerived` fact-status. It does the deterministic
//! preparation and *flags* everything that would require inference
//! (fine-grained decision typing, concept naming) for the consumer to handle
//! later. That is what keeps the module zero-LLM and its output golden-testable.

use crate::model::{Diff, GitRef, SourceLocation};
use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::path::PathBuf;
use time::OffsetDateTime;

/// A stable id for a prepared node. Derived deterministically from the source
/// (session id + span), so the same input always yields the same id.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub String);

impl NodeId {
    /// Construct a node id.
    pub fn new(s: impl Into<String>) -> Self {
        NodeId(s.into())
    }
    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The epistemic status of a node or edge. Memscribe emits only the first two;
/// the latter two are *flags* for a downstream inference layer, never values
/// Memscribe itself computes by guessing.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FactStatus {
    /// Verbatim from the source.
    Observed,
    /// Computed by a deterministic function of observed data.
    DeterministicallyDerived,
    /// Ranked by a statistical measure (downstream).
    StatisticallyRanked,
    /// An LLM hypothesis (downstream); Memscribe only ever *flags* this.
    LlmHypothesis,
}

/// The category of a deterministic commitment marker.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarkerCategory {
    /// Explicit decision verb ("use", "let's go with", "decide").
    DecisionVerb,
    /// A rejected alternative ("instead of X", "rather than").
    Rejection,
    /// A ban ("we will NOT / never use X") — Kruchten anticrisis.
    Ban,
    /// An imperative ("must", "always", "never", "shall").
    Imperative,
    /// A memory directive ("remember that", "keep in mind").
    Memory,
    /// Assistant-proposal-then-user-confirmation.
    Confirmation,
    /// An imperative request to change code ("fix", "add", "refactor",
    /// "remove", "optimize"). Distinct from [`Self::Imperative`] (modal
    /// obligation: must/always/never) — an action request should bind to an
    /// edit, not state a standing rule. Additive variant (serde snake_case →
    /// `action_request`); existing serialization is unchanged.
    ActionRequest,
}

/// Which deterministic commitment marker fired on a turn, and where.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitmentMarker {
    /// The rule id that matched (e.g. `decision_verb.use`).
    pub rule_id: String,
    /// The marker category.
    pub category: MarkerCategory,
    /// The verbatim text span that matched.
    pub matched_text: String,
    /// Byte offset of the match within the turn text.
    pub offset: usize,
}

/// A gated, verbatim dialogue span (always `Observed`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSpan {
    /// The session the span belongs to.
    pub session_id: String,
    /// The (inclusive-start, exclusive-end) turn-seq range.
    pub turn_range: Range<u64>,
    /// The verbatim dialogue text.
    pub text: String,
    /// Which deterministic markers fired.
    pub markers: Vec<CommitmentMarker>,
    /// Always [`FactStatus::Observed`].
    pub fact_status: FactStatus,
    /// Provenance pointers for replay & audit.
    pub provenance: Vec<SourceLocation>,
}

/// A considered option within a decision.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Opt {
    /// The option text (verbatim span).
    pub text: String,
    /// Whether this option was the one chosen.
    pub chosen: bool,
}

/// A pointer to a confirmation check (an ArchUnit rule, test, or schema check)
/// named in a decision.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckRef {
    /// The kind of check (`archunit` | `test` | `schema`).
    pub kind: String,
    /// The named target.
    pub target: String,
}

/// A decision parsed deterministically from a gated turn. The schema follows
/// IBIS / QOC / MADR / Kruchten. Prose typing that requires inference is left to
/// the consumer; only verbatim spans and structural flags are populated here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecisionRecord {
    /// The decision sentence (a verbatim span).
    pub epitome: String,
    /// Options parsed from "instead of X", "vs", or explicit lists.
    pub considered_options: Vec<Opt>,
    /// True for a ban ("we will NOT / never use X").
    pub is_ban: bool,
    /// A pointer to a node that supersedes this decision, if known.
    pub superseded_by: Option<NodeId>,
    /// A named confirmation check, if the decision references one.
    pub confirmation: Option<CheckRef>,
    /// The exact turn span (no accreted context).
    pub source_span: Range<u64>,
    /// `Observed` for the verbatim text. Element-typing uncertainty is flagged
    /// downstream as [`FactStatus::LlmHypothesis`], never guessed here.
    pub fact_status: FactStatus,
    /// When the decision was made: the originating gated turn's wall-clock time.
    /// Lives on the record (not a sidecar) so each decision carries its own real
    /// time across `nodeprep`'s `.record.clone()` and the NDJSON round-trip —
    /// without it, ingest stamps every node with the batch default (epoch 1000).
    #[serde(with = "time::serde::rfc3339", default = "epoch_fallback")]
    pub timestamp: OffsetDateTime,
    /// Who made the decision, when known — the authoritative per-engineer
    /// attribution (Teams). Git-mined decisions set this to the commit author
    /// ("Name <email>"); conversation-captured decisions leave it `None` (the read
    /// layer falls back to the store owner). Additive + `serde(default)`, so older
    /// NDJSON corpora and the conversation path deserialize/serialize unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
}

/// Backward-compat default for `DecisionRecord.timestamp` when reading NDJSON
/// produced before the field existed (e.g. a committed benchmark corpus): the
/// record deserializes with an epoch timestamp instead of failing the whole line.
fn epoch_fallback() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

/// A code edit episode: the path, the diff, and the git sha
/// (`DeterministicallyDerived`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeEpisode {
    /// The edited path.
    pub path: PathBuf,
    /// The normalized diff.
    pub diff: Diff,
    /// The git ref at edit time, if known.
    pub git: Option<GitRef>,
    /// A deterministic id for the episode.
    pub episode_id: String,
}

/// The relation a binding edge expresses.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// A decision/conversation produced an episode.
    Produced,
    /// A decision governs an episode.
    Governs,
    /// An episode is derived from a decision/conversation.
    DerivedFrom,
    /// Two nodes are statistically correlated.
    CorrelatedWith,
}

/// A PROV record: `used(session, decision)` + `wasGeneratedBy(diff, session)`
/// with the temporal invariant `t_use ≤ t_gen`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvRecord {
    /// The session that used the decision.
    pub used_session: String,
    /// The decision node that was used, if any.
    pub used_decision: Option<NodeId>,
    /// The session that generated the edit.
    pub was_generated_by_session: String,
    /// When the decision was used.
    #[serde(with = "time::serde::rfc3339")]
    pub t_use: OffsetDateTime,
    /// When the edit was generated. Invariant: `t_use ≤ t_gen`.
    #[serde(with = "time::serde::rfc3339")]
    pub t_gen: OffsetDateTime,
}

impl ProvRecord {
    /// Whether the temporal invariant `t_use ≤ t_gen` holds.
    #[must_use]
    pub fn is_temporally_valid(&self) -> bool {
        self.t_use <= self.t_gen
    }
}

/// A correlation measure between two nodes, when computable.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CorrelationTuple {
    /// Support.
    pub support: f64,
    /// Confidence.
    pub confidence: f64,
    /// Lift.
    pub lift: f64,
    /// Phi coefficient.
    pub phi: f64,
    /// p-value.
    pub p: f64,
}

/// A binding edge: decision/conversation → episode, with PROV, fact-status, and
/// (optional) correlation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BindingEdge {
    /// The source node.
    pub from: NodeId,
    /// The target node.
    pub to: NodeId,
    /// The relation.
    pub relation: Relation,
    /// The PROV record.
    pub prov: ProvRecord,
    /// `DeterministicallyDerived` when recorded live; else downgraded.
    pub fact_status: FactStatus,
    /// A correlation tuple, when computable.
    pub correlation: Option<CorrelationTuple>,
}

/// The typed data the consumer layer (MemCortex) ingests.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum PreparedNode {
    /// A gated, verbatim dialogue span.
    Conversation(ConversationSpan),
    /// A deterministically-parsed decision.
    Decision(DecisionRecord),
    /// A code edit episode.
    Episode(CodeEpisode),
    /// A decision/conversation → episode binding.
    Binding(BindingEdge),
}

impl PreparedNode {
    /// A stable tag for the node variant — used in tests and ordering.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            PreparedNode::Conversation(_) => "conversation",
            PreparedNode::Decision(_) => "decision",
            PreparedNode::Episode(_) => "episode",
            PreparedNode::Binding(_) => "binding",
        }
    }

    /// The node's fact-status.
    #[must_use]
    pub fn fact_status(&self) -> FactStatus {
        match self {
            PreparedNode::Conversation(c) => c.fact_status,
            PreparedNode::Decision(d) => d.fact_status,
            PreparedNode::Episode(_) => FactStatus::DeterministicallyDerived,
            PreparedNode::Binding(b) => b.fact_status,
        }
    }
}
