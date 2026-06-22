//! # memscribe-sink
//!
//! Concrete [`memscribe_core::Sink`] implementations. The canonical, default
//! sink is [`NdjsonSink`] — one JSON node per line — which makes the whole
//! module observable and testable without any external service. [`SqliteSink`]
//! (feature `sqlite`) gives a queryable local store, and `MemDbSink` (feature
//! `memdb`, off by default) writes into MemDB for Memtrace.
#![forbid(unsafe_code)]

pub mod ndjson;
pub use ndjson::NdjsonSink;

#[cfg(feature = "sqlite")]
pub mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteSink;

#[cfg(feature = "memdb")]
pub mod memdb;
#[cfg(feature = "memdb")]
pub use memdb::MemDbSink;
