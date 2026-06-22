//! The [`Sink`] trait — the single seam that decouples Memscribe from MemDB
//! (whitepaper §6).
//!
//! Nothing in the pipeline knows what a sink does with a node. Concrete sinks
//! (NDJSON, SQLite, and a feature-gated MemDB sink) live in `memscribe-sink`.
//! Because the canonical default is NDJSON, the entire module is observable and
//! testable without MemDB present.

use crate::error::SinkError;
use crate::node::PreparedNode;

/// A consumer of prepared nodes.
pub trait Sink: Send {
    /// Emit one prepared node.
    fn emit(&mut self, node: &PreparedNode) -> Result<(), SinkError>;
    /// Flush any buffered nodes.
    fn flush(&mut self) -> Result<(), SinkError>;

    /// Emit every node in a slice, then flush. Convenience for batch use. Kept
    /// object-safe (concrete slice, no generics) so `&mut dyn Sink` works.
    fn emit_all(&mut self, nodes: &[PreparedNode]) -> Result<(), SinkError> {
        for node in nodes {
            self.emit(node)?;
        }
        self.flush()
    }
}

/// An in-memory sink that collects nodes. Useful for tests, the conformance
/// harness, and `replay`.
#[derive(Debug, Default, Clone)]
pub struct VecSink {
    /// The collected nodes, in emission order.
    pub nodes: Vec<PreparedNode>,
}

impl VecSink {
    /// A fresh, empty collecting sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume the sink and return the collected nodes.
    #[must_use]
    pub fn into_nodes(self) -> Vec<PreparedNode> {
        self.nodes
    }
}

impl Sink for VecSink {
    fn emit(&mut self, node: &PreparedNode) -> Result<(), SinkError> {
        self.nodes.push(node.clone());
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}
