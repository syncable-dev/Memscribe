//! The linear, deterministic pipeline (whitepaper §3).
//!
//! `Source → Adapter` produces a normalized [`CaptureEvent`] stream; this module
//! turns that stream into [`PreparedNode`]s via Gate → Segmenter → Binder →
//! NodePrep, applies the optional redaction pass, and writes to a [`Sink`].
//! Everything here is pure and synchronous given the event stream.

use crate::adapter::{ParseCtx, RawRecord, TranscriptAdapter};
use crate::binder::{Binder, DefaultBinder};
use crate::error::{PipelineError, SinkError};
use crate::gate::CommitmentGate;
use crate::model::CaptureEvent;
use crate::node::PreparedNode;
use crate::nodeprep::{DefaultNodePrep, NodePrep};
use crate::redact::Redactor;
use crate::segmenter::{DefaultSegmenter, Segmenter};
use crate::sink::Sink;

/// The default pipeline with the standard stages. Construct with [`Self::new`]
/// (redaction on by default) or [`Self::without_redaction`].
#[derive(Debug)]
pub struct DefaultPipeline {
    /// The commitment-marker gate.
    pub gate: CommitmentGate,
    /// The segmenter stage.
    pub segmenter: DefaultSegmenter,
    /// The binder stage.
    pub binder: DefaultBinder,
    /// The node-prep stage.
    pub nodeprep: DefaultNodePrep,
    /// The redaction pass, if enabled.
    pub redactor: Option<Redactor>,
}

impl Default for DefaultPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultPipeline {
    /// A pipeline with default stages and redaction **on** (the safe default).
    #[must_use]
    pub fn new() -> Self {
        DefaultPipeline {
            gate: CommitmentGate::default_table(),
            segmenter: DefaultSegmenter,
            binder: DefaultBinder,
            nodeprep: DefaultNodePrep,
            redactor: Some(Redactor::default()),
        }
    }

    /// A pipeline with redaction disabled (e.g. for golden tests that assert on
    /// verbatim content).
    #[must_use]
    pub fn without_redaction() -> Self {
        DefaultPipeline {
            redactor: None,
            ..Self::new()
        }
    }

    /// Replace the gate (e.g. with a config-driven rule table).
    #[must_use]
    pub fn with_gate(mut self, gate: CommitmentGate) -> Self {
        self.gate = gate;
        self
    }

    /// Replace the redactor (e.g. `--no-content` mode), or pass `None` to
    /// disable redaction.
    #[must_use]
    pub fn with_redactor(mut self, redactor: Option<Redactor>) -> Self {
        self.redactor = redactor;
        self
    }

    /// Transform a normalized event stream into prepared nodes. **Pure**: the
    /// output is an exact function of `events`.
    #[must_use]
    pub fn prepare_events(&self, events: &[CaptureEvent]) -> Vec<PreparedNode> {
        let seg = self.segmenter.segment(events, &self.gate);
        let bindings = self.binder.bind(&seg);
        let mut nodes = self.nodeprep.prepare(&seg, bindings);
        if let Some(r) = &self.redactor {
            for n in &mut nodes {
                r.redact_node(n);
            }
        }
        nodes
    }

    /// Run an adapter over raw records (skipping malformed records — adapters
    /// route unrecognized-but-valid records to `Unknown`, so a real `Err` here
    /// is a genuinely broken line that is skipped-and-flagged), then prepare the
    /// resulting nodes.
    #[must_use]
    pub fn run_records(
        &self,
        adapter: &dyn TranscriptAdapter,
        records: &[RawRecord],
    ) -> Vec<PreparedNode> {
        let (events, _ctx) = parse_records(adapter, records);
        self.prepare_events(&events)
    }

    /// Run the full pipeline to a sink. Returns the number of nodes emitted.
    ///
    /// # Errors
    /// Returns a [`PipelineError`] if the sink fails to emit or flush.
    pub fn run_to_sink(
        &self,
        adapter: &dyn TranscriptAdapter,
        records: &[RawRecord],
        sink: &mut dyn Sink,
    ) -> Result<usize, PipelineError> {
        let nodes = self.run_records(adapter, records);
        emit_all(sink, &nodes)?;
        Ok(nodes.len())
    }
}

/// Parse a batch of raw records with a fresh context, collecting the normalized
/// events. Malformed records (a real `Err`) are skipped; the stream stays
/// lossless for well-formed input because adapters emit `Unknown` rather than
/// erroring on unrecognized records.
#[must_use]
pub fn parse_records(
    adapter: &dyn TranscriptAdapter,
    records: &[RawRecord],
) -> (Vec<CaptureEvent>, ParseCtx) {
    let mut ctx = ParseCtx::new();
    let mut events = Vec::new();
    for r in records {
        if let Ok(evs) = adapter.parse(r, &mut ctx) {
            events.extend(evs);
        }
    }
    (events, ctx)
}

/// Emit every node to a sink, then flush.
fn emit_all(sink: &mut dyn Sink, nodes: &[PreparedNode]) -> Result<(), SinkError> {
    for node in nodes {
        sink.emit(node)?;
    }
    sink.flush()
}
