<div align="center">

<img src="assets/memscribe-logo.svg" alt="Memscribe" width="132" height="132" />

# Memscribe

**Deterministic, zero-LLM conversation capture for AI coding agents.**

Memscribe tails the transcript logs your AI coding agents already write — Claude Code, Codex, Gemini, Cursor, Windsurf, Zed, VS Code / Copilot, and any OpenTelemetry-instrumented agent — and prepares them into typed, queryable nodes. No model calls. Same bytes in, same nodes out, every time.

[![CI](https://github.com/syncable-dev/Memscribe/actions/workflows/ci.yml/badge.svg)](https://github.com/syncable-dev/Memscribe/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.96+](https://img.shields.io/badge/rust-1.96%2B-orange.svg)](rust-toolchain.toml)
[![Tests](https://img.shields.io/badge/tests-260%20passing-success.svg)](#testing)
[![Zero-LLM](https://img.shields.io/badge/LLM%20calls-0-1a1a2e.svg)](#why-deterministic-matters)

</div>

---

## What it is

A coding agent is a stream of decisions and edits — *"use Postgres instead of MySQL,"* followed by the diffs that implement it. That stream is gold for memory, audit, analytics, and replay, but it's buried in five different churning log formats. **Memscribe is the boring, deterministic half of a memory system:** it reads those logs, normalizes them, and emits typed nodes — and because it never calls a model, its output is an exact function of its input.

That single property is the whole point. It makes capture **golden-file, property, and fuzz testable**, so the day a tool changes its format, the test suite fails loudly instead of silently corrupting your memory.

### Where Memscribe sits

Memscribe is the foundation of a three-layer stack. Each layer uses the one below it, and the dependency only ever points **downward**:

| Layer | Role | Calls a model? |
|:------|:-----|:--------------:|
| [**Memtrace**](https://github.com/syncable-dev/memtrace) | The product — a code-intelligence graph with agent memory | — |
| **MemCortex** | Inference & governance — the judgment calls on top of the captured data | yes |
| **Memscribe** | Deterministic capture — normalizes transcripts into typed nodes *(this repo)* | **no** |

Memtrace builds on MemCortex; MemCortex builds on Memscribe. Because Memscribe sits at the bottom, depends on nothing above it, and never calls a model, the boundary between the layers is a single stable data type — which is exactly what keeps this layer small, auditable, and exhaustively testable.

## The pipeline

One linear, deterministic pipeline. Every stage is a trait, so it can be tested in isolation and swapped.

```
  Source (memscribe-io)          Adapter (memscribe-adapters)
  tail JSONL / hook stdin   ─►   parse_line ─► CaptureEvent[]
  / OTLP receiver                (version-tolerant)
         │  RawRecord(bytes + provenance)        │  normalized events
         ▼                                       ▼
  Gate ─► Segmenter ─► Binder ─► NodePrep   ─►   Sink (memscribe-sink)
  admit?   arc / turn  decision   Prepared        MemDB · ndjson · sqlite
  markers  spans       ↔ edit     Node
```

`Source → Adapter` produces a normalized `CaptureEvent` stream — the system of record. `Gate → Segmenter → Binder → NodePrep` turn that into `PreparedNode`s. The `Sink` writes them. Everything between Source and Sink is pure and synchronous given the event stream, which is what makes the whole thing golden-testable end to end.

## Quick start

```bash
# Parse a transcript to NDJSON (the workhorse — great for trying it out)
cargo run -p memscribe-cli -- parse ~/.claude/projects/<slug>/<session>.jsonl --as claude_code

# Tail your agents live and write prepared nodes to a local SQLite store
cargo run -p memscribe-cli -- watch --tools claude,codex,gemini --sink sqlite --out memory.db

# See exactly what the redaction pass would strip from a file
cargo run -p memscribe-cli -- redact session.jsonl
```

Every tool's transcript normalizes to the **same shape**. Here a Claude Code decision-and-edits session becomes four kinds of node:

```jsonc
// memscribe parse fixtures/claude_code/2.0/happy_path_decision_then_edits.jsonl --as claude_code
{"node":"conversation","text":"Let's use Postgres instead of MySQL for the orders service.",
 "markers":[{"rule_id":"decision_verb.use",...},{"rule_id":"rejection.instead_of",...}], "fact_status":"observed"}
{"node":"decision","epitome":"Let's use Postgres instead of MySQL ...",
 "considered_options":[{"text":"MySQL","chosen":false},{"text":"Postgres","chosen":true}],"is_ban":false}
{"node":"episode","path":"src/db/config.rs","diff":{"added_lines":1,"removed_lines":1,...}}
{"node":"binding","relation":"produced","prov":{"t_use":"...10:00:00Z","t_gen":"...10:00:03Z"},
 "fact_status":"deterministically_derived","correlation":{...}}
```

## Supported tools

Nine version-tolerant adapters, each behind a Cargo feature flag. Parsers pattern-match the fields they need and route anything unrecognized to `Unknown` — they never panic and never drop a record.

| Tool | Transcript source | Status |
|:-----|:------------------|:-------|
| **Claude Code** | `~/.claude/projects/<slug>/<session>.jsonl` (append-only JSONL, DAG via `parentUuid`) | ✅ native |
| **Codex CLI** | `~/.codex/sessions/.../rollout-*.jsonl[.zst]` (`apply_patch` V4A diffs, transparent zstd) | ✅ native |
| **Gemini CLI** | `~/.gemini/tmp/<hash>/chats/session-*.jsonl` (`$set` / `$rewindTo` control lines) | ✅ native |
| **OpenTelemetry** | OTLP / GenAI semconv records — the universal fallback for any instrumented agent | ✅ native |
| **Cursor** · **Windsurf** · **Zed** · **VS Code** · **Copilot** | exported chat JSON (desktop stores are SQLite/undocumented — export-based, per the whitepaper) | ✅ export-shape |

All five **CLI/OTel** scenarios and the cross-tool conformance suite prove these adapters are interchangeable behind the contract.

## Usable with MemDB — and fully usable without it

The seam is the `Sink` trait. Nothing in the pipeline knows what a sink does with a node:

```rust
pub trait Sink: Send {
    fn emit(&mut self, node: &PreparedNode) -> Result<(), SinkError>;
    fn flush(&mut self) -> Result<(), SinkError>;
}
```

| Sink | Feature | Use |
|:-----|:--------|:----|
| `NdjsonSink` | default | One JSON node per line — the canonical, audit-friendly default. |
| `SqliteSink` | default | A queryable local store with zero external services. |
| `MemDbSink` | `--features memdb` | Writes nodes into MemDB with bi-temporal headers, for Memtrace. **Off by default.** |

Remove the `memdb` feature and Memscribe is a complete, auditable, local capture tool. See [`crates/memscribe-sink/MEMDB.md`](crates/memscribe-sink/MEMDB.md) for the integration design.

## The output contract

Memscribe only ever emits nodes with `Observed` or `DeterministicallyDerived` fact-status. Anything that would require inference (fine-grained decision typing, statistical ranking) is **flagged for a downstream layer, never guessed.**

| Node | Meaning | Fact status |
|:-----|:--------|:------------|
| `Conversation` | A gated, verbatim dialogue span with the commitment markers that fired | `Observed` |
| `Decision` | Parsed deterministically (IBIS/QOC/MADR/Kruchten): epitome, options, `is_ban` | `Observed` |
| `Episode` | The edit(s): path, diff, git sha | `DeterministicallyDerived` |
| `Binding` | decision → episode, with PROV (`t_use ≤ t_gen`) + correlation tuple | `DeterministicallyDerived` |

The **commitment-marker gate** (a config-driven, unit-tested rule table over decision verbs, rejections, bans, and imperatives) is the gate-before-store that the production audits showed is the difference between a working memory and a 97.8%-junk one.

## Why deterministic matters

| | Memscribe | LLM-based capture |
|:--|:--|:--|
| Output is a function of input | ✅ exact | ❌ varies run to run |
| Golden / property / fuzz testable | ✅ | ❌ |
| Cost per session | **$0.00** | API tokens |
| Reads your prompts & secrets | locally, redacted, auditable | sent to a model |
| Fails when a format changes | loudly (a test) | silently (bad data) |

## Testing

Because the pipeline is zero-LLM, it's tested the way a compiler is — fixtures in, exact expected output. The test corpus is a first-class deliverable.

- **Golden-file / snapshot** tests per tool, version, and scenario (`insta`)
- **Cross-tool conformance** — all 9 §8.2 scenarios (happy path, rejected alternative, ban, interleaved arcs, multi-edit, tool failure, rewind/compaction, subagent, no-marker) must normalize to the same shape regardless of tool
- **Property tests** (`proptest`): determinism, idempotency, monotonic seq, losslessness, gate purity, offset resumption
- **Fuzzing** (`cargo-fuzz`): one target per adapter — never panic, never loop, skip-and-continue
- **Redaction & privacy**, **crash/resume**, and a **cross-version corpus**

```bash
cargo test --workspace --all-features          # 260 tests
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo deny check
```

## CLI

| Command | What it does |
|:--------|:-------------|
| `memscribe watch [--tools …] [--sink …] [--out …]` | The steady-state capture daemon: tail transcripts, write nodes. |
| `memscribe parse <file> [--as <tool>]` | One-shot parse a transcript to NDJSON (the workhorse for tests/debugging). |
| `memscribe replay <file>` | Re-run preparation over a historical session. |
| `memscribe verify [--capture <file> --as <tool>]` | Run the conformance summary; `--capture` snapshots a live session into a fixture. |
| `memscribe redact <file> [--no-content]` | Show what the redaction pass would strip. |
| `memscribe hook` | The hook handler agents invoke (reads stdin, records, returns immediately). |

Configure per-tool path overrides, a custom commitment-marker table, redaction patterns, and the sink target in `memscribe.toml` — see [`memscribe.example.toml`](memscribe.example.toml).

## Workspace layout

| Crate | Responsibility |
|:------|:---------------|
| [`memscribe-core`](crates/memscribe-core) | The contract: model, traits, gate, segmenter, binder, node-prep, redaction. Depends on nothing in the workspace. |
| [`memscribe-adapters`](crates/memscribe-adapters) | The 9 per-tool parsers, behind feature flags. |
| [`memscribe-io`](crates/memscribe-io) | Sources: file reader, crash-safe offset tailer, live notify watcher, hook handler, OTLP receiver. |
| [`memscribe-sink`](crates/memscribe-sink) | NDJSON, SQLite, and the feature-gated MemDB sink. |
| [`memscribe-cli`](crates/memscribe-cli) | The `memscribe` binary. |
| [`memscribe-testkit`](crates/memscribe-testkit) | Golden harness, conformance suite, synthetic generators, invariant checks. |

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the deep dive and [`CONTRIBUTING.md`](CONTRIBUTING.md) to add an adapter.

## Requirements

- **Rust ≥ 1.96** (pinned in [`rust-toolchain.toml`](rust-toolchain.toml))
- **Git** — for repo/branch binding on episodes
- No network in the core path; the optional OTLP receiver binds to loopback only.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

<div align="center"><sub>Built by Memrack / Syncable · the deterministic half of the memory system.</sub></div>
