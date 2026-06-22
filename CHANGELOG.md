# Changelog

All notable changes to Memscribe are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/). The event schema additionally
carries its own `schema_version` so the consumer layer (MemCortex) can refuse
or adapt to an incompatible event schema independently of the crate version.

## [Unreleased]

### Added
- **M1 — Core contract.** The frozen thin-waist: `CaptureEvent` / `EventKind`
  normalized event model, `PreparedNode` output contract with `FactStatus`,
  the `TranscriptAdapter` and `Sink` traits, and the deterministic pipeline
  (Gate → Segmenter → Binder → NodePrep).
- **Adapters.** Claude Code, Codex CLI, Gemini CLI, OTel GenAI, plus
  VS Code / Copilot / Cursor / Windsurf / Zed, each version-tolerant and
  routing unknowns to `EventKind::Unknown`.
- **Sinks.** NDJSON (canonical default), SQLite, and a feature-gated MemDB sink.
- **IO sources.** notify-based file tailer with persisted byte-offset resume,
  hook server, and an optional OTLP receiver.
- **CLI.** `watch`, `hook`, `parse`, `replay`, `verify`, `redact`.
- **Testkit.** Golden-file harness, cross-tool conformance suite, synthetic
  generators, property tests, and fuzz targets.
