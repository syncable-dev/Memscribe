//! Shared, deterministic helpers for adapters. **Do not add tool-specific logic
//! here** — keep that in each tool's module so adapters can be maintained
//! independently. These helpers build normalized events with correct
//! provenance, sequencing, and ids.

use memscribe_core::{
    content_id, CaptureEvent, EventKind, ParseCtx, ParseError, RawRecord, SourceKind, Timestamp,
    SCHEMA_VERSION,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Parse a JSONL line into a JSON value. Returns `None` for blank lines or
/// invalid JSON (the caller decides whether that is an `Unknown` or a skip).
#[must_use]
pub fn parse_json_line(raw: &RawRecord) -> Option<serde_json::Value> {
    let s = raw.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    serde_json::from_str(s).ok()
}

/// Parse a timestamp from RFC3339, or from epoch seconds/milliseconds. Returns
/// `None` if neither parses.
#[must_use]
pub fn parse_ts(s: &str) -> Option<Timestamp> {
    if let Ok(t) = OffsetDateTime::parse(s.trim(), &Rfc3339) {
        return Some(t);
    }
    let n: i64 = s.trim().parse().ok()?;
    // Heuristic: values above ~year 2286 in seconds are really milliseconds.
    let (secs, nanos) = if n.abs() > 10_000_000_000 {
        (n / 1000, (n % 1000) * 1_000_000)
    } else {
        (n, 0)
    };
    OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .map(|t| t + time::Duration::nanoseconds(nanos))
}

/// Extract an RFC3339/epoch timestamp from a JSON object under any of the given
/// keys, falling back to the Unix epoch (so output stays deterministic even when
/// a record carries no timestamp).
#[must_use]
pub fn ts_from(value: &serde_json::Value, keys: &[&str]) -> Timestamp {
    for k in keys {
        if let Some(v) = value.get(*k) {
            if let Some(s) = v.as_str() {
                if let Some(t) = parse_ts(s) {
                    return t;
                }
            } else if let Some(n) = v.as_i64() {
                if let Some(t) = parse_ts(&n.to_string()) {
                    return t;
                }
            }
        }
    }
    OffsetDateTime::UNIX_EPOCH
}

/// The Unix epoch — a stable default timestamp.
#[must_use]
pub fn epoch() -> Timestamp {
    OffsetDateTime::UNIX_EPOCH
}

/// Build a normalized [`CaptureEvent`], allocating the monotonic `seq` from the
/// context and stamping the session/project binding.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn mk_event(
    source: SourceKind,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
    event_id: String,
    parent_id: Option<String>,
    timestamp: Timestamp,
    kind: EventKind,
) -> CaptureEvent {
    let seq = ctx.alloc_seq();
    let session_id = ctx
        .session_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    CaptureEvent {
        schema_version: SCHEMA_VERSION,
        source,
        session_id,
        seq,
        event_id,
        parent_id,
        timestamp,
        project: ctx.project_or_default(),
        kind,
        provenance: raw.location.clone(),
    }
}

/// Build an [`EventKind::Unknown`] event from a raw JSON value — the lossless,
/// version-tolerant fallback every adapter uses for records it does not yet
/// understand.
#[must_use]
pub fn unknown_event(
    source: SourceKind,
    ctx: &mut ParseCtx,
    raw: &RawRecord,
    value: serde_json::Value,
) -> CaptureEvent {
    let raw_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".to_string());
    let timestamp = ts_from(&value, &["timestamp", "time", "ts", "created_at"]);
    let id = content_id(&raw.bytes);
    mk_event(
        source,
        ctx,
        raw,
        id,
        None,
        timestamp,
        EventKind::Unknown {
            raw_type,
            raw: value,
        },
    )
}

/// The default skeleton parse: emit exactly one `Unknown` event per non-blank
/// record (so the stream is lossless even before a real parser exists). Tool
/// modules replace this with real parsing but should preserve the losslessness
/// guarantee for records they do not recognize.
///
/// # Errors
/// Never returns an error — present for signature parity with `parse`.
pub fn stub_parse(
    source: SourceKind,
    raw: &RawRecord,
    ctx: &mut ParseCtx,
) -> Result<Vec<CaptureEvent>, ParseError> {
    let s = raw.as_str().map(str::trim).unwrap_or("");
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let value =
        serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.to_string()));
    Ok(vec![unknown_event(source, ctx, raw, value)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use memscribe_core::SourceLocation;

    fn raw(s: &str) -> RawRecord {
        RawRecord::from_line(s, SourceLocation::new("t.jsonl", 0, 1))
    }

    #[test]
    fn parse_ts_rfc3339_and_epoch() {
        assert!(parse_ts("2026-06-22T10:00:00Z").is_some());
        assert!(parse_ts("1750000000").is_some());
        assert!(parse_ts("1750000000000").is_some());
        assert!(parse_ts("not a time").is_none());
    }

    #[test]
    fn stub_parse_is_lossless_for_nonblank() {
        let mut ctx = ParseCtx::new();
        let evs = stub_parse(SourceKind::Unknown, &raw("{\"type\":\"x\"}"), &mut ctx).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind.tag(), "unknown");
    }

    #[test]
    fn stub_parse_skips_blank() {
        let mut ctx = ParseCtx::new();
        assert!(stub_parse(SourceKind::Unknown, &raw("   "), &mut ctx)
            .unwrap()
            .is_empty());
    }
}
