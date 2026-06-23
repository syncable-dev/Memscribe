//! Redaction & privacy tests (whitepaper §8.6) and the cross-version corpus
//! (whitepaper §8.7).
//!
//! ## §8.6 — redaction
//! Drive the **full** [`DefaultPipeline::new`] (redaction **on** by default) over
//! transcripts that carry real secret shapes — an Anthropic key, an AWS access
//! key, a GitHub token, a bearer token, a PEM private-key block, an `API_KEY=`
//! assignment, and `.env`-style lines — inside both *user-turn text* and *diffs*.
//! The guarantee under test: **no secret substring survives in any emitted node's
//! serialized JSON**. A second test proves that a no-content [`Redactor`] elides
//! all verbatim text while leaving the node *structure* (kinds and counts)
//! unchanged.
//!
//! ## §8.7 — cross-version tolerance
//! For the three primary tools we add a *second* fixture version with a slightly
//! different record shape (Claude Code 2.1 with string `content`, a legacy Codex
//! `v1` pre-rollout shape, a Gemini `legacy_json` `$set`/`$rewindTo` corpus) and
//! assert the adapter still parses it *losslessly* — every non-blank record maps
//! to at least one event, nothing panics, and unrecognized records/fields route
//! to [`EventKind::Unknown`] rather than failing.

use memscribe_core::{
    CaptureEvent, DefaultPipeline, EventKind, PreparedNode, Redactor, SourceKind,
};
use memscribe_testkit::golden::fixtures_dir;
use memscribe_testkit::{count_input_records, parse_events};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Shared secret catalog
// ---------------------------------------------------------------------------

/// The verbatim secret substrings that must never survive the pipeline. Each is
/// matched by a default [`Redactor`] pattern; the tests assert none of these
/// appears in any emitted node's JSON after redaction.
const SECRETS: &[&str] = &[
    // Anthropic API key (`sk-ant-…`).
    "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA",
    // AWS access key id (`AKIA` + 16).
    "AKIAIOSFODNN7EXAMPLE",
    // GitHub personal access token (`ghp_…`).
    "ghp_1234567890abcdefghijklmnopqrstuvwx",
    // A bearer token value.
    "abcdefghijklmnop1234567890",
    // The PEM private-key body line (must be elided with the block).
    "MIIEowIBAAKCAQEAsupersecretkeymaterial",
    // `API_KEY=` assignment value (in a diff).
    "supersecretapikeyvalue123",
    // `.env`-style assignment values.
    "topsecretenvvalue",
    "hunter2envpassword",
];

/// A PEM private-key block embedded in a diff. The whole block (BEGIN…END) must
/// be elided, including the body line tracked in [`SECRETS`].
const PRIVATE_KEY_BLOCK: &str = "-----BEGIN RSA PRIVATE KEY-----\\nMIIEowIBAAKCAQEAsupersecretkeymaterial\\nB2hY9kRdeadbeefcafef00d\\n-----END RSA PRIVATE KEY-----";

/// A Claude Code transcript whose **user-turn text** and **diffs** both carry
/// secrets. The first user turn is a decision ("use … instead of …") so it is
/// elevated; the second is a memory directive ("remember that …"). Two assistant
/// edits carry a PEM block and an `API_KEY=` assignment in their diffs.
fn secret_bearing_claude_transcript() -> Vec<u8> {
    // Decision turn — secrets in user text (Anthropic key + bearer token).
    let decision = r#"{"type":"user","uuid":"r1","parentUuid":null,"timestamp":"2026-06-22T10:00:00.000Z","sessionId":"sess-redact-001","cwd":"/repo","gitBranch":"main","version":"2.0.5","message":{"role":"user","content":"Let's use the Anthropic API instead of OpenAI. My key is sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA and the call uses Authorization: Bearer abcdefghijklmnop1234567890."}}"#;
    // Memory directive turn — AWS key + GitHub token + .env assignments.
    let memory = r#"{"type":"user","uuid":"r2","parentUuid":"r1","timestamp":"2026-06-22T10:00:01.000Z","sessionId":"sess-redact-001","version":"2.0.5","message":{"role":"user","content":"Remember that the deploy creds are AKIAIOSFODNN7EXAMPLE and ghp_1234567890abcdefghijklmnopqrstuvwx. The .env has SECRET=topsecretenvvalue and password: hunter2envpassword."}}"#;
    // Edit #1 — a PEM private-key block lands in the diff (old/new/unified). The
    // `{pk}` interpolation puts the BEGIN…END block into `newString`.
    let edit_pk = format!(
        r#"{{"type":"assistant","uuid":"r3","parentUuid":"r2","timestamp":"2026-06-22T10:00:02.000Z","sessionId":"sess-redact-001","version":"2.0.5","message":{{"role":"assistant","model":"claude-opus-4-8","content":[{{"type":"tool_use","id":"call_pk","name":"Write","input":{{"file_path":"/repo/key.pem"}}}}]}},"toolUseResult":{{"filePath":"/repo/key.pem","oldString":"","newString":"{PRIVATE_KEY_BLOCK}","structuredPatch":[{{"oldStart":1,"oldLines":0,"newStart":1,"newLines":4,"lines":["+-----BEGIN RSA PRIVATE KEY-----","+MIIEowIBAAKCAQEAsupersecretkeymaterial","+B2hY9kRdeadbeefcafef00d","+-----END RSA PRIVATE KEY-----"]}}]}}}}"#
    );
    // Edit #2 — an API_KEY= assignment lands in the diff.
    let edit_cfg = r#"{"type":"assistant","uuid":"r4","parentUuid":"r3","timestamp":"2026-06-22T10:00:03.000Z","sessionId":"sess-redact-001","version":"2.0.5","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"tool_use","id":"call_cfg","name":"Edit","input":{"file_path":"/repo/config.rs"}}]},"toolUseResult":{"filePath":"/repo/config.rs","oldString":"const API_KEY=PLACEHOLDER","newString":"const API_KEY=supersecretapikeyvalue123","structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-const API_KEY=PLACEHOLDER","+const API_KEY=supersecretapikeyvalue123"]}]}}"#;

    [decision, memory, &edit_pk, edit_cfg]
        .join("\n")
        .into_bytes()
}

/// Serialize a node stream to one JSON blob for substring scanning.
fn nodes_to_json(nodes: &[PreparedNode]) -> String {
    serde_json::to_string(nodes).expect("nodes serialize")
}

// ---------------------------------------------------------------------------
// §8.6 — redaction strips every secret from emitted node JSON
// ---------------------------------------------------------------------------

#[test]
fn full_pipeline_redacts_every_secret_from_node_json() {
    let bytes = secret_bearing_claude_transcript();
    let path = Path::new("sess-redact-001.jsonl");
    let events = parse_events(SourceKind::ClaudeCode, &bytes, path);

    // Grounding: without redaction, the secrets DO reach emitted nodes. If a
    // secret fails to appear here, the fixture is not exercising the pass and
    // the redaction assertion below would be vacuous — so we fail loudly first.
    let plain = DefaultPipeline::without_redaction().prepare_events(&events);
    assert!(!plain.is_empty(), "fixture must produce nodes");
    let plain_json = nodes_to_json(&plain);
    for secret in SECRETS {
        assert!(
            plain_json.contains(secret),
            "precondition: secret {secret:?} must reach a node BEFORE redaction \
             (otherwise the redaction assertion is vacuous)"
        );
    }

    // The real guarantee: the default pipeline has redaction ON, and no secret
    // substring may survive in ANY emitted node's serialized JSON.
    let redacted = DefaultPipeline::new().prepare_events(&events);
    assert_eq!(
        redacted.len(),
        plain.len(),
        "redaction must not add or drop nodes"
    );
    let redacted_json = nodes_to_json(&redacted);
    for secret in SECRETS {
        assert!(
            !redacted_json.contains(secret),
            "secret {secret:?} survived redaction in: {redacted_json}"
        );
    }
    // The redaction marker proves the pass actually fired (not that the text was
    // merely absent for some unrelated reason).
    assert!(
        redacted_json.contains("[REDACTED:"),
        "redaction markers must be present"
    );
}

#[test]
fn redactor_strips_each_pattern_in_isolation() {
    // A direct, per-pattern check against the default redactor, independent of
    // segmenter/gate behavior — locks each individual pattern.
    let r = Redactor::default();
    let cases = [
        "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA",
        "AKIAIOSFODNN7EXAMPLE",
        "ghp_1234567890abcdefghijklmnopqrstuvwx",
        "Authorization: Bearer abcdefghijklmnop1234567890",
        "const API_KEY=supersecretapikeyvalue123",
        "SECRET=topsecretenvvalue",
        "password: hunter2envpassword",
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAsupersecretkeymaterial\n-----END RSA PRIVATE KEY-----",
    ];
    for case in cases {
        assert!(
            r.contains_secret(case),
            "redactor must detect secret in {case:?}"
        );
        let out = r.redact_text(case);
        for secret in SECRETS {
            assert!(
                !out.contains(secret),
                "pattern leaked {secret:?} from {case:?} -> {out:?}"
            );
        }
        assert!(
            out.contains("[REDACTED:"),
            "redaction marker missing for {case:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// §8.6 — no-content mode elides verbatim text but preserves structure
// ---------------------------------------------------------------------------

#[test]
fn no_content_elides_text_but_keeps_node_kinds_and_counts() {
    let bytes = secret_bearing_claude_transcript();
    let path = Path::new("sess-redact-001.jsonl");
    let events = parse_events(SourceKind::ClaudeCode, &bytes, path);

    // Baseline structure (redaction off): node count and the ordered kind list.
    let plain = DefaultPipeline::without_redaction().prepare_events(&events);
    let plain_kinds: Vec<&'static str> = plain.iter().map(PreparedNode::tag).collect();

    // No-content redactor: elides all verbatim text, keeps structure.
    let no_content =
        DefaultPipeline::new().with_redactor(Some(Redactor::with_default_patterns(true)));
    let elided = no_content.prepare_events(&events);
    let elided_kinds: Vec<&'static str> = elided.iter().map(PreparedNode::tag).collect();

    // Structure unchanged: same number of nodes, same kinds in the same order.
    assert_eq!(
        elided.len(),
        plain.len(),
        "no-content must not change node count"
    );
    assert_eq!(
        elided_kinds, plain_kinds,
        "no-content must not change node kinds/order"
    );

    // All verbatim text is elided to the structural placeholder, and NO secret
    // (and indeed no original prose) survives anywhere.
    let elided_json = nodes_to_json(&elided);
    assert!(
        elided_json.contains("[content elided]"),
        "no-content placeholder must be present: {elided_json}"
    );
    for secret in SECRETS {
        assert!(
            !elided_json.contains(secret),
            "no-content leaked secret {secret:?}: {elided_json}"
        );
    }
    // A representative non-secret prose fragment is also gone, proving full
    // elision rather than mere secret-stripping.
    assert!(
        !elided_json.contains("Anthropic API instead of OpenAI"),
        "no-content must elide ALL verbatim text, not just secrets"
    );
}

// ---------------------------------------------------------------------------
// §8.7 — cross-version corpus: version tolerance for the three primary tools
// ---------------------------------------------------------------------------

/// The path to a cross-version fixture under `fixtures/<tool>/<version>/`.
fn version_fixture(tool: &str, version: &str, case: &str) -> PathBuf {
    fixtures_dir()
        .join(tool)
        .join(version)
        .join(format!("{case}.jsonl"))
}

/// Parse a fixture and assert the version-tolerance guarantee:
/// - the file is read and produces events (no panic),
/// - losslessness: at least as many events as non-blank records,
/// - any unrecognized record/field is preserved as [`EventKind::Unknown`],
///   never dropped or errored.
fn assert_version_tolerant(
    tool: SourceKind,
    fixture: &Path,
    expect_unknown: bool,
) -> Vec<CaptureEvent> {
    let bytes = std::fs::read(fixture)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture.display()));
    let events = parse_events(tool, &bytes, fixture);

    let nonblank = count_input_records(tool, &bytes, fixture);
    assert!(
        events.len() >= nonblank,
        "{}: lossy — {} non-blank records produced only {} events",
        fixture.display(),
        nonblank,
        events.len()
    );

    if expect_unknown {
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Unknown { .. })),
            "{}: an unrecognized record must route to Unknown, not be dropped",
            fixture.display()
        );
    }

    // The full pipeline must run over the (possibly novel) shape without panic.
    let nodes = DefaultPipeline::new().prepare_events(&events);
    let _ = nodes_to_json(&nodes);

    events
}

#[test]
fn claude_code_2_1_string_content_parses_losslessly() {
    // 2.1: `content` is a plain string (not blocks), `gitHead` replaces `gitSha`,
    // and a `telemetry` record type is unrecognized → Unknown.
    let fixture = version_fixture("claude_code", "2.1", "version_tolerance");
    let events = assert_version_tolerant(SourceKind::ClaudeCode, &fixture, true);

    // The string-`content` user turn is still a UserTurn with verbatim text.
    let decision = events.iter().find_map(|e| match &e.kind {
        EventKind::UserTurn { text, .. } if text.contains("Postgres instead of MySQL") => {
            Some(text.clone())
        }
        _ => None,
    });
    assert!(
        decision.is_some(),
        "string-content user turn must parse to a UserTurn"
    );

    // The string-`content` ASSISTANT turn (a renamed shape) is still an
    // AssistantTurn carrying its verbatim text.
    let asst = events.iter().find_map(|e| match &e.kind {
        EventKind::AssistantTurn { text, .. } if text.contains("Switching the orders service") => {
            Some(text.clone())
        }
        _ => None,
    });
    assert!(
        asst.is_some(),
        "string-content assistant turn must parse to an AssistantTurn"
    );

    // The unknown `telemetry` record is preserved verbatim, with its raw type.
    let unknown = events.iter().find_map(|e| match &e.kind {
        EventKind::Unknown { raw_type, .. } => Some(raw_type.clone()),
        _ => None,
    });
    assert_eq!(
        unknown.as_deref(),
        Some("telemetry"),
        "the telemetry record must survive as Unknown with its raw type"
    );
}

#[test]
fn codex_v1_pre_rollout_shape_parses_losslessly() {
    // v1: only `session_meta` is recognized; the pre-rollout `record_type` lines
    // and a `kind`-tagged state line have no `type`+`payload` shape → Unknown.
    let fixture = version_fixture("codex", "v1", "version_tolerance");
    let events = assert_version_tolerant(SourceKind::Codex, &fixture, true);

    // The recognized session_meta still opens the session.
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::SessionStart { .. })),
        "session_meta must still produce a SessionStart in the legacy corpus"
    );
    // The legacy `record_type` lines are preserved as Unknown (not dropped).
    let unknown_count = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Unknown { .. }))
        .count();
    assert!(
        unknown_count >= 3,
        "all three non-session_meta legacy records must survive as Unknown, got {unknown_count}"
    );
}

#[test]
fn gemini_legacy_json_control_lines_parse_losslessly() {
    // legacy_json: `$set` opens the session, `content` (not `text`) carries the
    // turn body, `role:system` is an unknown role → Unknown, and `$rewindTo` is
    // a control line → Rewind.
    let fixture = version_fixture("gemini", "legacy_json", "version_tolerance");
    let events = assert_version_tolerant(SourceKind::Gemini, &fixture, true);

    // `$set` with a cwd opens the session.
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::SessionStart { .. })),
        "$set with cwd must open the session"
    );
    // The legacy `content` field is read as the user-turn text.
    let user = events.iter().find_map(|e| match &e.kind {
        EventKind::UserTurn { text, .. } if text.contains("Postgres instead of MySQL") => {
            Some(text.clone())
        }
        _ => None,
    });
    assert!(
        user.is_some(),
        "legacy `content` field must populate the UserTurn text"
    );

    // The `$rewindTo` control line maps to a Rewind, not a dropped record.
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Rewind { .. })),
        "$rewindTo must map to a Rewind event"
    );

    // The unknown `role:system` record is preserved, not dropped.
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Unknown { .. })),
        "an unknown role must survive as Unknown"
    );
}
