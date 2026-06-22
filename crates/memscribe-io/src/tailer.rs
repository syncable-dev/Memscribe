//! The file tailer.
//!
//! [`poll_new_records`] is the crash-safe core: it reads the bytes appended to a
//! transcript since the last persisted offset, stops at the last complete line
//! (so a half-written final line is never emitted — it resumes there next poll),
//! advances the offset, and returns the new records with file-relative
//! provenance. The notify-based live watcher (feature `watch`) is layered on top
//! of this and is the fleet's task to wire.

use crate::cursor_store::OffsetStore;
use crate::records::read_records_from_bytes;
use memscribe_core::RawRecord;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Read records appended since the last persisted offset for `key`, advancing
/// the offset to the last complete line. Returns the new records (empty if none
/// or only a partial trailing line is available).
///
/// # Errors
/// Returns an [`io::Error`] if the file cannot be opened or read.
pub fn poll_new_records(
    path: &Path,
    store: &mut dyn OffsetStore,
    key: &str,
) -> io::Result<Vec<RawRecord>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = store.get(key).unwrap_or(0).min(len);
    if start >= len {
        return Ok(Vec::new());
    }
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    // Only consume up to the last newline; a partial final line is left for the
    // next poll. This is what makes restart mid-line lossless and duplicate-free.
    let consumable = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    if consumable == 0 {
        return Ok(Vec::new());
    }

    let mut recs = read_records_from_bytes(&buf[..consumable], path);
    for r in &mut recs {
        // Provenance offsets become file-absolute.
        r.location.byte_offset += start;
    }
    store.set(key, start + consumable as u64);
    Ok(recs)
}

/// The live, notify-based tailer (feature `watch`).
///
/// [`LiveTailer`] watches a set of transcript files and, on each debounced
/// create/modify event, delegates to the crash-safe [`poll_new_records`] core to
/// emit only the records appended since the last persisted offset. It owns one
/// [`OffsetStore`] for all watched files (keyed by path), so a restart resumes
/// exactly where it left off.
///
/// Watching is done at the *directory* level (the parent of each watched file),
/// which is what makes rotation and late-created files work: when a tool
/// truncates-and-rewrites or only creates the transcript after the tailer has
/// started, the directory watch still delivers the event and the file is picked
/// up on the next poll. A file whose offset is ahead of a shrunken file (log
/// rotation / truncation) is handled by [`poll_new_records`], which clamps the
/// start offset to the current length.
///
/// The API is blocking and deterministic:
/// - [`LiveTailer::poll`] waits up to a timeout for the next debounced batch and
///   returns the emitted records (empty on timeout).
/// - [`LiveTailer::run`] loops, invoking a callback for each batch, until the
///   watcher is dropped or the callback returns [`ControlFlow::Break`].
///
/// It never panics: watcher/IO errors are surfaced as `tracing` warnings and the
/// affected batch is skipped rather than unwinding.
#[cfg(feature = "watch")]
pub mod live {
    use super::poll_new_records;
    use crate::cursor_store::OffsetStore;
    use memscribe_core::RawRecord;
    use notify::{EventKind, RecursiveMode, Watcher};
    use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, FileIdMap};
    use std::collections::BTreeSet;
    use std::ops::ControlFlow;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Duration;

    /// What a [`LiveTailer`] should do after a batch is handled by `run`.
    pub type Batch = Vec<RawRecord>;

    /// A canonical key for a watched path. We use the lossy string form of the
    /// path the caller registered, so the offset store key is stable across
    /// polls (we deliberately do not canonicalize on disk — a rotated/recreated
    /// file keeps the same logical key).
    fn key_for(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// The live notify-based tailer. Generic over the [`OffsetStore`] so it can
    /// drive either the in-memory or the SQLite-backed cursor.
    pub struct LiveTailer<S: OffsetStore> {
        store: S,
        // Files we emit records for, by their registered (logical) path.
        interest: BTreeSet<PathBuf>,
        // Parent directories actually handed to the OS watcher.
        watched_dirs: BTreeSet<PathBuf>,
        debouncer: Debouncer<notify::RecommendedWatcher, FileIdMap>,
        rx: Receiver<DebounceEventResult>,
    }

    impl<S: OffsetStore> LiveTailer<S> {
        /// Create a tailer with the given offset store and debounce timeout.
        ///
        /// `timeout` is the notify-debouncer-full coalescing window; a value
        /// around 100–300ms keeps latency low while collapsing the burst of
        /// events a single append produces.
        ///
        /// # Errors
        /// Returns a [`notify::Error`] if the OS watcher cannot be created.
        pub fn new(store: S, timeout: Duration) -> notify::Result<Self> {
            let (tx, rx) = std::sync::mpsc::channel();
            // `std::sync::mpsc::Sender` implements `DebounceEventHandler`.
            let debouncer = new_debouncer(timeout, None, tx)?;
            Ok(Self {
                store,
                interest: BTreeSet::new(),
                watched_dirs: BTreeSet::new(),
                debouncer,
                rx,
            })
        }

        /// Register a single transcript file to tail. The file need not exist yet
        /// — the nearest existing ancestor directory is watched recursively, so a
        /// later-created file (even one whose parent directory does not exist yet)
        /// is still picked up. Re-registering a path is idempotent.
        ///
        /// # Errors
        /// Returns a [`notify::Error`] only if a *new* directory cannot be watched;
        /// an already-watched directory is a no-op. A path with no existing
        /// ancestor at all is recorded in the interest set and retried lazily on
        /// the next [`watch_path`] / [`poll_existing`] — it never errors.
        ///
        /// [`poll_existing`]: LiveTailer::poll_existing
        pub fn watch_path(&mut self, path: impl AsRef<Path>) -> notify::Result<()> {
            let path = path.as_ref().to_path_buf();
            self.interest.insert(path.clone());
            self.ensure_ancestor_watch(&path)
        }

        /// Watch the nearest *existing* ancestor directory of `path`. If the
        /// immediate parent exists we watch it non-recursively (cheap, exact). If
        /// it does not yet exist (e.g. a tool that creates `~/.tool/sessions/`
        /// lazily), we climb to the nearest existing ancestor and watch *that*
        /// recursively, so the eventual creation of the file is observed. Already
        /// watched directories are a no-op.
        fn ensure_ancestor_watch(&mut self, path: &Path) -> notify::Result<()> {
            // The directory the file lives in (own parent, or "." for a bare name).
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

            // Walk up to the first ancestor that exists on disk right now.
            let mut candidate = parent.as_path();
            let mut recursive = RecursiveMode::NonRecursive;
            loop {
                if candidate.is_dir() {
                    break;
                }
                match candidate.parent() {
                    Some(up) if !up.as_os_str().is_empty() => {
                        candidate = up;
                        // We dropped below the exact parent → must watch
                        // recursively to catch the not-yet-created subtree.
                        recursive = RecursiveMode::Recursive;
                    }
                    _ => {
                        // No existing ancestor at all (very unusual). Keep it in
                        // the interest set; it is retried on the next register.
                        return Ok(());
                    }
                }
            }

            let dir = candidate.to_path_buf();
            if self.watched_dirs.insert(dir.clone()) {
                self.debouncer.watcher().watch(&dir, recursive)?;
            }
            Ok(())
        }

        /// Register many transcript files at once (see [`watch_path`]).
        ///
        /// [`watch_path`]: LiveTailer::watch_path
        ///
        /// # Errors
        /// Returns the first [`notify::Error`] encountered while registering.
        pub fn watch_paths<P: AsRef<Path>>(
            &mut self,
            paths: impl IntoIterator<Item = P>,
        ) -> notify::Result<()> {
            for p in paths {
                self.watch_path(p)?;
            }
            Ok(())
        }

        /// Immediately poll every watched file once for records appended since the
        /// last offset, without waiting for an event. Useful to drain content that
        /// already existed (or was written before the watcher started) on startup.
        ///
        /// Also lazily (re)establishes the directory watch for any interested path
        /// whose ancestor directory has since come into existence — closing the
        /// window where a file registered before its directory existed would
        /// otherwise never be watched.
        ///
        /// Records are returned in deterministic (sorted-by-path) order.
        pub fn poll_existing(&mut self) -> Batch {
            let mut out = Vec::new();
            // `interest` is a BTreeSet → iteration is sorted → deterministic.
            let paths: Vec<PathBuf> = self.interest.iter().cloned().collect();
            for path in &paths {
                // Best-effort: retry the ancestor watch in case the directory was
                // created after registration. A failure here is non-fatal.
                let _ = self.ensure_ancestor_watch(path);
            }
            for path in paths {
                self.drain_path(&path, &mut out);
            }
            out
        }

        /// Block up to `timeout` for the next debounced batch and return the
        /// records it produced. Returns an empty batch on timeout (so the caller
        /// can interleave other work) and an empty batch — never an error — if the
        /// watcher reported a recoverable error (it is logged via `tracing`).
        ///
        /// Returns `None` only when the watcher has shut down and no further events
        /// will ever arrive (the sender was dropped), signalling end-of-stream.
        pub fn poll(&mut self, timeout: Duration) -> Option<Batch> {
            match self.rx.recv_timeout(timeout) {
                Ok(result) => Some(self.handle_result(result)),
                Err(RecvTimeoutError::Timeout) => Some(Vec::new()),
                Err(RecvTimeoutError::Disconnected) => None,
            }
        }

        /// Run the blocking tail loop, invoking `on_batch` for each non-empty
        /// debounced batch. The loop ends when the watcher disconnects or
        /// `on_batch` returns [`ControlFlow::Break`]. Empty batches (timeouts) are
        /// not delivered to the callback.
        ///
        /// `tick` bounds how long a single `recv` blocks, so the loop stays
        /// responsive to a `Break` even when the tree is idle.
        pub fn run<F>(&mut self, tick: Duration, mut on_batch: F)
        where
            F: FnMut(Batch) -> ControlFlow<()>,
        {
            loop {
                match self.poll(tick) {
                    Some(batch) => {
                        if batch.is_empty() {
                            continue;
                        }
                        if on_batch(batch).is_break() {
                            return;
                        }
                    }
                    None => return,
                }
            }
        }

        /// Test seam: feed a synthetic debounced *modify* event for `paths`
        /// through the exact same translation path the real watcher uses, so the
        /// event→records logic can be tested deterministically without depending
        /// on the platform's (notoriously timing-dependent) filesystem
        /// notifications. Not part of the public API.
        #[cfg(test)]
        pub(crate) fn handle_synthetic_modify<P: AsRef<Path>>(
            &mut self,
            paths: impl IntoIterator<Item = P>,
        ) -> Batch {
            use notify::event::{Event, EventKind as Ek, ModifyKind};
            use notify_debouncer_full::DebouncedEvent;
            let mut event = Event::new(Ek::Modify(ModifyKind::Any));
            for p in paths {
                event = event.add_path(p.as_ref().to_path_buf());
            }
            let debounced = DebouncedEvent::new(event, std::time::Instant::now());
            self.handle_result(Ok(vec![debounced]))
        }

        /// Turn one debounced result into the records to emit. Errors are logged
        /// and yield an empty batch — the tailer is panic-free by contract.
        fn handle_result(&mut self, result: DebounceEventResult) -> Batch {
            let mut out = Vec::new();
            let events = match result {
                Ok(events) => events,
                Err(errors) => {
                    for e in errors {
                        tracing::warn!(error = %e, "live tailer watch error; skipping batch");
                    }
                    return out;
                }
            };

            // Collect the distinct interested paths touched by create/modify
            // events, then drain each once in deterministic (sorted) order.
            let mut touched: BTreeSet<PathBuf> = BTreeSet::new();
            for ev in events {
                if !matches!(
                    ev.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any
                ) {
                    continue;
                }
                for p in &ev.paths {
                    if self.interest.contains(p) {
                        touched.insert(p.clone());
                    }
                }
            }
            for path in touched {
                self.drain_path(&path, &mut out);
            }
            out
        }

        /// Poll one path, appending its new records to `out`. A missing file
        /// (late create / rotation gap) is treated as "no records yet", not an
        /// error.
        ///
        /// Handles **truncation/rotation**: if the file is now shorter than the
        /// persisted offset, the transcript was rewritten in place (the tool
        /// truncated and started over). The stored offset is stale, so we reset it
        /// to `0` before delegating to the crash-safe core — which then reads the
        /// fresh content from the start. This never re-emits old content (the old
        /// bytes are gone) and never loses the new content.
        fn drain_path(&mut self, path: &Path, out: &mut Batch) {
            let key = key_for(path);
            if let Ok(meta) = std::fs::metadata(path) {
                let len = meta.len();
                if self.store.get(&key).is_some_and(|off| off > len) {
                    tracing::debug!(
                        path = %path.display(),
                        "transcript shrank below cursor; treating as rotation, resetting offset"
                    );
                    self.store.set(&key, 0);
                }
            }
            match poll_new_records(path, &mut self.store, &key) {
                Ok(mut recs) => out.append(&mut recs),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // File not present yet (or mid-rotation). Nothing to emit.
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "live tailer poll error");
                }
            }
        }

        /// Borrow the underlying offset store (e.g. to flush/inspect).
        pub fn store(&self) -> &S {
            &self.store
        }

        /// Mutably borrow the underlying offset store.
        pub fn store_mut(&mut self) -> &mut S {
            &mut self.store
        }
    }
}

#[cfg(feature = "watch")]
pub use live::LiveTailer;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor_store::MemoryOffsetStore;
    use std::io::Write;

    #[test]
    fn resumes_from_offset_with_no_loss_or_dup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut store = MemoryOffsetStore::new();

        std::fs::write(&path, b"a\nb\n").unwrap();
        let first = poll_new_records(&path, &mut store, "s").unwrap();
        assert_eq!(first.len(), 2);

        // Append more; only the new records come back.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"c\n").unwrap();
        let second = poll_new_records(&path, &mut store, "s").unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].as_str(), Some("c"));
    }

    #[test]
    fn partial_final_line_is_held_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut store = MemoryOffsetStore::new();

        std::fs::write(&path, b"a\nb\npartial").unwrap();
        let recs = poll_new_records(&path, &mut store, "s").unwrap();
        assert_eq!(recs.len(), 2, "partial trailing line must be held back");

        // Once the line is completed, it is delivered exactly once.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"_done\n").unwrap();
        let recs = poll_new_records(&path, &mut store, "s").unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].as_str(), Some("partial_done"));
    }

    /// Crash/replay invariant (whitepaper §8.5): a process that dies after
    /// consuming some complete lines but while a final line is still being
    /// written must, on resume with the *persisted* offset, emit every record
    /// exactly once — zero loss, zero duplication — regardless of where the
    /// crash truncated the stream.
    ///
    /// We simulate this by persisting the offset to a fresh store on each
    /// "process restart" (the offset is the only state that survives a crash),
    /// growing the file one fragment at a time, and crashing mid-line.
    #[test]
    fn crash_mid_line_resumes_with_zero_loss_zero_dup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");

        // The full intended transcript: three complete records.
        let full = b"first\nsecond\nthird\n";

        // The "crash schedule": a sequence of byte prefixes of `full` that the
        // file is observed at across restarts. Several land *inside* a line
        // (truncated final line) — the dangerous case.
        let crash_prefixes = [
            6usize, // "first\n"            (boundary)
            9,      // "first\nsec"         (mid second line — TRUNCATED)
            13,     // "first\nsecond"      (mid second line — TRUNCATED, no \n)
            14,     // "first\nsecond\n"    (boundary)
            17,     // "first\nsecond\nthi" (mid third line — TRUNCATED)
            full.len(),
        ];

        // The single piece of state that survives a crash.
        let mut persisted_offset: u64 = 0;
        let mut emitted: Vec<String> = Vec::new();

        for &prefix in &crash_prefixes {
            // The file as it exists at this crash point.
            std::fs::write(&path, &full[..prefix]).unwrap();

            // "Restart": a brand-new store seeded ONLY with the persisted offset.
            let mut store = MemoryOffsetStore::new();
            if persisted_offset > 0 {
                store.set("s", persisted_offset);
            }

            let recs = poll_new_records(&path, &mut store, "s").unwrap();
            for r in &recs {
                emitted.push(r.as_str().unwrap().to_string());
            }

            // Persist whatever offset the tailer advanced to (it only advances
            // past complete lines), exactly as a real crash-safe cursor would.
            persisted_offset = store.get("s").unwrap_or(persisted_offset);
        }

        // Zero loss + zero dup: every record once, in order.
        assert_eq!(
            emitted,
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ],
            "each record must be emitted exactly once across crashes"
        );
        // The offset finished exactly at EOF.
        assert_eq!(persisted_offset, full.len() as u64);
    }

    /// A re-poll at the *same* persisted offset (e.g. the watcher fires twice for
    /// one append, or the process restarts without any new bytes) must be a
    /// no-op — this is the duplicate-suppression half of the invariant.
    #[test]
    fn repoll_without_new_bytes_emits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut store = MemoryOffsetStore::new();

        std::fs::write(&path, b"x\ny\n").unwrap();
        assert_eq!(poll_new_records(&path, &mut store, "s").unwrap().len(), 2);
        // No new bytes; repeated polls (incl. a spurious watcher wakeup).
        assert!(poll_new_records(&path, &mut store, "s").unwrap().is_empty());
        assert!(poll_new_records(&path, &mut store, "s").unwrap().is_empty());
    }
}

#[cfg(all(test, feature = "watch"))]
mod live_tests {
    //! These tests exercise the `LiveTailer` translation logic deterministically
    //! via the `handle_synthetic_modify` seam. They deliberately do **not** rely
    //! on the operating system actually delivering filesystem notifications:
    //! FSEvents/inotify delivery for short-lived temp files is timing-dependent
    //! and would make the suite flaky (and, on a watcher that never fires, hang).
    //! The real OS path (`watch_path` → debouncer → `poll`) is wired and
    //! compiled; here we validate that a debounced modify event produces exactly
    //! the newly-appended records, with the same crash-safe offset semantics the
    //! core guarantees.

    use super::live::LiveTailer;
    use crate::cursor_store::MemoryOffsetStore;
    use std::io::Write;
    use std::time::Duration;

    fn append(path: &std::path::Path, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
    }

    fn texts(batch: &[memscribe_core::RawRecord]) -> Vec<String> {
        batch
            .iter()
            .map(|r| r.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn new_and_watch_path_succeed_without_error() {
        // Constructing the real OS watcher and registering paths must work even
        // for not-yet-existing files (the parent dir is watched).
        let dir = tempfile::tempdir().unwrap();
        let mut tailer =
            LiveTailer::new(MemoryOffsetStore::new(), Duration::from_millis(50)).unwrap();
        tailer
            .watch_paths([dir.path().join("a.jsonl"), dir.path().join("sub/b.jsonl")])
            .unwrap();
        // Re-registering a path / a sibling in an already-watched dir is a no-op.
        tailer.watch_path(dir.path().join("a.jsonl")).unwrap();
    }

    #[test]
    fn poll_existing_drains_pre_written_content_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pre.jsonl");
        std::fs::write(&path, b"a\nb\n").unwrap();

        let mut tailer =
            LiveTailer::new(MemoryOffsetStore::new(), Duration::from_millis(50)).unwrap();
        tailer.watch_path(&path).unwrap();

        assert_eq!(texts(&tailer.poll_existing()), vec!["a", "b"]);
        // A second drain with no new bytes yields nothing (offset persisted).
        assert!(tailer.poll_existing().is_empty());
    }

    #[test]
    fn modify_event_emits_only_new_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.jsonl");
        std::fs::write(&path, b"seed\n").unwrap();

        let mut tailer =
            LiveTailer::new(MemoryOffsetStore::new(), Duration::from_millis(50)).unwrap();
        tailer.watch_path(&path).unwrap();
        // Consume the seed so we only observe the live append.
        let _ = tailer.poll_existing();

        append(&path, b"live-one\nlive-two\n");
        let got = tailer.handle_synthetic_modify([&path]);
        assert_eq!(texts(&got), vec!["live-one", "live-two"]);
        // A duplicate event for the same (unchanged) file emits nothing — the
        // offset already advanced. This is the dedup guarantee under spurious
        // debouncer wakeups.
        assert!(tailer.handle_synthetic_modify([&path]).is_empty());
    }

    #[test]
    fn late_created_file_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late.jsonl"); // does not exist yet

        let mut tailer =
            LiveTailer::new(MemoryOffsetStore::new(), Duration::from_millis(50)).unwrap();
        tailer.watch_path(&path).unwrap();
        // A modify event for a not-yet-existing file is harmless (NotFound is
        // swallowed) and emits nothing.
        assert!(tailer.handle_synthetic_modify([&path]).is_empty());

        // Now it appears; the create/modify event tails it from byte 0.
        append(&path, b"born-late\n");
        let got = tailer.handle_synthetic_modify([&path]);
        assert_eq!(texts(&got), vec!["born-late"]);
    }

    #[test]
    fn rotation_truncation_does_not_lose_or_duplicate() {
        // A tool that truncates-and-rewrites its transcript (log rotation) must
        // not crash the tailer or replay stale content. poll_new_records clamps
        // the start offset to the (now smaller) length.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rot.jsonl");
        std::fs::write(&path, b"old-a\nold-b\n").unwrap();

        let mut tailer =
            LiveTailer::new(MemoryOffsetStore::new(), Duration::from_millis(50)).unwrap();
        tailer.watch_path(&path).unwrap();
        assert_eq!(texts(&tailer.poll_existing()), vec!["old-a", "old-b"]);

        // Rotation: the file shrinks below the persisted offset, then new lines.
        std::fs::write(&path, b"new-a\n").unwrap();
        let got = tailer.handle_synthetic_modify([&path]);
        assert_eq!(
            texts(&got),
            vec!["new-a"],
            "post-rotation content tails cleanly"
        );
    }

    #[test]
    fn only_interested_paths_in_event_are_drained() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("watched.jsonl");
        let sibling = dir.path().join("sibling.jsonl");
        std::fs::write(&watched, b"mine\n").unwrap();
        std::fs::write(&sibling, b"not-mine\n").unwrap();

        let mut tailer =
            LiveTailer::new(MemoryOffsetStore::new(), Duration::from_millis(50)).unwrap();
        tailer.watch_path(&watched).unwrap();

        // An event mentioning both paths only drains the registered one.
        let got = tailer.handle_synthetic_modify([watched.clone(), sibling.clone()]);
        assert_eq!(texts(&got), vec!["mine"]);
    }
}
