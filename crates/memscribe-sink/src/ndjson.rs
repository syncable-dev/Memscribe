//! The NDJSON sink — the canonical, audit-friendly default. Writes one JSON
//! `PreparedNode` per line to any [`std::io::Write`] (stdout, a file, a buffer).

use memscribe_core::{PreparedNode, Sink, SinkError};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// A sink that serializes each node as one line of JSON.
pub struct NdjsonSink<W: Write> {
    writer: W,
    count: usize,
}

impl<W: Write> NdjsonSink<W> {
    /// Wrap an arbitrary writer.
    pub fn new(writer: W) -> Self {
        NdjsonSink { writer, count: 0 }
    }

    /// The number of nodes emitted so far.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Consume the sink and return the inner writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl NdjsonSink<BufWriter<io::Stdout>> {
    /// An NDJSON sink writing to stdout.
    #[must_use]
    pub fn stdout() -> Self {
        NdjsonSink::new(BufWriter::new(io::stdout()))
    }
}

impl NdjsonSink<BufWriter<File>> {
    /// An NDJSON sink writing to a file at `path` (truncating any existing file).
    ///
    /// # Errors
    /// Returns a [`SinkError`] if the file cannot be created.
    pub fn file(path: impl AsRef<Path>) -> Result<Self, SinkError> {
        let f = File::create(path).map_err(|e| SinkError::Write(e.to_string()))?;
        Ok(NdjsonSink::new(BufWriter::new(f)))
    }
}

impl<W: Write + Send> Sink for NdjsonSink<W> {
    fn emit(&mut self, node: &PreparedNode) -> Result<(), SinkError> {
        let line = serde_json::to_string(node).map_err(|e| SinkError::Serialize(e.to_string()))?;
        self.writer
            .write_all(line.as_bytes())
            .map_err(|e| SinkError::Write(e.to_string()))?;
        self.writer
            .write_all(b"\n")
            .map_err(|e| SinkError::Write(e.to_string()))?;
        self.count += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.writer
            .flush()
            .map_err(|e| SinkError::Flush(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::{CodeEpisode, Diff, PreparedNode};

    #[test]
    fn writes_one_line_per_node() {
        let node = PreparedNode::Episode(CodeEpisode {
            path: "src/lib.rs".into(),
            diff: Diff::for_path("src/lib.rs"),
            git: None,
            episode_id: "abc".into(),
        });
        let mut sink = NdjsonSink::new(Vec::new());
        sink.emit(&node).unwrap();
        sink.emit(&node).unwrap();
        sink.flush().unwrap();
        let out = String::from_utf8(sink.into_inner()).unwrap();
        assert_eq!(out.lines().count(), 2);
        // Each line is valid JSON.
        for line in out.lines() {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn roundtrips_through_json() {
        let node = PreparedNode::Episode(CodeEpisode {
            path: "a.rs".into(),
            diff: Diff::for_path("a.rs"),
            git: None,
            episode_id: "id".into(),
        });
        let mut sink = NdjsonSink::new(Vec::new());
        sink.emit(&node).unwrap();
        sink.flush().unwrap();
        let out = String::from_utf8(sink.into_inner()).unwrap();
        let back: PreparedNode = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(back, node);
    }
}
