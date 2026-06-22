# Memscribe

**A self-contained, open-source Rust workspace for deterministic, zero-LLM
conversation capture from AI coding agents.**

Memscribe tails the transcript logs that AI coding agents — Claude Code, Codex
CLI, Gemini CLI, Cursor, Windsurf, Zed, VS Code / Copilot, and any
OpenTelemetry-instrumented agent — already write, and prepares that raw data
into typed nodes that an inference-and-governance layer (MemCortex / Memtrace)
can consume. It is **deterministic and zero-LLM by construction**: capture is
reading and parsing, never summarizing. The output is an exact function of the
input, which is what makes the whole module golden-file, property, and fuzz
testable.

Dual-licensed under **MIT OR Apache-2.0**.

---

## Why a standalone module

MemCortex is the inference-and-governance layer; Memscribe is the deterministic
data layer beneath it. Separating them changes what is testable and who can
contribute:

- **Determinism is a property you can test.** Memscribe never calls a model, so
  the same input bytes always produce the same output nodes. Golden-file,
  snapshot, and property tests are *meaningful*.
- **A small, sharp boundary.** Memscribe depends on *nothing* from Memtrace or
  MemDB. The dependency is one-way; the contract is a single stable data type
  (the prepared-node stream).
- **The adapters are the volatile part.** Every tool's transcript format churns.
  An open repo lets the community version adapters as the tools move.

## Architecture

A single, linear, deterministic pipeline. Each stage is a trait, so it can be
tested in isolation and swapped.

```
 Source (memscribe-io)         Adapter (memscribe-adapters)
 tail JSONL / hook stdin   →   parse_line → CaptureEvent[]
 / OTLP receiver               (version-tolerant)
        │ RawRecord(bytes + provenance)        │ normalized events
        ▼                                      ▼
 Gate → Segmenter → Binder → NodePrep  →  Sink (memscribe-sink)
 admit?  arc/turn   decision   Prepared    MemDB / ndjson / sqlite
 markers  spans     ↔ edit     Node
```

`Source → Adapter` produces a normalized `CaptureEvent` stream (the system of
record). `Gate → Segmenter → Binder → NodePrep` transform that into
`PreparedNode`s. The `Sink` writes them. Everything between Source and Sink is
pure and synchronous given the event stream — which is what makes the whole
thing golden-testable end to end.

### Crates

| Crate | Responsibility |
|-------|----------------|
| `memscribe-core` | The contract: model, traits, pipeline, gate, segmenter, binder, node-prep, redaction. Depends on nothing in the workspace. |
| `memscribe-adapters` | Per-tool parsers behind feature flags (`claude_code`, `codex`, `gemini`, `otel`, `cursor`, `windsurf`, `zed`, `vscode`, `copilot`). |
| `memscribe-io` | Generic sources: notify-based file tailer (offset resume), hook server, OTLP receiver. |
| `memscribe-sink` | `Sink` implementations: NDJSON, SQLite, stdout, and a feature-gated MemDB sink. |
| `memscribe-cli` | The `memscribe` binary: `watch` / `hook` / `parse` / `replay` / `verify` / `redact`. |
| `memscribe-testkit` | Golden-file harness, fixture loaders, synthetic generators, conformance suite. |

## Usable **with** MemDB — and fully usable **without** it

This was a first-class design requirement. The seam is the `Sink` trait
(defined in `memscribe-core`):

```rust
pub trait Sink: Send {
    fn emit(&mut self, node: &PreparedNode) -> Result<(), SinkError>;
    fn flush(&mut self) -> Result<(), SinkError>;
}
```

Nothing in the pipeline knows what a Sink does with a node. That single seam is
what decouples Memscribe from MemDB entirely:

- **Without MemDB (the default).** The canonical sink is `NdjsonSink`, which
  writes one JSON node per line to stdout or a file. `SqliteSink` gives you a
  queryable local store with zero external services. The entire module is
  observable and testable with no MemDB present:

  ```console
  $ memscribe parse session.jsonl --as claude_code        # → NDJSON on stdout
  $ memscribe watch --tools claude,codex,gemini --sink sqlite
  ```

  This is the mode every test, every fixture, and every CI gate runs in.

- **With MemDB (opt-in, feature-gated).** Building `memscribe-sink` with
  `--features memdb` enables `MemDbSink`, which writes `PreparedNode`s into
  MemDB with their bi-temporal headers (`valid_at` = the turn/episode time,
  `transaction_at` = ingest time, `episode_id` = the arc/episode). Memtrace
  consumes Memscribe as a git submodule and turns this feature on. The crate
  follows semver and the event schema carries `schema_version`, so a consumer
  can refuse or adapt to an incompatible schema. **Memscribe never depends on
  MemDB; MemDB-the-consumer depends on Memscribe.** The dependency is one-way.

In short: MemDB is one `Sink` implementation behind one feature flag. Remove it
and Memscribe is a complete, auditable, local capture tool.

## CLI

```
memscribe watch  [--tools claude,codex,gemini] [--sink memdb|ndjson|sqlite] [--config m.toml]
memscribe hook                     # hook handler agents invoke (reads stdin, records, returns 0)
memscribe parse  <file> [--as claude_code]   # one-shot parse a transcript to NDJSON
memscribe replay <session-id|file>           # re-run preparation over a historical session
memscribe verify [--capture]                 # run the conformance suite; --capture snapshots a live session
memscribe redact <file>                      # show what the redaction pass would strip
```

## Testing

Because the pipeline is deterministic and zero-LLM, it is tested the way a
compiler is: fixtures in, exact expected output. See the whitepaper §8.

- **Golden-file / snapshot** tests per tool, per version, per scenario (`insta`).
- **Cross-tool conformance** — the same canonical scenarios captured from every
  tool must normalize to the same shape.
- **Property tests** (`proptest`): determinism, idempotency, monotonic seq,
  losslessness, gate purity, offset resumption.
- **Fuzzing** (`cargo-fuzz`): one target per adapter parser; never panic, never
  loop, skip-and-continue on malformed input.
- **Replay & crash**, **redaction & privacy**, and a **cross-version corpus**.

```console
$ cargo test                       # unit + golden + conformance + proptest
$ cargo clippy --all-targets -- -D warnings
$ cargo deny check
```

## Status

Initial working model. See [`CHANGELOG.md`](./CHANGELOG.md) and the roadmap
(M1–M6) in the implementation whitepaper.
