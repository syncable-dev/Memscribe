//! Node preparation: assemble the final [`PreparedNode`] stream (whitepaper §3,
//! §8.8).
//!
//! The node-prep stage takes a [`Segmentation`] and the binder's edges and emits
//! a deterministically-ordered stream of prepared nodes. Order is chronological
//! by originating seq, with a stable per-kind tiebreak, so the output is
//! byte-stable for golden tests.

use crate::node::{BindingEdge, PreparedNode};
use crate::segmenter::Segmentation;

/// The node-prep stage.
pub trait NodePrep {
    /// Assemble the final prepared-node stream.
    fn prepare(&self, seg: &Segmentation, bindings: Vec<BindingEdge>) -> Vec<PreparedNode>;
}

/// The default deterministic node-prep.
#[derive(Debug, Default)]
pub struct DefaultNodePrep;

/// A node carrying its deterministic sort key: `(primary_seq, kind_rank)`.
struct Keyed {
    seq: u64,
    rank: u8,
    secondary: String,
    node: PreparedNode,
}

impl NodePrep for DefaultNodePrep {
    fn prepare(&self, seg: &Segmentation, bindings: Vec<BindingEdge>) -> Vec<PreparedNode> {
        let mut keyed: Vec<Keyed> = Vec::new();

        for c in &seg.conversations {
            keyed.push(Keyed {
                seq: c.turn_range.start,
                rank: 0,
                secondary: c.session_id.clone(),
                node: PreparedNode::Conversation(c.clone()),
            });
        }
        for d in &seg.decisions {
            keyed.push(Keyed {
                seq: d.turn_seq,
                rank: 1,
                secondary: d.node_id.0.clone(),
                node: PreparedNode::Decision(d.record.clone()),
            });
        }
        for e in &seg.episodes {
            keyed.push(Keyed {
                seq: e.seq,
                rank: 2,
                secondary: e.node_id.0.clone(),
                node: PreparedNode::Episode(e.episode.clone()),
            });
        }
        for b in bindings {
            // Bindings sort just after the episode they generate.
            let secondary = format!("{}->{}", b.from, b.to);
            keyed.push(Keyed {
                seq: u64::MAX,
                rank: 3,
                secondary,
                node: PreparedNode::Binding(b),
            });
        }

        keyed.sort_by(|a, b| {
            a.seq
                .cmp(&b.seq)
                .then(a.rank.cmp(&b.rank))
                .then_with(|| a.secondary.cmp(&b.secondary))
        });

        keyed.into_iter().map(|k| k.node).collect()
    }
}
