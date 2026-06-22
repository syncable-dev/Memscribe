//! One-shot record reading with exact provenance.
//!
//! Splits a transcript into newline-delimited [`RawRecord`]s, each carrying its
//! byte offset and 1-based line number, transparently decompressing `.zst`
//! (Codex cold rollouts). This is the deterministic reader behind `memscribe
//! parse` and the golden-file harness.

use memscribe_core::{RawRecord, SourceLocation};
use std::io;
use std::path::Path;

/// Split raw bytes into newline-delimited records with byte/line provenance.
/// A trailing newline yields no extra record; blank lines are preserved as
/// empty records (adapters skip them) so byte offsets stay exact.
#[must_use]
pub fn read_records_from_bytes(bytes: &[u8], path: &Path) -> Vec<RawRecord> {
    let mut out = Vec::new();
    let mut offset: u64 = 0;
    let mut line_no: u64 = 1;
    let mut start = 0usize;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let line = &bytes[start..i];
            out.push(RawRecord::new(
                line.to_vec(),
                SourceLocation::new(path, offset, line_no),
            ));
            offset = (i + 1) as u64;
            line_no += 1;
            start = i + 1;
        }
    }
    // Trailing bytes with no final newline form one last record.
    if start < bytes.len() {
        out.push(RawRecord::new(
            bytes[start..].to_vec(),
            SourceLocation::new(path, offset, line_no),
        ));
    }
    out
}

/// Read a transcript file into records, decompressing `.zst` transparently.
///
/// # Errors
/// Returns an [`io::Error`] if the file cannot be read or decompressed.
pub fn read_records(path: impl AsRef<Path>) -> io::Result<Vec<RawRecord>> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)?;
    let bytes = if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        zstd::decode_all(&bytes[..])?
    } else {
        bytes
    };
    Ok(read_records_from_bytes(&bytes, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn splits_lines_with_offsets() {
        let recs = read_records_from_bytes(b"a\nbb\nccc", Path::new("t.jsonl"));
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].location.byte_offset, 0);
        assert_eq!(recs[0].location.line_no, 1);
        assert_eq!(recs[1].location.byte_offset, 2);
        assert_eq!(recs[1].location.line_no, 2);
        assert_eq!(recs[2].location.byte_offset, 5);
        assert_eq!(recs[2].as_str(), Some("ccc"));
    }

    #[test]
    fn trailing_newline_yields_no_empty_record() {
        let recs = read_records_from_bytes(b"a\nb\n", Path::new("t.jsonl"));
        assert_eq!(recs.len(), 2);
    }

    /// Offset-resumption property (whitepaper §8.3): splitting at any record
    /// boundary and concatenating yields the same records as reading the whole.
    #[test]
    fn split_then_concat_equals_whole() {
        let data = b"one\ntwo\nthree\nfour";
        let whole: Vec<Vec<u8>> = read_records_from_bytes(data, Path::new("t"))
            .into_iter()
            .map(|r| r.bytes)
            .collect();
        for split in 0..=data.len() {
            // Only split on record boundaries (start of file, end of file, or
            // immediately after a newline) — that is where a real tailer resumes.
            let on_boundary = split == 0 || split == data.len() || data[split - 1] == b'\n';
            if !on_boundary {
                continue;
            }
            let mut combined: Vec<Vec<u8>> =
                read_records_from_bytes(&data[..split], Path::new("t"))
                    .into_iter()
                    .map(|r| r.bytes)
                    .collect();
            combined.extend(
                read_records_from_bytes(&data[split..], Path::new("t"))
                    .into_iter()
                    .map(|r| r.bytes),
            );
            assert_eq!(combined, whole, "split at {split}");
        }
    }
}
