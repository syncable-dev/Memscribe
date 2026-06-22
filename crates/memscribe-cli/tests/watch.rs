//! End-to-end black-box tests for the `memscribe watch` command (assert_cmd).
//!
//! These prove the full `watch` pipeline against the real compiled binary and a
//! hermetic temp root: **discover → parse → DefaultPipeline → sink**. Each test
//! seeds a committed `claude_code` fixture under a tempdir, runs `watch`, and
//! asserts the prepared-node stream that lands in the sink — by *kind*, not just
//! "some bytes came out". The whitepaper §8.2 `happy_path_decision_then_edits`
//! scenario must normalize to at least one Conversation, one Decision, and one
//! Episode node, so that is exactly what we count.
//!
//! Coverage:
//! - `--once` + `--sink ndjson --out <file>` → drains a discovered transcript to
//!   a file and the file contains the expected node kinds.
//! - `--once` + `--sink ndjson` (stdout, the default) → same node kinds on stdout.
//! - `--once` + `--sink sqlite --out <db>` → rows land in the `nodes` table,
//!   counted by `node_type` straight out of the SQLite database.
//! - an empty root → no output, exit 0 (the "nothing discovered" floor).
//! - live mode (no `--once`) → tailing a growing transcript emits nodes and the
//!   process shuts down cleanly on Ctrl-C, with a HOME-overridden cursor store so
//!   the test never touches the real `~/.local/state`.

use assert_cmd::Command as AssertCommand;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The workspace root (two levels up from this crate's manifest dir:
/// `crates/memscribe-cli` → `crates` → workspace).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .expect("workspace root resolvable from CARGO_MANIFEST_DIR")
}

/// The committed Claude Code happy-path fixture: a decision turn followed by
/// edits to N files → 1 Decision, N Episodes, N Bindings, 1 Conversation.
fn claude_happy_path_fixture() -> PathBuf {
    workspace_root().join("fixtures/claude_code/2.0/happy_path_decision_then_edits.jsonl")
}

fn memscribe() -> AssertCommand {
    AssertCommand::cargo_bin("memscribe").expect("the `memscribe` binary builds")
}

/// Parse an NDJSON node stream and tally nodes by their `"node"` variant tag
/// (`conversation` / `decision` / `episode` / `binding`). Every non-blank line
/// must be a JSON object carrying a string `node` field.
fn tally_ndjson_node_kinds(ndjson: &str) -> BTreeMap<String, usize> {
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for line in ndjson.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("watch emitted a non-JSON line {line:?}: {e}"));
        assert!(
            value.is_object(),
            "each NDJSON node must be a JSON object, got: {line:?}"
        );
        let tag = value
            .get("node")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("node line is missing a string `node` tag: {line:?}"));
        *by_kind.entry(tag.to_string()).or_default() += 1;
    }
    by_kind
}

/// Assert the §8.2 happy-path shape: at least one Conversation, one Decision,
/// and one Episode were prepared. (Bindings follow Episodes but we don't gate
/// on them so the assertion stays robust to scenario tweaks.)
fn assert_happy_path_shape(by_kind: &BTreeMap<String, usize>) {
    let conv = by_kind.get("conversation").copied().unwrap_or(0);
    let dec = by_kind.get("decision").copied().unwrap_or(0);
    let epi = by_kind.get("episode").copied().unwrap_or(0);
    assert!(
        conv >= 1,
        "expected >=1 conversation node, got {conv} (tally: {by_kind:?})"
    );
    assert!(
        dec >= 1,
        "expected >=1 decision node, got {dec} (tally: {by_kind:?})"
    );
    assert!(
        epi >= 1,
        "expected >=1 episode node, got {epi} (tally: {by_kind:?})"
    );
}

/// Seed a tempdir with the claude happy-path fixture under a name whose path
/// carries `claude` (so the tool is inferable) and return `(tempdir, root)`.
fn seed_claude_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let transcript = root.join("claude-session.jsonl");
    std::fs::copy(claude_happy_path_fixture(), &transcript).expect("copy fixture into temp root");
    (dir, root)
}

#[test]
fn watch_once_ndjson_to_file_emits_expected_node_kinds() {
    // The load-bearing proof: discover the seeded transcript under --root, run it
    // through DefaultPipeline, and write prepared nodes to an --out file. Then
    // parse that file and assert the §8.2 happy-path node shape.
    let (_dir, root) = seed_claude_root();
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out = out_dir.path().join("nodes.ndjson");

    memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--tools")
        .arg("claude_code")
        .arg("--root")
        .arg(&root)
        .arg("--sink")
        .arg("ndjson")
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    let written = std::fs::read_to_string(&out)
        .unwrap_or_else(|e| panic!("watch should have written {}: {e}", out.display()));
    let by_kind = tally_ndjson_node_kinds(&written);
    assert_happy_path_shape(&by_kind);
}

#[test]
fn watch_once_ndjson_to_stdout_emits_expected_node_kinds() {
    // The default sink (`ndjson` → stdout, `--out -`). Proves the same
    // discover → pipeline → sink path lands nodes on stdout.
    let (_dir, root) = seed_claude_root();

    let assert = memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--tools")
        .arg("claude_code")
        .arg("--root")
        .arg(&root)
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout is UTF-8");
    let by_kind = tally_ndjson_node_kinds(&stdout);
    assert_happy_path_shape(&by_kind);
}

#[test]
fn watch_once_sqlite_sink_persists_node_rows() {
    // `--sink sqlite --out <db>`: the prepared nodes must land as rows in the
    // sink's `nodes` table. We read the resulting database directly and count by
    // `node_type` to prove the sqlite path is wired end-to-end (not just that a
    // file was created).
    let (_dir, root) = seed_claude_root();
    let db_dir = tempfile::tempdir().expect("db tempdir");
    let db = db_dir.path().join("nodes.sqlite");

    memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--tools")
        .arg("claude_code")
        .arg("--root")
        .arg(&root)
        .arg("--sink")
        .arg("sqlite")
        .arg("--out")
        .arg(&db)
        .assert()
        .success();

    assert!(
        db.exists(),
        "sqlite sink must create the db at {}",
        db.display()
    );

    let conn = rusqlite::Connection::open(&db).expect("open the sink db");
    let count_of = |node_type: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE node_type = ?1",
            rusqlite::params![node_type],
            |r| r.get(0),
        )
        .expect("count query")
    };
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
        .expect("total query");

    assert!(total >= 3, "expected >=3 node rows, got {total}");
    assert!(count_of("conversation") >= 1, "expected a conversation row");
    assert!(count_of("decision") >= 1, "expected a decision row");
    assert!(count_of("episode") >= 1, "expected an episode row");
}

#[test]
fn watch_once_empty_root_emits_nothing_and_exits_zero() {
    // An empty root discovers no transcripts: stdout must be empty and the
    // process must still exit 0 (the "nothing to do" floor, not an error).
    let empty = tempfile::tempdir().expect("empty tempdir");

    let assert = memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--tools")
        .arg("claude_code")
        .arg("--root")
        .arg(empty.path())
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("stdout is UTF-8");
    assert!(
        stdout.trim().is_empty(),
        "an empty root must emit no nodes, got: {stdout:?}"
    );
}

#[test]
fn watch_once_empty_root_to_file_writes_empty_output() {
    // Same floor but with a file sink: the out file may be created but must hold
    // no node lines, and the process exits 0.
    let empty = tempfile::tempdir().expect("empty tempdir");
    let out_dir = tempfile::tempdir().expect("out tempdir");
    let out = out_dir.path().join("nodes.ndjson");

    memscribe()
        .arg("watch")
        .arg("--once")
        .arg("--tools")
        .arg("claude_code")
        .arg("--root")
        .arg(empty.path())
        .arg("--sink")
        .arg("ndjson")
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    // The file may or may not exist (a stream that never writes can be empty);
    // either way it must contain zero node lines.
    let written = std::fs::read_to_string(&out).unwrap_or_default();
    let by_kind = tally_ndjson_node_kinds(&written);
    assert!(
        by_kind.is_empty(),
        "an empty root must produce no nodes, got: {by_kind:?}"
    );
}

/// Live mode (no `--once`) tails a growing transcript and emits nodes as it
/// grows, exiting cleanly on Ctrl-C. This drives the *real* notify-backed
/// `LiveTailer` path end-to-end. We:
///   1. seed a transcript so `poll_existing` has content to drain immediately,
///   2. spawn `watch` (no `--once`) with `HOME` pointed at a tempdir so the
///      persistent cursor store stays hermetic,
///   3. let it run briefly, then send SIGINT and assert it shut down cleanly and
///      drained the pre-existing content as nodes.
///
/// Unix-only: it relies on POSIX signals to deliver Ctrl-C to the child.
#[cfg(unix)]
#[test]
fn watch_live_drains_then_shuts_down_on_ctrl_c() {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let (_dir, root) = seed_claude_root();
    // A hermetic HOME so the cursor store (`$HOME/.local/state/memscribe`) never
    // touches the developer's real state dir.
    let home = tempfile::tempdir().expect("home tempdir");

    let bin = assert_cmd::cargo::cargo_bin("memscribe");
    let mut child = Command::new(bin)
        .arg("watch")
        .arg("--tools")
        .arg("claude_code")
        .arg("--root")
        .arg(&root)
        // ndjson → stdout so we can read the drained nodes back.
        .env("HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn memscribe watch (live)");

    let pid = child.id() as i32;

    // Give the tailer time to register the watch and drain pre-existing content
    // via `poll_existing`, then ask it to stop with SIGINT (Ctrl-C).
    std::thread::sleep(Duration::from_millis(1500));
    // SAFETY-free libc-less SIGINT: shell out to `kill -INT` (POSIX, always
    // present on the unix test hosts) rather than pulling in a libc dependency.
    let _ = Command::new("kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status();

    // Wait for a clean exit, bounded so a wedged child fails loud rather than
    // hanging the suite.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("live `watch` did not exit within 10s after SIGINT");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    assert!(
        status.success(),
        "live `watch` must exit cleanly (0) on Ctrl-C, got {status:?}"
    );

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_string(&mut stdout)
        .expect("read child stdout");

    // The pre-existing content must have been drained as the §8.2 node shape.
    let by_kind = tally_ndjson_node_kinds(&stdout);
    assert_happy_path_shape(&by_kind);
}
