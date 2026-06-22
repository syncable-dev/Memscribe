//! Integration tests for the `memscribe.toml` config loader and
//! `verify --capture` (assert_cmd). Hermetic: every test writes its inputs into
//! a `tempdir` and asserts the user-visible behavior of the real compiled CLI.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};

fn memscribe() -> Command {
    Command::cargo_bin("memscribe").expect("the `memscribe` binary builds")
}

/// The workspace root (three levels up from this test crate's manifest dir).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .expect("workspace root resolvable from CARGO_MANIFEST_DIR")
}

/// A known-good Claude Code fixture (path contains `claude`, so it is inferable).
fn claude_fixture() -> PathBuf {
    workspace_root().join("fixtures/claude_code/2.0/happy_path_decision_then_edits.jsonl")
}

/// One Claude-Code-shaped user turn whose text carries `body` verbatim. The path
/// inference and adapter both recognize this as `claude_code`.
fn claude_user_turn(body: &str) -> String {
    // A single, minimal JSONL line — a user message with plain-string content.
    format!(
        r#"{{"type":"user","uuid":"11111111-1111-4111-8111-111111111111","parentUuid":null,"timestamp":"2026-06-22T10:00:00.000Z","sessionId":"sess-cfg-001","cwd":"/tmp/proj","gitBranch":"main","version":"2.0.5","isSidechain":false,"message":{{"role":"user","content":{}}}}}"#,
        serde_json::to_string(body).unwrap()
    )
}

/// Collect the emitted NDJSON nodes from a successful run's stdout.
fn nodes_from_stdout(stdout: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8(stdout.to_vec()).expect("stdout is valid UTF-8");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each NDJSON line parses"))
        .collect()
}

/// `watch --once --config <toml>` must consume the config: a CUSTOM commitment
/// rule (replacing the default table) gates a turn the defaults would ignore, and
/// a CUSTOM redaction pattern (replacing the default set) strips a secret the
/// defaults would miss. This is the end-to-end round-trip of TASK A.
#[test]
fn config_custom_gate_and_redaction_round_trip() {
    let dir = tempfile::tempdir().unwrap();

    // A user turn whose only commitment marker is the custom word `banana`
    // (no default verb fires on it), carrying a secret only the custom pattern
    // matches.
    let transcript = dir.path().join("claude-session.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n",
            claude_user_turn("we will banana the orders service; key BANANA-1234")
        ),
    )
    .unwrap();

    // A config that REPLACES the default gate + redaction tables.
    let config = dir.path().join("memscribe.toml");
    std::fs::write(
        &config,
        r#"
[capture]
tools = ["claude_code"]

[[gate.rules]]
id = "custom.banana"
category = "decision_verb"
pattern = "banana"

[redact]
no_content = false

[[redact.patterns]]
label = "banana_token"
pattern = "BANANA-[0-9]{4}"
"#,
    )
    .unwrap();

    let assert = memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--config")
        .arg(&config)
        .arg("--root")
        .arg(dir.path())
        .assert()
        .success();

    let nodes = nodes_from_stdout(&assert.get_output().stdout);
    let blob = serde_json::to_string(&nodes).unwrap();

    // The custom gate admitted the `banana` turn → at least one node was emitted.
    assert!(
        !nodes.is_empty(),
        "the custom gate rule must admit the `banana` turn; got no nodes"
    );
    // The custom redaction pattern replaced the secret with its label.
    assert!(
        blob.contains("[REDACTED:banana_token]"),
        "the custom redaction pattern must fire; nodes:\n{blob}"
    );
    // And the raw secret must be gone.
    assert!(
        !blob.contains("BANANA-1234"),
        "the raw custom secret must not survive; nodes:\n{blob}"
    );
}

/// The config's `[capture].tools` selects the tool set when `--tools` is omitted.
/// With a `claude_code`-only config and a discoverable claude transcript, the run
/// drains it and emits valid NDJSON.
#[test]
fn config_capture_tools_selects_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("claude-session.jsonl");
    std::fs::copy(claude_fixture(), &transcript).unwrap();

    let config = dir.path().join("memscribe.toml");
    std::fs::write(
        &config,
        r#"
[capture]
tools = ["claude_code"]
"#,
    )
    .unwrap();

    let assert = memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--config")
        .arg(&config)
        .arg("--root")
        .arg(dir.path())
        .assert()
        .success();

    let nodes = nodes_from_stdout(&assert.get_output().stdout);
    assert!(
        !nodes.is_empty(),
        "the discovered transcript must yield nodes"
    );
}

/// A malformed config must fail with a clear message, not a panic.
#[test]
fn config_invalid_pattern_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("memscribe.toml");
    std::fs::write(
        &config,
        r#"
[[gate.rules]]
id = "bad"
category = "imperative"
pattern = "("
"#,
    )
    .unwrap();

    memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--config")
        .arg(&config)
        .arg("--root")
        .arg(dir.path())
        .assert()
        .failure();
}

/// `verify --capture <session> --as <tool>` must snapshot the transcript into a
/// new fixture under `<corpus>/<tool>/captured/<name>.<ext>`. The corpus root is
/// redirected to a tempdir via `MEMSCRIBE_FIXTURES_DIR`, so the test is fully
/// hermetic and never touches the repo's real `fixtures/`.
#[test]
fn verify_capture_writes_a_fixture() {
    let corpus = tempfile::tempdir().unwrap();
    let unique = "sample_session";

    let captured_dir = corpus.path().join("claude_code").join("captured");
    let dest = captured_dir.join(format!("{unique}.jsonl"));
    let nodes_dest = captured_dir.join(format!("{unique}.nodes.ndjson"));

    memscribe()
        .env("MEMSCRIBE_FIXTURES_DIR", corpus.path())
        .arg("verify")
        .arg("--capture")
        .arg(claude_fixture())
        .arg("--as")
        .arg("claude_code")
        .arg("--name")
        .arg(unique)
        .arg("--with-nodes")
        .assert()
        .success()
        .stdout(predicate::str::contains("captured"));

    // The raw transcript fixture was written, byte-for-byte equal to the source.
    assert!(
        dest.is_file(),
        "captured fixture must exist at {}",
        dest.display()
    );
    let written = std::fs::read(&dest).unwrap();
    let original = std::fs::read(claude_fixture()).unwrap();
    assert_eq!(
        written, original,
        "captured fixture must be a verbatim copy"
    );

    // `--with-nodes` wrote the prepared nodes alongside, as valid NDJSON.
    assert!(
        nodes_dest.is_file(),
        "prepared nodes must be written at {}",
        nodes_dest.display()
    );
    let nodes_text = std::fs::read_to_string(&nodes_dest).unwrap();
    let mut lines = 0usize;
    for line in nodes_text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        let v: serde_json::Value = serde_json::from_str(line).expect("nodes line parses");
        assert!(v.is_object(), "each captured node is a JSON object");
    }
    assert!(
        lines > 0,
        "the captured sample must yield at least one node"
    );
}

/// `verify --capture` on a session it cannot resolve must fail cleanly (it never
/// admits an unresolvable sample to the corpus).
#[test]
fn verify_capture_unknown_tool_fails() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("mystery.jsonl");
    std::fs::write(&session, "{}\n").unwrap();

    memscribe()
        .arg("verify")
        .arg("--capture")
        .arg(&session)
        .arg("--as")
        .arg("not-a-real-tool")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown tool"));
}
