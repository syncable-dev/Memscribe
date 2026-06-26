//! The SQLite sink (feature `sqlite`) — a queryable local store with no external
//! service. Each node is stored as JSON alongside its variant tag, its
//! [`FactStatus`](memscribe_core::FactStatus), and a deterministic primary key
//! derived from the node's own identity (so re-emitting the same node is an
//! upsert, not a duplicate).

use memscribe_core::{PreparedNode, Sink, SinkError};
use rusqlite::Connection;
use std::path::Path;

/// A sink that writes nodes into a local SQLite database.
pub struct SqliteSink {
    conn: Connection,
    count: usize,
}

impl SqliteSink {
    /// Open (or create) a SQLite database at `path`.
    ///
    /// # Errors
    /// Returns a [`SinkError`] if the database cannot be opened or initialized.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SinkError> {
        let conn = Connection::open(path).map_err(|e| SinkError::Write(e.to_string()))?;
        Self::init(conn)
    }

    /// An in-memory SQLite database (for tests).
    ///
    /// # Errors
    /// Returns a [`SinkError`] if the database cannot be initialized.
    pub fn in_memory() -> Result<Self, SinkError> {
        let conn = Connection::open_in_memory().map_err(|e| SinkError::Write(e.to_string()))?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, SinkError> {
        // `pk` is the node's own stable identity (see `primary_key`), so an
        // `INSERT OR REPLACE` re-emit of the same node updates in place rather
        // than appending a duplicate row. `fact_status` is indexed alongside
        // `node_type` so consumers can filter on epistemic status without
        // re-parsing the JSON blob.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS nodes (
                pk          TEXT PRIMARY KEY,
                node_type   TEXT NOT NULL,
                fact_status TEXT NOT NULL,
                json        TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes(node_type);
            CREATE INDEX IF NOT EXISTS idx_nodes_fact_status ON nodes(fact_status);",
        )
        .map_err(|e| SinkError::Write(e.to_string()))?;
        Ok(SqliteSink { conn, count: 0 })
    }

    /// The number of `emit` calls accepted so far. Note this counts emissions,
    /// not distinct rows: re-emitting a node with the same [`primary_key`] is an
    /// upsert, so the table may hold fewer rows than this value.
    ///
    /// [`primary_key`]: SqliteSink::primary_key
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Count the rows currently stored for a given variant tag (e.g. `"episode"`,
    /// `"binding"`, `"decision"`, `"conversation"` — see [`PreparedNode::tag`]).
    ///
    /// # Errors
    /// Returns a [`SinkError`] if the query fails.
    pub fn query_count_by_type(&self, node_type: &str) -> Result<u64, SinkError> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE node_type = ?1",
                rusqlite::params![node_type],
                |r| r.get(0),
            )
            .map_err(|e| SinkError::Write(e.to_string()))?;
        // COUNT(*) is non-negative; the cast is lossless for any real table size.
        Ok(n.max(0) as u64)
    }

    /// A deterministic primary key for a node, derived from the node's own
    /// identity so the same logical node always maps to the same row. The key is
    /// namespaced by the variant tag so two different variants can never collide.
    ///
    /// The identity per variant:
    /// * `Episode`   → its `episode_id`.
    /// * `Binding`   → `from→to` plus the relation (a directed, typed edge).
    /// * `Decision`  → the session-agnostic turn span it was parsed from.
    /// * `Conversation` → session id plus its turn range.
    #[must_use]
    pub fn primary_key(node: &PreparedNode) -> String {
        let tag = node.tag();
        match node {
            PreparedNode::Episode(e) => format!("{tag}:{}", e.episode_id),
            PreparedNode::Binding(b) => {
                format!("{tag}:{}->{}:{:?}", b.from, b.to, b.relation)
            }
            PreparedNode::Decision(d) => {
                format!("{tag}:{}..{}", d.source_span.start, d.source_span.end)
            }
            PreparedNode::Conversation(c) => {
                format!(
                    "{tag}:{}:{}..{}",
                    c.session_id, c.turn_range.start, c.turn_range.end
                )
            }
        }
    }
}

impl Sink for SqliteSink {
    fn emit(&mut self, node: &PreparedNode) -> Result<(), SinkError> {
        let json = serde_json::to_string(node).map_err(|e| SinkError::Serialize(e.to_string()))?;
        let fact = format!("{:?}", node.fact_status());
        let pk = Self::primary_key(node);
        self.conn
            .execute(
                "INSERT OR REPLACE INTO nodes (pk, node_type, fact_status, json)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![pk, node.tag(), fact, json],
            )
            .map_err(|e| SinkError::Write(e.to_string()))?;
        self.count += 1;
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
        PreparedNode, ProvRecord, Relation,
    };
    use time::OffsetDateTime;

    fn episode() -> PreparedNode {
        PreparedNode::Episode(CodeEpisode {
            path: "a.rs".into(),
            diff: Diff::for_path("a.rs"),
            git: None,
            episode_id: "ep-1".into(),
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

    fn decision() -> PreparedNode {
        PreparedNode::Decision(DecisionRecord {
            epitome: "use postgres".into(),
            considered_options: Vec::new(),
            is_ban: false,
            superseded_by: None,
            confirmation: None,
            source_span: 1..2,
            fact_status: FactStatus::Observed,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
        })
    }

    fn binding() -> PreparedNode {
        PreparedNode::Binding(BindingEdge {
            from: NodeId::new("decision:1"),
            to: NodeId::new("episode:ep-1"),
            relation: Relation::Produced,
            prov: ProvRecord {
                used_session: "sess-1".into(),
                used_decision: Some(NodeId::new("decision:1")),
                was_generated_by_session: "sess-1".into(),
                t_use: OffsetDateTime::UNIX_EPOCH,
                t_gen: OffsetDateTime::UNIX_EPOCH,
            },
            fact_status: FactStatus::DeterministicallyDerived,
            correlation: None,
        })
    }

    #[test]
    fn inserts_nodes() {
        let mut sink = SqliteSink::in_memory().unwrap();
        sink.emit(&episode()).unwrap();
        sink.flush().unwrap();
        assert_eq!(sink.count(), 1);
        let n: i64 = sink
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn stores_all_four_variants_and_reads_them_back() {
        let mut sink = SqliteSink::in_memory().unwrap();
        let nodes = [conversation(), decision(), episode(), binding()];
        for n in &nodes {
            sink.emit(n).unwrap();
        }
        sink.flush().unwrap();

        // One row per variant tag.
        for tag in ["conversation", "decision", "episode", "binding"] {
            assert_eq!(sink.query_count_by_type(tag).unwrap(), 1, "tag {tag}");
        }
        assert_eq!(sink.query_count_by_type("nonexistent").unwrap(), 0);

        // The stored fact_status matches each node's own status, and the JSON
        // round-trips back to the exact node.
        for original in &nodes {
            let pk = SqliteSink::primary_key(original);
            let (fact, json): (String, String) = sink
                .conn
                .query_row(
                    "SELECT fact_status, json FROM nodes WHERE pk = ?1",
                    rusqlite::params![pk],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(fact, format!("{:?}", original.fact_status()));
            let back: PreparedNode = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, original);
        }
    }

    #[test]
    fn primary_key_is_stable_so_re_emit_upserts() {
        let mut sink = SqliteSink::in_memory().unwrap();
        sink.emit(&episode()).unwrap();
        sink.emit(&episode()).unwrap();
        sink.flush().unwrap();
        // Two emits, but the same identity → a single row.
        assert_eq!(sink.count(), 2);
        assert_eq!(sink.query_count_by_type("episode").unwrap(), 1);
    }
}
