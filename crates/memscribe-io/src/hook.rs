//! The hook handler (whitepaper §7).
//!
//! A `memscribe hook` subcommand registered as the tools' hook handler reads
//! event JSON on stdin, records it, and — critically — captures the
//! `transcript_path` and the live edit event so the Binder can write a PROV
//! `wasGeneratedBy` record at the moment of the edit. It exits 0 immediately,
//! never blocks the agent, and never invokes a model.

use memscribe_core::{RawRecord, SourceLocation};
use serde::Deserialize;

/// The common shape of a tool hook payload delivered on stdin. Each tool's
/// schema differs, so all fields are optional and unknown fields are retained.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookPayload {
    /// The tool-native session id, if present.
    pub session_id: Option<String>,
    /// The path to the live transcript the hook fired for.
    pub transcript_path: Option<String>,
    /// The hook event name (e.g. `PostToolUse`, `UserPromptSubmit`).
    pub hook_event_name: Option<String>,
    /// The working directory, if present.
    pub cwd: Option<String>,
    /// Any remaining fields, preserved verbatim.
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

/// Parse a hook payload from stdin bytes. Returns `None` if the bytes are not
/// valid JSON (the handler still exits 0 — it never blocks the agent).
#[must_use]
pub fn parse_hook_payload(bytes: &[u8]) -> Option<HookPayload> {
    serde_json::from_slice(bytes).ok()
}

/// Wrap raw hook bytes as a [`RawRecord`] with a synthetic hook provenance, so
/// the same parsing path can consume it.
#[must_use]
pub fn hook_record(bytes: &[u8]) -> RawRecord {
    RawRecord::new(bytes.to_vec(), SourceLocation::new("<hook stdin>", 0, 1))
}

/// The outcome of [`record_hook`]: the parsed payload, the transcript path it
/// captured (if the payload carried one), and a synthetic [`RawRecord`] a caller
/// can hand straight to the Binder to record a live edit.
#[derive(Debug, Clone)]
pub struct RecordedHook {
    /// The parsed hook payload (common fields + preserved `rest`).
    pub payload: HookPayload,
    /// The live transcript path the hook fired for, if the payload carried one.
    pub transcript_path: Option<String>,
    /// A synthetic record wrapping the hook bytes, located at the transcript
    /// path when known (so PROV provenance points at the real file) and at
    /// `<hook stdin>` otherwise.
    pub record: RawRecord,
}

/// Parse hook stdin bytes, capture the `transcript_path`, and build a synthetic
/// [`RawRecord`] so a caller can feed the Binder a live edit at the moment the
/// hook fired.
///
/// Returns `None` only when the bytes are not valid JSON — mirroring
/// [`parse_hook_payload`], the handler still exits 0 and never blocks the agent.
/// When a `transcript_path` is present the synthetic record's provenance points
/// at that file (offset/line `0`/`1`, since the exact in-file location is not
/// yet known at hook time); otherwise it falls back to `<hook stdin>`.
#[must_use]
pub fn record_hook(bytes: &[u8]) -> Option<RecordedHook> {
    let payload = parse_hook_payload(bytes)?;
    let transcript_path = payload.transcript_path.clone();
    let location = match transcript_path.as_deref() {
        Some(path) => SourceLocation::new(path, 0, 1),
        None => SourceLocation::new("<hook stdin>", 0, 1),
    };
    let record = RawRecord::new(bytes.to_vec(), location);
    Some(RecordedHook {
        payload,
        transcript_path,
        record,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_fields_and_keeps_rest() {
        let payload = br#"{"session_id":"abc","transcript_path":"/t.jsonl","hook_event_name":"PostToolUse","tool_name":"Edit"}"#;
        let p = parse_hook_payload(payload).unwrap();
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert_eq!(p.hook_event_name.as_deref(), Some("PostToolUse"));
        assert!(p.rest.contains_key("tool_name"));
    }

    #[test]
    fn invalid_json_is_none_not_panic() {
        assert!(parse_hook_payload(b"not json").is_none());
    }

    #[test]
    fn record_hook_captures_transcript_path_and_locates_record_there() {
        let payload = br#"{"session_id":"s1","transcript_path":"/home/u/.codex/sessions/x.jsonl","hook_event_name":"PostToolUse","tool_name":"Edit"}"#;
        let rec = record_hook(payload).expect("valid json");
        assert_eq!(
            rec.transcript_path.as_deref(),
            Some("/home/u/.codex/sessions/x.jsonl")
        );
        // The synthetic record carries the raw bytes verbatim...
        assert_eq!(rec.record.bytes, payload.to_vec());
        // ...and its provenance points at the captured transcript, not stdin.
        assert_eq!(
            rec.record.location.file,
            std::path::PathBuf::from("/home/u/.codex/sessions/x.jsonl")
        );
        assert_eq!(rec.record.location.line_no, 1);
        assert_eq!(rec.payload.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn record_hook_without_transcript_falls_back_to_stdin() {
        let payload = br#"{"hook_event_name":"UserPromptSubmit"}"#;
        let rec = record_hook(payload).expect("valid json");
        assert!(rec.transcript_path.is_none());
        assert_eq!(
            rec.record.location.file,
            std::path::PathBuf::from("<hook stdin>")
        );
    }

    #[test]
    fn record_hook_invalid_json_is_none_not_panic() {
        assert!(record_hook(b"<<<not json>>>").is_none());
    }
}
