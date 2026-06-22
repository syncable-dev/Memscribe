//! Golden snapshots for every fixture (whitepaper §8.1).
//!
//! For each `fixtures/<tool>/<version>/<case>.jsonl` we snapshot **two** things
//! with `insta`:
//!
//! 1. the normalized [`CaptureEvent`] stream (`testkit::parse_events`), and
//! 2. the prepared [`PreparedNode`] stream (`testkit::prepare_nodes`).
//!
//! The committed `.snap` files under `tests/snapshots/` are the golden record. A
//! future diff is then unambiguous: either a real regression in an adapter / the
//! pipeline, or an *intended* format change that the author re-accepts with
//! `cargo insta accept` (or `INSTA_UPDATE=always`). Because the pipeline is a
//! pure function of the input bytes and the *relative* fixture path we feed it,
//! these snapshots are byte-stable and machine-independent.
//!
//! First run (writes the snapshots):
//! ```text
//! INSTA_UPDATE=always cargo test -p memscribe-testkit --test golden
//! ```
//! Then re-run without the env var to prove they are stable.

use insta::assert_json_snapshot;
use memscribe_core::SourceKind;
use memscribe_testkit::golden::{discover_cases, fixtures_dir, GoldenCase};
use memscribe_testkit::{parse_events, prepare_nodes};
use std::path::{Path, PathBuf};

/// The stable, machine-independent path we feed the pipeline so provenance and
/// any path-derived ids are identical on every machine.
fn relative_input_path(case: &GoldenCase) -> PathBuf {
    Path::new("fixtures")
        .join(&case.tool)
        .join(&case.version)
        .join(format!("{}.jsonl", case.case))
}

/// Resolve the tool slug to a [`SourceKind`], skipping any fixture directory
/// that is not a known tool (so a stray directory never fails the suite).
fn source_kind(case: &GoldenCase) -> Option<SourceKind> {
    SourceKind::parse(&case.tool).filter(|k| *k != SourceKind::Unknown)
}

/// A stable per-fixture snapshot name. Slashes/dots in the slugs are normalized
/// so insta writes one `.snap` file per fixture under `tests/snapshots/`.
fn snap_name(prefix: &str, case: &GoldenCase) -> String {
    let sanitize = |s: &str| s.replace(['.', '/', '\\', ' '], "_");
    format!(
        "{prefix}__{}__{}__{}",
        sanitize(&case.tool),
        sanitize(&case.version),
        sanitize(&case.case),
    )
}

#[test]
fn golden_snapshots_for_every_fixture() {
    // Pin the snapshot directory and strip insta's auto-prepended module path so
    // the file names are exactly `snapshots/<prefix>__<tool>__<version>__<case>.snap`.
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path("snapshots");
    settings.set_prepend_module_to_snapshot(false);

    let cases = discover_cases();
    assert!(
        !cases.is_empty(),
        "no fixtures discovered under {}",
        fixtures_dir().display()
    );

    let mut snapped = 0;
    settings.bind(|| {
        for case in &cases {
            let Some(kind) = source_kind(case) else {
                panic!(
                    "fixture tool slug {:?} does not resolve to a known SourceKind",
                    case.tool
                );
            };

            let bytes = case
                .read_input()
                .unwrap_or_else(|e| panic!("read fixture {:?}: {e}", case.input_path()));
            let rel = relative_input_path(case);

            // 1. Normalized events.
            let events = parse_events(kind, &bytes, &rel);
            assert_json_snapshot!(snap_name("events", case), events);

            // 2. Prepared nodes (redaction off, so content is verbatim).
            let nodes = prepare_nodes(kind, &bytes, &rel);
            assert_json_snapshot!(snap_name("nodes", case), nodes);

            snapped += 1;
        }
    });

    assert_eq!(
        snapped,
        cases.len(),
        "every discovered fixture must be snapshotted"
    );
}
