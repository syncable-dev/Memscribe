# memscribe-fuzz

Coverage-guided fuzz targets for every Memscribe adapter parser (implementation
whitepaper §8.4). The contract every adapter must uphold is simple and strict:

> A parser **must never panic** and must **terminate**. Unrecognized-but-well-
> formed records route to `EventKind::Unknown`; only genuinely malformed bytes
> return `ParseError`. Neither outcome may crash the stream.

These targets exercise exactly that: each one builds its adapter, wraps the raw
fuzz bytes in a `RawRecord`, and calls `parse()` (and `schema_fingerprint()`)
with a fresh `ParseCtx`. libFuzzer turns any panic, hang, or OOM into a crash
artifact you can replay.

## Targets

One target per adapter parser:

| target        | adapter                                      |
| ------------- | -------------------------------------------- |
| `claude_code` | `memscribe_adapters::claude_code::ClaudeCodeAdapter` |
| `codex`       | `memscribe_adapters::codex::CodexAdapter`            |
| `gemini`      | `memscribe_adapters::gemini::GeminiAdapter`          |
| `otel`        | `memscribe_adapters::otel::OtelAdapter`              |
| `cursor`      | `memscribe_adapters::cursor::CursorAdapter`          |
| `windsurf`    | `memscribe_adapters::windsurf::WindsurfAdapter`      |
| `zed`         | `memscribe_adapters::zed::ZedAdapter`                |
| `vscode`      | `memscribe_adapters::vscode::VsCodeAdapter`          |
| `copilot`     | `memscribe_adapters::copilot::CopilotAdapter`        |

## Layout

This crate is **excluded** from the root Cargo workspace (`[workspace] exclude =
["fuzz"]` in the repo-root `Cargo.toml`). That keeps `libfuzzer-sys` — which
needs a nightly toolchain and sanitizer flags — out of a plain `cargo build` /
`cargo test` of the workspace.

## Prerequisites

`cargo-fuzz` runs on a **nightly** toolchain:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Running

From the repository root:

```sh
# Build all targets (compile-only smoke test).
cargo +nightly fuzz build

# Fuzz a single adapter parser.
cargo +nightly fuzz run claude_code

# Time-box a run (recommended for CI).
cargo +nightly fuzz run codex -- -max_total_time=60

# List every target.
cargo +nightly fuzz list
```

Crash-reproducing inputs land in `fuzz/artifacts/<target>/`; replay one with:

```sh
cargo +nightly fuzz run claude_code fuzz/artifacts/claude_code/crash-<hash>
```

## Building without nightly (CI structural check)

The targets are written so that **plain stable `cargo build` also compiles
them**, which lets CI verify the structure is intact even on a runner without
nightly or `cargo-fuzz`:

```sh
cargo build --manifest-path fuzz/Cargo.toml
```

Outside `cargo-fuzz`, the `fuzzing` cfg is unset, so each `[[bin]]` becomes a
tiny stub `main` that exercises the same shared `run()` helper once on an empty
input instead of wiring up the libFuzzer runtime. The actual `fuzz_target!`
entry point is only compiled under `#[cfg(fuzzing)]`, which `cargo-fuzz` sets.

## Relationship to the non-nightly robustness suite

`crates/memscribe-testkit/tests/robustness.rs` is the workspace-resident, no-
nightly counterpart: a `proptest` that feeds mutated and adversarial bytes
(random, truncated JSON, deeply nested JSON, huge numbers, invalid UTF-8) to
every adapter and asserts no panic, bounded time, and that a malformed line is
skipped/`Unknown` rather than aborting the stream. Run it with:

```sh
cargo test -p memscribe-testkit --test robustness
```
