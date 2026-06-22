//! The MemDB sink (feature `memdb`, **off by default**).
//!
//! This is the single seam Memtrace turns on to ingest [`PreparedNode`]s into
//! MemDB with their **bi-temporal** headers. The two time axes (see `MEMDB.md`
//! and MemDB's `memcore_core::RecordHeader`) are:
//!
//! * **`valid_at`** — *valid time*: when the fact was true in the world. For
//!   Memscribe that is the **turn / episode time** — the moment the agent edit
//!   happened or the dialogue turn occurred — **not** when we ingested it.
//! * **`transaction_at`** — *transaction time*: when MemDB learned the fact,
//!   i.e. our **ingest time**. One sink instance stamps a single
//!   `transaction_at` for the whole batch so a replayed transcript lands at one
//!   coherent transaction instant.
//! * **`episode_id`** — the **arc / episode** the node belongs to. MemDB keys
//!   its episodic, co-change and provenance machinery off this id
//!   (`RecordHeader::episode_id`).
//!
//! Memscribe is fully usable **without** MemDB: NDJSON is the default sink and
//! this module only compiles when the `memdb` feature is enabled. Until the real
//! gRPC client (`memcore_client::MemcoreClient`) is wired in (see `MEMDB.md` for
//! the field-by-field mapping onto `RecordHeader` / `create_record_with_properties`),
//! this sink prepares each node into an in-memory [`MemDbRecord`] carrying the
//! correct bi-temporal shape, which Memtrace's own integration test asserts
//! against.

use memscribe_core::{PreparedNode, Sink, SinkError};
use time::OffsetDateTime;

/// Bi-temporal coordinates written alongside each node in MemDB.
///
/// These map directly onto MemDB's `memcore_core::RecordHeader`: `valid_at` →
/// `RecordHeader::valid_at`, `transaction_at` is the wall-clock at which the
/// `create_record` RPC is issued, and `episode_id` → `RecordHeader::episode_id`.
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

/// A node prepared for MemDB ingestion.
#[derive(Debug, Clone)]
pub struct MemDbRecord {
    /// The bi-temporal header.
    pub header: BiTemporal,
    /// The node's canonical JSON.
    pub node_json: String,
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
}

impl Sink for MemDbSink {
    fn emit(&mut self, node: &PreparedNode) -> Result<(), SinkError> {
        let node_json =
            serde_json::to_string(node).map_err(|e| SinkError::Serialize(e.to_string()))?;
        self.records.push(MemDbRecord {
            header: self.header_for(node),
            node_json,
        });
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
        BindingEdge, CodeEpisode, ConversationSpan, DecisionRecord, Diff, FactStatus, NodeId,
        ProvRecord, Relation,
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
            considered_options: Vec::new(),
            is_ban: false,
            superseded_by: None,
            confirmation: None,
            source_span: 1..2,
            fact_status: FactStatus::Observed,
        })
    }

    fn conversation() -> PreparedNode {
        PreparedNode::Conversation(ConversationSpan {
            session_id: "sess-1".into(),
            turn_range: 0..3,
            text: "let's use postgres".into(),
            markers: Vec::new(),
            fact_status: FactStatus::Observed,
            provenance: Vec::new(),
        })
    }

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
}
