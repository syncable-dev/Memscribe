//! Reusable invariant checks (whitepaper §8.3). These are written as `Result`
//! returns so they can be used both in `#[test]`s and in `proptest` bodies.

use memscribe_core::CaptureEvent;
use std::collections::HashMap;

/// `seq` is strictly increasing within a session and matches file order.
///
/// # Errors
/// Returns a message describing the first violation.
pub fn check_monotonic_seq(events: &[CaptureEvent]) -> Result<(), String> {
    let mut last: HashMap<&str, u64> = HashMap::new();
    for ev in events {
        if let Some(prev) = last.get(ev.session_id.as_str()) {
            if ev.seq <= *prev {
                return Err(format!(
                    "seq not strictly increasing in session {}: {} after {}",
                    ev.session_id, ev.seq, prev
                ));
            }
        }
        last.insert(ev.session_id.as_str(), ev.seq);
    }
    Ok(())
}

/// Losslessness: every non-blank source record maps to at least one event.
///
/// # Errors
/// Returns a message if fewer events than records were produced.
pub fn check_lossless(nonblank_record_count: usize, events: &[CaptureEvent]) -> Result<(), String> {
    if events.len() < nonblank_record_count {
        return Err(format!(
            "lossy: {} records produced only {} events",
            nonblank_record_count,
            events.len()
        ));
    }
    Ok(())
}

/// Idempotency by `event_id`: re-ingesting the same input yields the same set of
/// `(session_id, event_id)` keys with no duplicates introduced.
///
/// # Errors
/// Returns a message if duplicate dedup keys are present.
pub fn check_unique_event_ids(events: &[CaptureEvent]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for ev in events {
        let key = (ev.session_id.clone(), ev.event_id.clone());
        if !seen.insert(key) {
            return Err(format!(
                "duplicate event_id {} in session {}",
                ev.event_id, ev.session_id
            ));
        }
    }
    Ok(())
}

/// Determinism: two parses of the same input are byte-identical (serialized).
///
/// # Errors
/// Returns a message if the two event vectors differ.
pub fn check_determinism(a: &[CaptureEvent], b: &[CaptureEvent]) -> Result<(), String> {
    let ja = serde_json::to_string(a).map_err(|e| e.to_string())?;
    let jb = serde_json::to_string(b).map_err(|e| e.to_string())?;
    if ja != jb {
        return Err("parse is not deterministic across runs".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_events;
    use memscribe_core::SourceKind;
    use std::path::Path;

    #[test]
    fn stub_stream_satisfies_invariants() {
        let jsonl = b"{\"type\":\"a\"}\n{\"type\":\"b\"}\n";
        let events = parse_events(SourceKind::ClaudeCode, jsonl, Path::new("t.jsonl"));
        check_monotonic_seq(&events).unwrap();
        check_lossless(2, &events).unwrap();
        let again = parse_events(SourceKind::ClaudeCode, jsonl, Path::new("t.jsonl"));
        check_determinism(&events, &again).unwrap();
    }
}
