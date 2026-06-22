# Security & Privacy

Memscribe reads files that contain prompts, source code, and potentially
secrets (API keys, tokens, `.env` contents). It is designed to be safe for
security-conscious teams to run:

- **Local-only, no network in the core path.** The Source → Adapter → Gate →
  Segmenter → Binder → NodePrep → Sink pipeline performs no network I/O. The
  optional OTLP receiver binds to loopback only. The optional MemDB sink is the
  only component that talks to another process, and it is feature-gated off by
  default.
- **Redaction on by default.** Known secret patterns (API keys, bearer tokens,
  `.env` assignments, private-key blocks) are stripped before the Sink. See
  `memscribe redact <file>` to preview exactly what would be removed.
- **`--no-content` mode.** Stores structure only (event kinds, spans, diffs
  stats) with all verbatim text elided — for the most sensitive environments.
- **Honors tool suppression switches.** When a tool exposes a privacy switch
  (e.g. `CLAUDE_CODE_SKIP_PROMPT_HISTORY`), Memscribe respects it and does not
  capture the suppressed content.
- **Auditable.** Because the default sink is NDJSON and every node carries a
  `SourceLocation` provenance pointer, you can audit exactly what was captured
  and trace any node back to the byte range it came from.

## Reporting a vulnerability

Please report security issues privately to the maintainers via the repository's
security advisory channel rather than a public issue.
