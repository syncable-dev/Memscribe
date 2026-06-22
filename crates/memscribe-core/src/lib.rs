//! # memscribe-core
//!
//! The deterministic, zero-LLM **contract** at the heart of Memscribe.
//!
//! This crate defines the thin waist that decouples per-tool adapters from
//! everything downstream:
//!
//! - The normalized event model ([`CaptureEvent`] / [`EventKind`]) — the system
//!   of record produced by adapters.
//! - The output contract ([`node::PreparedNode`]) — the typed nodes a consumer
//!   (MemCortex / Memtrace) ingests, each carrying a [`node::FactStatus`].
//! - The [`TranscriptAdapter`] and [`Sink`] traits — the two plug points.
//! - The deterministic pipeline: [`gate`] → [`segmenter`] → [`binder`] →
//!   [`nodeprep`], plus a [`redact`] pass.
//!
//! Everything here is a pure function of its input. No model is ever called.
//! That is what makes Memscribe golden-file, property, and fuzz testable.
#![forbid(unsafe_code)]

pub mod adapter;
pub mod binder;
pub mod error;
pub mod gate;
pub mod model;
pub mod node;
pub mod nodeprep;
pub mod pipeline;
pub mod redact;
pub mod segmenter;
pub mod sink;

pub use adapter::{
    DiscoverCfg, ParseCtx, RawRecord, SchemaVariant, TranscriptAdapter, TranscriptHandle,
};
pub use binder::{Binder, DefaultBinder};
pub use error::{ParseError, PipelineError, SinkError};
pub use gate::{CommitmentGate, GateRule};
pub use model::{
    content_id, CaptureEvent, Diff, EventKind, GitRef, Part, ProjectRef, SourceKind,
    SourceLocation, Timestamp, Usage, SCHEMA_VERSION,
};
pub use node::{
    BindingEdge, CheckRef, CodeEpisode, CommitmentMarker, ConversationSpan, CorrelationTuple,
    DecisionRecord, FactStatus, MarkerCategory, NodeId, Opt, PreparedNode, ProvRecord, Relation,
};
pub use nodeprep::{DefaultNodePrep, NodePrep};
pub use pipeline::DefaultPipeline;
pub use redact::Redactor;
pub use segmenter::{DecisionCandidate, DefaultSegmenter, EpisodeRecord, Segmentation, Segmenter};
pub use sink::{Sink, VecSink};
