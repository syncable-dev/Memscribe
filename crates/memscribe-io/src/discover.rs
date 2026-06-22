//! Transcript discovery.
//!
//! A small, dependency-light helper that walks a directory tree and collects the
//! transcript files a [`crate::tailer::LiveTailer`] should watch (or the one-shot
//! reader should replay). It is generic over the set of extensions a tool uses
//! (`jsonl`, `json`, `zst`, ...) and reports, per file, whether it is a `.zst`
//! cold rollout so the caller can route it to the decompressing reader.
//!
//! Discovery is deterministic (results are sorted by path), panic-free, and
//! tolerant of unreadable subtrees: directories it cannot descend are skipped
//! rather than aborting the whole walk.

use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// One discovered transcript file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Discovered {
    /// Absolute (or root-relative) path to the transcript file.
    pub path: PathBuf,
    /// Whether the file is a `.zst` (Codex cold rollout) needing decompression.
    pub is_zst: bool,
}

/// Whether a path's extension matches one of `exts` (case-insensitive, no dot).
fn ext_matches(path: &Path, exts: &[&str]) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => exts.iter().any(|want| want.eq_ignore_ascii_case(ext)),
        None => false,
    }
}

/// Recursively find transcript files under `root` whose extension is in `exts`.
///
/// `exts` are bare extensions without the leading dot (e.g. `["jsonl", "zst"]`)
/// and are matched case-insensitively. Symlinks are not followed (avoids cycles
/// and surprise escapes from the watched tree). The result is sorted for
/// determinism. A `.zst` file is flagged via [`Discovered::is_zst`] regardless of
/// whether `"zst"` itself was requested — e.g. `transcript.jsonl.zst` matched by
/// the `"zst"` ext is still a zst.
///
/// Unreadable entries are silently skipped; this never panics and never returns
/// an error — a missing or non-directory `root` simply yields an empty list.
#[must_use]
pub fn find_transcripts(root: impl AsRef<Path>, exts: &[&str]) -> Vec<Discovered> {
    let mut out: Vec<Discovered> = WalkDir::new(root.as_ref())
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| ext_matches(p, exts))
        .map(|path| {
            let is_zst = path.extension().and_then(|e| e.to_str()) == Some("zst");
            Discovered { path, is_zst }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The just the paths of [`find_transcripts`], convenient for seeding a watcher.
#[must_use]
pub fn find_transcript_paths(root: impl AsRef<Path>, exts: &[&str]) -> Vec<PathBuf> {
    find_transcripts(root, exts)
        .into_iter()
        .map(|d| d.path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn finds_nested_by_extension_and_flags_zst() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("a.jsonl"));
        touch(&root.join("nested/b.jsonl"));
        touch(&root.join("nested/deep/c.jsonl.zst"));
        touch(&root.join("ignore.txt"));
        touch(&root.join("nested/ignore.md"));

        let found = find_transcripts(root, &["jsonl", "zst"]);
        assert_eq!(found.len(), 3, "three transcripts, txt/md ignored");

        // Deterministic order (sorted by path).
        let mut sorted = found.clone();
        sorted.sort();
        assert_eq!(found, sorted);

        let zst: Vec<_> = found.iter().filter(|d| d.is_zst).collect();
        assert_eq!(zst.len(), 1);
        assert!(zst[0].path.ends_with("c.jsonl.zst"));
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("upper.JSONL"));
        let found = find_transcripts(root, &["jsonl"]);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn missing_root_yields_empty_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(find_transcripts(&missing, &["jsonl"]).is_empty());
    }

    #[test]
    fn paths_helper_returns_only_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("a.jsonl"));
        let paths = find_transcript_paths(root, &["jsonl"]);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("a.jsonl"));
    }

    #[test]
    fn empty_exts_matches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.jsonl"));
        assert!(find_transcripts(dir.path(), &[]).is_empty());
    }
}
