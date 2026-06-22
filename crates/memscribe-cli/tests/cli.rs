//! Black-box integration tests for the `memscribe` binary (assert_cmd).
//!
//! These drive the real compiled CLI end-to-end against the repo's fixtures and
//! temp files, asserting the user-visible contracts: `parse` emits valid NDJSON,
//! `--as` forces an adapter, `redact` strips a seeded secret, `hook` consumes
//! stdin and exits 0, unknown tools fail cleanly, and `--help` works.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};

/// The workspace root (three levels up from this test crate's manifest dir:
/// `crates/memscribe-cli` → `crates` → workspace).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .expect("workspace root resolvable from CARGO_MANIFEST_DIR")
}

/// A known-good Claude Code fixture (its path contains `claude`, so the tool is
/// inferable without `--as`).
fn claude_fixture() -> PathBuf {
    workspace_root().join("fixtures/claude_code/2.0/happy_path_decision_then_edits.jsonl")
}

fn memscribe() -> Command {
    Command::cargo_bin("memscribe").expect("the `memscribe` binary builds")
}

/// Every non-blank stdout line must be a parseable JSON object.
fn assert_all_lines_are_json_objects(stdout: &[u8]) {
    let text = String::from_utf8(stdout.to_vec()).expect("stdout is valid UTF-8");
    let mut lines = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line is not JSON: {line:?}: {e}"));
        assert!(
            value.is_object(),
            "each NDJSON line must be a JSON object, got: {line:?}"
        );
    }
    assert!(lines > 0, "parse must emit at least one NDJSON node");
}

#[test]
fn parse_fixture_emits_valid_ndjson() {
    let assert = memscribe()
        .arg("parse")
        .arg(claude_fixture())
        .assert()
        .success();
    assert_all_lines_are_json_objects(&assert.get_output().stdout);
}

#[test]
fn parse_with_explicit_as_tool() {
    // Force the adapter rather than inferring; output must still be valid NDJSON.
    let assert = memscribe()
        .arg("parse")
        .arg("--as")
        .arg("claude_code")
        .arg(claude_fixture())
        .assert()
        .success();
    assert_all_lines_are_json_objects(&assert.get_output().stdout);
}

#[test]
fn parse_unknown_tool_errors_cleanly() {
    // An unrecognized `--as` value must fail with a clear message, not a panic.
    memscribe()
        .arg("parse")
        .arg("--as")
        .arg("not-a-real-tool")
        .arg(claude_fixture())
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown tool"));
}

#[test]
fn redact_strips_a_seeded_secret() {
    // Seed a realistically-shaped Anthropic key (the default pattern requires
    // 16+ trailing chars) and assert the redactor removes it from stdout.
    let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA";
    let body = format!("here is a key: {secret}\nplain second line\n");

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("leaky.txt");
    std::fs::write(&file, &body).unwrap();

    let assert = memscribe().arg("redact").arg(&file).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        !stdout.contains(secret),
        "the seeded secret must be gone from stdout, got:\n{stdout}"
    );
    // Non-secret content survives.
    assert!(
        stdout.contains("plain second line"),
        "non-secret content must be preserved"
    );
}

#[test]
fn hook_reads_stdin_and_exits_zero() {
    // The hook handler must consume stdin and exit 0 immediately — never block.
    memscribe()
        .arg("hook")
        .write_stdin(
            r#"{"session_id":"s1","transcript_path":"/tmp/x.jsonl","hook_event_name":"PostToolUse","tool_name":"Edit"}"#,
        )
        .assert()
        .success();
}

#[test]
fn hook_with_invalid_json_still_exits_zero() {
    // Invalid JSON on stdin must not crash the hook — it still exits 0.
    memscribe()
        .arg("hook")
        .write_stdin("this is not json")
        .assert()
        .success();
}

#[test]
fn help_works() {
    memscribe()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("memscribe"))
        .stdout(predicate::str::contains("watch"))
        .stdout(predicate::str::contains("parse"));
}

#[test]
fn verify_passes_over_fixtures() {
    // `verify` parses every fixture and prints a per-tool PASS/FAIL table; with
    // the shipped (green) fixtures it must succeed and report PASS.
    memscribe()
        .arg("verify")
        .assert()
        .success()
        .stdout(predicate::str::contains("PASS"))
        .stdout(predicate::str::contains("claude_code"));
}

#[test]
fn watch_once_drains_a_discovered_transcript() {
    // `watch --once --root <dir>` discovers the seeded transcript, parses it, and
    // emits prepared nodes to stdout (the ndjson sink). It must exit cleanly and
    // not hang (no live tail loop in `--once`).
    let dir = tempfile::tempdir().unwrap();
    // The filename carries `claude` so the tool is inferable.
    let transcript = dir.path().join("claude-session.jsonl");
    std::fs::copy(claude_fixture(), &transcript).unwrap();

    let assert = memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--tools")
        .arg("claude_code")
        .arg("--root")
        .arg(dir.path())
        .assert()
        .success();

    // Whatever it emitted must be valid NDJSON (a non-empty transcript yields
    // at least one node).
    assert_all_lines_are_json_objects(&assert.get_output().stdout);
}

#[test]
fn watch_unknown_tool_errors_cleanly() {
    memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--tools")
        .arg("not-a-real-tool")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown tool"));
}
