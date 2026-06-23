//! # memscribe-testkit
//!
//! The test harness that makes Memscribe's determinism a property you can run.
//! It provides:
//!
//! - [`parse_events`] / [`prepare_nodes`] — drive a tool's adapter and the
//!   pipeline over raw bytes, the way every golden and property test does.
//! - [`invariants`] — reusable checks for the whitepaper §8.3 invariants
//!   (determinism, monotonic seq, losslessness, idempotency).
//! - [`golden`] — fixture path resolution and load/compare helpers.
//! - [`scenarios`] — the cross-tool conformance scenario catalog (§8.2).
#![forbid(unsafe_code)]

pub mod golden;
pub mod invariants;
pub mod scenarios;

use memscribe_adapters::adapter_for;
use memscribe_core::{
    pipeline::parse_records, CaptureEvent, DefaultPipeline, PreparedNode, RawRecord, SourceKind,
    StoreReader, TranscriptHandle,
};
use memscribe_io::read_records_from_bytes;
use std::path::Path;

/// Parse a tool's transcript bytes into the normalized event stream.
///
/// # Panics
/// Panics if the adapter for `tool` is not compiled into the build.
#[must_use]
pub fn parse_events(tool: SourceKind, jsonl: &[u8], path: &Path) -> Vec<CaptureEvent> {
    let recs = records_for(tool, jsonl, path);
    let adapter = adapter_for(tool).expect("adapter feature must be enabled for this tool");
    let (events, _ctx) = parse_records(adapter.as_ref(), &recs);
    events
}

/// Parse and prepare a tool's transcript bytes into the prepared-node stream
/// (redaction off, so tests can assert on verbatim content).
///
/// # Panics
/// Panics if the adapter for `tool` is not compiled into the build.
#[must_use]
pub fn prepare_nodes(tool: SourceKind, jsonl: &[u8], path: &Path) -> Vec<PreparedNode> {
    let recs = records_for(tool, jsonl, path);
    let adapter = adapter_for(tool).expect("adapter feature must be enabled for this tool");
    DefaultPipeline::without_redaction().run_records(adapter.as_ref(), &recs)
}

/// Count non-blank lines — the lower bound on events for line-delimited inputs.
#[must_use]
pub fn count_nonblank_lines(jsonl: &[u8]) -> usize {
    String::from_utf8_lossy(jsonl)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// Count the logical input records a tool sees for these bytes and path.
///
/// For line-delimited transcripts this is the non-blank line count; for native
/// adapters whose real reader expands an on-disk store or whole-document export,
/// this reflects the adapter's own recordization so the losslessness floor matches
/// production behavior.
#[must_use]
pub fn count_input_records(tool: SourceKind, bytes: &[u8], path: &Path) -> usize {
    records_for(tool, bytes, path)
        .into_iter()
        .filter(record_is_nonblank)
        .count()
}

/// Load raw records the same way production would for this tool/path.
fn records_for(tool: SourceKind, bytes: &[u8], path: &Path) -> Vec<RawRecord> {
    let adapter = adapter_for(tool).expect("adapter feature must be enabled for this tool");

    if adapter.store_reader() == StoreReader::Native
        && path.exists()
        && !is_line_delimited_path(path)
    {
        let handle = TranscriptHandle {
            path: path.to_path_buf(),
            source: tool,
            session_hint: path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string),
            compressed: false,
        };
        if let Ok(records) = adapter.read_native(&handle) {
            return records;
        }
    }

    read_records_from_bytes(bytes, path)
}

fn is_line_delimited_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("jsonl" | "ndjson" | "zst")
    )
}

fn record_is_nonblank(record: &RawRecord) -> bool {
    record
        .as_str()
        .map_or_else(|| !record.bytes.is_empty(), |s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(name: &str, ext: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("memscribe-testkit-{name}-{nanos}.{ext}"))
    }

    #[test]
    fn parse_events_uses_native_reader_for_existing_vscode_json_export() {
        let path = unique_temp_path("vscode-native-export", "json");
        let json = r#"[
  {
    "sessionId": "sess-1",
    "requests": [
      {
        "requestId": "r1",
        "message": { "text": "Tell me about memscribe" },
        "response": [
          { "value": "Memscribe is deterministic." }
        ]
      }
    ]
  }
]"#;
        std::fs::write(&path, json).expect("write temp vscode export");

        let events = parse_events(SourceKind::VsCode, json.as_bytes(), &path);
        let tags: Vec<&'static str> = events.iter().map(|e| e.kind.tag()).collect();

        let _ = std::fs::remove_file(&path);

        assert_eq!(tags, vec!["user_turn", "assistant_turn"]);
    }

    #[test]
    fn count_input_records_treats_native_vscode_json_as_one_session_record() {
        let path = unique_temp_path("vscode-native-count", "json");
        let json = r#"[
  {
    "sessionId": "sess-1",
    "requests": [
      { "requestId": "r1", "message": { "text": "one" }, "response": [{ "value": "A" }] },
      { "requestId": "r2", "message": { "text": "two" }, "response": [{ "value": "B" }] }
    ]
  }
]"#;
        std::fs::write(&path, json).expect("write temp vscode export");

        let count = count_input_records(SourceKind::VsCode, json.as_bytes(), &path);

        let _ = std::fs::remove_file(&path);

        assert_eq!(
            count, 1,
            "one session document should remain one raw record"
        );
    }
}
