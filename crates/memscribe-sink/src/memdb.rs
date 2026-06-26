//! The MemDB sink (feature `memdb`, **off by default**).
//!
//! This is the single seam Memtrace turns on to ingest [`PreparedNode`]s into
//! MemDB with their **bi-temporal** headers and a **typed kind + property
//! layout**. It is deliberately a *reference*: it does **not** depend on the
//! `memdb` crate. Memtrace supplies the real gRPC client
//! (`memcore_client::MemcoreClient`) and converts the [`MemDbRecord`]s this sink
//! produces into `create_record_with_properties` calls. The field-by-field
//! wiring — including a complete, copy-paste `Sink` impl for Memtrace — lives in
//! `MEMDB.md`.
//!
//! ## The two time axes (MemDB `memcore_core::RecordHeader`)
//!
//! * **`valid_at`** — *valid time*: when the fact was true in the world. For
//!   Memscribe that is the **turn / episode time** — the moment the agent edit
//!   happened or the dialogue turn occurred — **not** when we ingested it. It
//!   comes from the node and is never guessed.
//! * **`transaction_at`** — *transaction time*: when MemDB learned the fact,
//!   i.e. our **ingest time**. One sink instance stamps a single
//!   `transaction_at` for the whole batch so a replayed transcript lands at one
//!   coherent transaction instant. MemDB has no explicit `transaction_at` field
//!   on `RecordHeader`; on the wire it is the wall-clock at which the
//!   `create_record` RPC is issued. The sink keeps it for audit and as the
//!   `valid_at` fallback when a node carries no intrinsic valid time.
//! * **`episode_id`** — the **arc / episode** the node belongs to. MemDB keys
//!   its episodic, co-change and provenance machinery off this id
//!   (`RecordHeader::episode_id`).
//!
//! ## The kind + property layout
//!
//! Every MemDB record carries a `RecordKind` (`Node` / `Edge` / `Episode`, see
//! [`RecordKindTag`]) and a set of typed `Property` rows that feed MemDB's
//! property index — the same index that backs Memtrace's `find_symbol` /
//! `find_code`. This sink derives both deterministically from the prepared node
//! so the Memtrace-side wiring is a mechanical translation:
//!
//! | `PreparedNode` | `RecordKind` | `valid_at` | `episode_id` | Key properties |
//! |----------------|--------------|-----------|--------------|----------------|
//! | `Conversation` | `Node` (1)   | *(none)*  | *(none)*     | `node`, `session_id`, `turn_start`, `turn_end`, `fact_status`, `marker_count` |
//! | `Decision`     | `Node` (1)   | *(none)*  | *(none)*     | `node`, `epitome`, `is_ban`, `fact_status`, `option_count`, `source_span_*` |
//! | `Episode`      | `Episode` (4)| *(none)*  | `Some(id)`   | `node`, `episode_id`, `path`, `fact_status` |
//! | `Binding`      | `Edge` (2)   | `t_gen`   | *(none)*     | `node`, `from`, `to`, `relation`, `fact_status` |
//!
//! Memscribe is fully usable **without** MemDB: NDJSON is the default sink and
//! this module only compiles when the `memdb` feature is enabled.

use memscribe_core::{FactStatus, PreparedNode, Sink, SinkError};
use time::OffsetDateTime;

/// MemDB's `RecordKind`, mirrored here so the reference sink stays free of any
/// dependency on the `memdb` crate. The discriminants match
/// `memcore_core::RecordKind` exactly (`memcore-core/src/lib.rs:201`):
/// `Node = 1`, `Edge = 2`, `Episode = 4`. Memtrace maps each tag straight onto
/// the real enum when it constructs the `RecordHeader`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKindTag {
    /// A graph vertex — `RecordKind::Node` (1). Conversations and decisions.
    Node,
    /// A graph edge — `RecordKind::Edge` (2). Bindings (`from → to`).
    Edge,
    /// An arc/episode — `RecordKind::Episode` (4). Code edit episodes.
    Episode,
}

impl RecordKindTag {
    /// The `u8` discriminant, identical to `memcore_core::RecordKind as u8`.
    #[must_use]
    pub fn discriminant(self) -> u8 {
        match self {
            RecordKindTag::Node => 1,
            RecordKindTag::Edge => 2,
            RecordKindTag::Episode => 4,
        }
    }
}

/// A single typed property row, mirroring `memcore_core::properties::Property`.
///
/// Memtrace replays these onto `memcore_client::PropertyBuilder`
/// (`.string` / `.int` / `.bool`) when it builds the create request. Keeping the
/// shape here — rather than the prost wire type — is what lets the reference
/// sink avoid the `memdb` dependency while still pinning the exact property
/// layout MemDB will index.
#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    /// The property key (e.g. `"file_path"`, `"start_line"`).
    pub key: String,
    /// The typed value.
    pub value: PropValue,
}

impl Prop {
    fn string(key: &str, value: impl Into<String>) -> Self {
        Prop {
            key: key.to_string(),
            value: PropValue::String(value.into()),
        }
    }
    fn int(key: &str, value: i64) -> Self {
        Prop {
            key: key.to_string(),
            value: PropValue::Int(value),
        }
    }
    fn bool(key: &str, value: bool) -> Self {
        Prop {
            key: key.to_string(),
            value: PropValue::Bool(value),
        }
    }
}

/// A typed property value, mirroring `memcore_core::properties::PropertyValue`.
#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    /// Maps to `PropertyBuilder::string` / `PropertyValue::String`.
    String(String),
    /// Maps to `PropertyBuilder::int` / `PropertyValue::Int`.
    Int(i64),
    /// Maps to `PropertyBuilder::bool` / `PropertyValue::Bool`.
    Bool(bool),
}

/// Bi-temporal coordinates written alongside each node in MemDB.
///
/// These map directly onto MemDB's `memcore_core::RecordHeader`: `valid_at` →
/// `RecordHeader::valid_at` (as `Micros`), `transaction_at` is the wall-clock at
/// which the `create_record` RPC is issued (no explicit `RecordHeader` field),
/// and `episode_id` → `RecordHeader::episode_id` (resolved to `EpisodeId(u32)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BiTemporal {
    /// The turn/episode time (**valid time**), when derivable from the node.
    /// `None` when the node carries no intrinsic valid-time anchor (the consumer
    /// then falls back to `transaction_at`).
    pub valid_at: Option<OffsetDateTime>,
    /// The ingest time (**transaction time**).
    pub transaction_at: OffsetDateTime,
    /// The arc/episode id, when the node carries one.
    pub episode_id: Option<String>,
}

/// A node prepared for MemDB ingestion: the bi-temporal header, the target
/// `RecordKind`, the canonical body JSON, and the typed property rows.
#[derive(Debug, Clone)]
pub struct MemDbRecord {
    /// The bi-temporal header.
    pub header: BiTemporal,
    /// The MemDB `RecordKind` this node maps to.
    pub kind: RecordKindTag,
    /// The node's canonical JSON — becomes the record `body`.
    pub node_json: String,
    /// The typed property rows — become `Vec<Property>` via `PropertyBuilder`.
    pub properties: Vec<Prop>,
}

impl MemDbRecord {
    /// Look up a property value by key (test/inspection helper).
    #[must_use]
    pub fn prop(&self, key: &str) -> Option<&PropValue> {
        self.properties
            .iter()
            .find(|p| p.key == key)
            .map(|p| &p.value)
    }
}

/// A sink that prepares nodes for MemDB ingestion.
pub struct MemDbSink {
    records: Vec<MemDbRecord>,
    transaction_at: OffsetDateTime,
}

impl MemDbSink {
    /// A sink stamping `transaction_at` as the ingest time. (Pass a fixed value
    /// for deterministic tests; pass `OffsetDateTime::now_utc()` in production.)
    #[must_use]
    pub fn new(transaction_at: OffsetDateTime) -> Self {
        MemDbSink {
            records: Vec::new(),
            transaction_at,
        }
    }

    /// The records prepared so far.
    #[must_use]
    pub fn records(&self) -> &[MemDbRecord] {
        &self.records
    }

    /// Derive the bi-temporal header for a node.
    ///
    /// The valid-time / episode anchors are intrinsic to the node and never
    /// guessed:
    ///
    /// * [`Episode`](PreparedNode::Episode) carries an `episode_id` (the arc) but
    ///   no in-band timestamp on the prepared struct, so its valid time is left
    ///   to the consumer; `episode_id` is set.
    /// * [`Binding`](PreparedNode::Binding) is a PROV edge: its valid time is the
    ///   moment the bound edit was generated, `prov.t_gen` (the `wasGeneratedBy`
    ///   instant). It is not itself an episode, so `episode_id` is `None`.
    /// * [`Decision`](PreparedNode::Decision) and
    ///   [`Conversation`](PreparedNode::Conversation) have no intrinsic
    ///   `OffsetDateTime` on the prepared node (only turn-seq spans), so valid
    ///   time falls back to `transaction_at` downstream; both leave `episode_id`
    ///   unset here.
    fn header_for(&self, node: &PreparedNode) -> BiTemporal {
        let (valid_at, episode_id) = match node {
            PreparedNode::Episode(e) => (None, Some(e.episode_id.clone())),
            PreparedNode::Binding(b) => (Some(b.prov.t_gen), None),
            PreparedNode::Decision(_) | PreparedNode::Conversation(_) => (None, None),
        };
        BiTemporal {
            valid_at,
            transaction_at: self.transaction_at,
            episode_id,
        }
    }

    /// The MemDB `RecordKind` this node maps to.
    ///
    /// * Conversation / Decision → `Node` (graph vertices).
    /// * Episode → `Episode` (an arc; registered via `record_episode`).
    /// * Binding → `Edge` (`from → to`, typed by `relation`).
    fn kind_for(node: &PreparedNode) -> RecordKindTag {
        match node {
            PreparedNode::Conversation(_) | PreparedNode::Decision(_) => RecordKindTag::Node,
            PreparedNode::Episode(_) => RecordKindTag::Episode,
            PreparedNode::Binding(_) => RecordKindTag::Edge,
        }
    }

    /// The typed property rows for a node.
    ///
    /// Every record carries a `node` tag (the variant). Beyond that, each
    /// variant contributes the small, deterministic handful of indexable rows
    /// MemDB's property index needs to make the record findable via
    /// `find_by_property` / `find_symbol`. None of these are inferred — they are
    /// verbatim or structural facts already present on the prepared node.
    fn properties_for(node: &PreparedNode) -> Vec<Prop> {
        let fact_status = |s: FactStatus| match s {
            FactStatus::Observed => "observed",
            FactStatus::DeterministicallyDerived => "deterministically_derived",
            FactStatus::StatisticallyRanked => "statistically_ranked",
            FactStatus::LlmHypothesis => "llm_hypothesis",
        };
        let mut props = vec![Prop::string("node", node.tag())];
        match node {
            PreparedNode::Conversation(c) => {
                props.push(Prop::string("session_id", c.session_id.clone()));
                props.push(Prop::int("turn_start", c.turn_range.start as i64));
                props.push(Prop::int("turn_end", c.turn_range.end as i64));
                props.push(Prop::int("marker_count", c.markers.len() as i64));
                props.push(Prop::string("fact_status", fact_status(c.fact_status)));
            }
            PreparedNode::Decision(d) => {
                props.push(Prop::string("epitome", d.epitome.clone()));
                props.push(Prop::bool("is_ban", d.is_ban));
                props.push(Prop::int("option_count", d.considered_options.len() as i64));
                props.push(Prop::int("source_span_start", d.source_span.start as i64));
                props.push(Prop::int("source_span_end", d.source_span.end as i64));
                props.push(Prop::string("fact_status", fact_status(d.fact_status)));
            }
            PreparedNode::Episode(e) => {
                props.push(Prop::string("episode_id", e.episode_id.clone()));
                props.push(Prop::string("path", e.path.to_string_lossy().into_owned()));
                props.push(Prop::string(
                    "fact_status",
                    fact_status(FactStatus::DeterministicallyDerived),
                ));
            }
            PreparedNode::Binding(b) => {
                // The edge endpoints. The SDK's `create_record_with_properties`
                // does not expose the low-level `(out_rid, in_rid)` edge fields
                // (that is `memcore_graph::EdgeLayout::encode`, server-side), so
                // the binding's endpoints travel as indexable properties +
                // body; Memtrace resolves `from`/`to` ids to rids on its side.
                props.push(Prop::string("from", b.from.as_str()));
                props.push(Prop::string("to", b.to.as_str()));
                props.push(Prop::string("relation", relation_tag(b.relation)));
                props.push(Prop::string("fact_status", fact_status(b.fact_status)));
            }
        }
        props
    }

    /// Prepare the full `MemDbRecord` for a node: header + kind + body + props.
    fn record_for(&self, node: &PreparedNode) -> Result<MemDbRecord, SinkError> {
        let node_json =
            serde_json::to_string(node).map_err(|e| SinkError::Serialize(e.to_string()))?;
        Ok(MemDbRecord {
            header: self.header_for(node),
            kind: Self::kind_for(node),
            node_json,
            properties: Self::properties_for(node),
        })
    }
}

/// The stable snake_case tag for a binding relation, matching the `serde`
/// `rename_all = "snake_case"` on `memscribe_core::Relation`.
fn relation_tag(r: memscribe_core::Relation) -> &'static str {
    use memscribe_core::Relation;
    match r {
        Relation::Produced => "produced",
        Relation::Governs => "governs",
        Relation::DerivedFrom => "derived_from",
        Relation::CorrelatedWith => "correlated_with",
    }
}

impl Sink for MemDbSink {
    fn emit(&mut self, node: &PreparedNode) -> Result<(), SinkError> {
        let record = self.record_for(node)?;
        self.records.push(record);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::{
        BindingEdge, CodeEpisode, CommitmentMarker, ConversationSpan, DecisionRecord, Diff,
        FactStatus, MarkerCategory, NodeId, Opt, ProvRecord, Relation,
    };
    use time::macros::datetime;

    const INGEST: OffsetDateTime = datetime!(2026-06-22 12:00:00 UTC);
    const GEN: OffsetDateTime = datetime!(2026-06-22 09:30:00 UTC);

    fn episode() -> PreparedNode {
        PreparedNode::Episode(CodeEpisode {
            path: "a.rs".into(),
            diff: Diff::for_path("a.rs"),
            git: None,
            episode_id: "ep-42".into(),
        })
    }

    fn binding() -> PreparedNode {
        PreparedNode::Binding(BindingEdge {
            from: NodeId::new("decision:1"),
            to: NodeId::new("episode:ep-42"),
            relation: Relation::Produced,
            prov: ProvRecord {
                used_session: "sess-1".into(),
                used_decision: Some(NodeId::new("decision:1")),
                was_generated_by_session: "sess-1".into(),
                t_use: datetime!(2026-06-22 09:00:00 UTC),
                t_gen: GEN,
            },
            fact_status: FactStatus::DeterministicallyDerived,
            correlation: None,
        })
    }

    fn decision() -> PreparedNode {
        PreparedNode::Decision(DecisionRecord {
            epitome: "use postgres".into(),
            considered_options: vec![
                Opt {
                    text: "postgres".into(),
                    chosen: true,
                },
                Opt {
                    text: "mysql".into(),
                    chosen: false,
                },
            ],
            is_ban: false,
            superseded_by: None,
            confirmation: None,
            source_span: 1..2,
            fact_status: FactStatus::Observed,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
        })
    }

    fn conversation() -> PreparedNode {
        PreparedNode::Conversation(ConversationSpan {
            session_id: "sess-1".into(),
            turn_range: 0..3,
            text: "let's use postgres".into(),
            markers: vec![CommitmentMarker {
                rule_id: "decision_verb.use".into(),
                category: MarkerCategory::DecisionVerb,
                matched_text: "use".into(),
                offset: 6,
            }],
            fact_status: FactStatus::Observed,
            provenance: Vec::new(),
        })
    }

    // ---- bi-temporal header ------------------------------------------------

    #[test]
    fn episode_node_sets_episode_id() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit(&episode()).unwrap();
        sink.flush().unwrap();
        let header = &sink.records()[0].header;
        assert_eq!(header.episode_id.as_deref(), Some("ep-42"));
        assert_eq!(header.transaction_at, INGEST);
        // An episode has no intrinsic valid timestamp on the prepared node.
        assert_eq!(header.valid_at, None);
    }

    #[test]
    fn binding_node_valid_at_is_prov_t_gen() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit(&binding()).unwrap();
        sink.flush().unwrap();
        let header = &sink.records()[0].header;
        // valid time = when the bound edit was generated.
        assert_eq!(header.valid_at, Some(GEN));
        // transaction time = ingest time, independent of valid time.
        assert_eq!(header.transaction_at, INGEST);
        // A binding is an edge, not an episode.
        assert_eq!(header.episode_id, None);
    }

    #[test]
    fn decision_and_conversation_have_no_intrinsic_anchors() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit(&decision()).unwrap();
        sink.emit(&conversation()).unwrap();
        sink.flush().unwrap();
        for rec in sink.records() {
            assert_eq!(rec.header.valid_at, None);
            assert_eq!(rec.header.episode_id, None);
            assert_eq!(rec.header.transaction_at, INGEST);
        }
    }

    #[test]
    fn transaction_at_is_shared_across_a_batch() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit_all(&[episode(), binding(), decision()]).unwrap();
        assert_eq!(sink.records().len(), 3);
        for rec in sink.records() {
            assert_eq!(rec.header.transaction_at, INGEST);
        }
    }

    // ---- RecordKind mapping ------------------------------------------------

    #[test]
    fn record_kind_discriminants_match_memdb() {
        // Mirror `memcore_core::RecordKind`: Node=1, Edge=2, Episode=4.
        assert_eq!(RecordKindTag::Node.discriminant(), 1);
        assert_eq!(RecordKindTag::Edge.discriminant(), 2);
        assert_eq!(RecordKindTag::Episode.discriminant(), 4);
    }

    #[test]
    fn each_variant_maps_to_the_right_kind() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit_all(&[conversation(), decision(), episode(), binding()])
            .unwrap();
        let kinds: Vec<RecordKindTag> = sink.records().iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RecordKindTag::Node,    // conversation
                RecordKindTag::Node,    // decision
                RecordKindTag::Episode, // episode
                RecordKindTag::Edge,    // binding
            ]
        );
    }

    // ---- property layout, per variant --------------------------------------

    #[test]
    fn conversation_property_layout() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit(&conversation()).unwrap();
        let rec = &sink.records()[0];
        assert_eq!(rec.kind, RecordKindTag::Node);
        assert_eq!(
            rec.prop("node"),
            Some(&PropValue::String("conversation".into()))
        );
        assert_eq!(
            rec.prop("session_id"),
            Some(&PropValue::String("sess-1".into()))
        );
        assert_eq!(rec.prop("turn_start"), Some(&PropValue::Int(0)));
        assert_eq!(rec.prop("turn_end"), Some(&PropValue::Int(3)));
        assert_eq!(rec.prop("marker_count"), Some(&PropValue::Int(1)));
        assert_eq!(
            rec.prop("fact_status"),
            Some(&PropValue::String("observed".into()))
        );
        // The body is the canonical node JSON, round-trippable back to the node.
        let back: PreparedNode = serde_json::from_str(&rec.node_json).unwrap();
        assert_eq!(back, conversation());
    }

    #[test]
    fn decision_property_layout() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit(&decision()).unwrap();
        let rec = &sink.records()[0];
        assert_eq!(rec.kind, RecordKindTag::Node);
        assert_eq!(
            rec.prop("node"),
            Some(&PropValue::String("decision".into()))
        );
        assert_eq!(
            rec.prop("epitome"),
            Some(&PropValue::String("use postgres".into()))
        );
        assert_eq!(rec.prop("is_ban"), Some(&PropValue::Bool(false)));
        assert_eq!(rec.prop("option_count"), Some(&PropValue::Int(2)));
        assert_eq!(rec.prop("source_span_start"), Some(&PropValue::Int(1)));
        assert_eq!(rec.prop("source_span_end"), Some(&PropValue::Int(2)));
        assert_eq!(
            rec.prop("fact_status"),
            Some(&PropValue::String("observed".into()))
        );
    }

    #[test]
    fn episode_property_layout() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit(&episode()).unwrap();
        let rec = &sink.records()[0];
        assert_eq!(rec.kind, RecordKindTag::Episode);
        assert_eq!(rec.prop("node"), Some(&PropValue::String("episode".into())));
        // The episode_id is BOTH a header field (the arc) and an indexable prop.
        assert_eq!(
            rec.prop("episode_id"),
            Some(&PropValue::String("ep-42".into()))
        );
        assert_eq!(rec.header.episode_id.as_deref(), Some("ep-42"));
        assert_eq!(rec.prop("path"), Some(&PropValue::String("a.rs".into())));
        assert_eq!(
            rec.prop("fact_status"),
            Some(&PropValue::String("deterministically_derived".into()))
        );
    }

    #[test]
    fn binding_property_layout() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit(&binding()).unwrap();
        let rec = &sink.records()[0];
        assert_eq!(rec.kind, RecordKindTag::Edge);
        assert_eq!(rec.prop("node"), Some(&PropValue::String("binding".into())));
        // Edge endpoints travel as indexable props (the SDK create RPC has no
        // typed out_rid/in_rid; Memtrace resolves these ids to rids).
        assert_eq!(
            rec.prop("from"),
            Some(&PropValue::String("decision:1".into()))
        );
        assert_eq!(
            rec.prop("to"),
            Some(&PropValue::String("episode:ep-42".into()))
        );
        assert_eq!(
            rec.prop("relation"),
            Some(&PropValue::String("produced".into()))
        );
        assert_eq!(
            rec.prop("fact_status"),
            Some(&PropValue::String("deterministically_derived".into()))
        );
    }

    #[test]
    fn every_record_carries_a_node_tag_prop_and_nonempty_body() {
        let mut sink = MemDbSink::new(INGEST);
        sink.emit_all(&[conversation(), decision(), episode(), binding()])
            .unwrap();
        for rec in sink.records() {
            assert!(matches!(rec.prop("node"), Some(PropValue::String(_))));
            assert!(!rec.node_json.is_empty());
            assert!(rec.prop("fact_status").is_some());
        }
    }

    #[test]
    fn ban_decision_sets_is_ban_true() {
        let ban = PreparedNode::Decision(DecisionRecord {
            epitome: "never use mongodb".into(),
            considered_options: Vec::new(),
            is_ban: true,
            superseded_by: None,
            confirmation: None,
            source_span: 5..6,
            fact_status: FactStatus::Observed,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
        });
        let mut sink = MemDbSink::new(INGEST);
        sink.emit(&ban).unwrap();
        assert_eq!(
            sink.records()[0].prop("is_ban"),
            Some(&PropValue::Bool(true))
        );
    }
}
