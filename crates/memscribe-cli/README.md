# memscribe (CLI)

The `memscribe` binary — the daemon and the toolbox for Memscribe's
deterministic, zero-LLM transcript capture. It wraps the workspace crates
(`memscribe-core`, `-adapters`, `-io`, `-sink`) behind six subcommands.

```console
cargo run -p memscribe-cli -- <command> [args]
# or, once installed:
memscribe <command> [args]
```

The pipeline is deterministic and never calls a model. By default the redaction
pass is **on**, so secrets are stripped before anything is written. See the
workspace [ARCHITECTURE.md](../../ARCHITECTURE.md) for the pipeline and
[memscribe.example.toml](../../memscribe.example.toml) for the config surface.

---

## Commands

### `watch` — the steady-state capture daemon

Tail discovered transcripts (and serve the hook endpoint), preparing nodes to a
sink as they arrive.

```console
memscribe watch [--tools claude,codex,gemini] [--sink ndjson|sqlite|memdb] \
                [--out FILE|-] [--root DIR ...] [--once] [--config memscribe.toml]
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--tools` | every adapter | Comma-separated tool slugs to watch (`SourceKind::parse` values). |
| `--sink` | `ndjson` | Sink target: `ndjson`, `sqlite`, or `memdb` (`memdb` needs the `memdb` feature). |
| `--out` | `-` (stdout) | Where prepared nodes go (a file for `ndjson`/`sqlite`); `-` is stdout. |
| `--root` | `$HOME` | Directory root(s) to scan for transcripts; repeatable. |
| `--once` | off | Drain what already exists and exit, instead of tailing live. |
| `--config` | — | Path to a `memscribe.toml` (see `memscribe.example.toml`). |

### `hook` — the hook handler

Reads a hook payload from stdin, records it, and exits `0` immediately. It never
blocks the agent and never invokes a model. Agents wire this as their hook
command.

```console
memscribe hook < payload.json
```

### `parse` — one-shot parse a transcript to NDJSON

The workhorse for tests and debugging: run one transcript file through the
adapter and the full pipeline, emitting prepared nodes as NDJSON on stdout.

```console
memscribe parse <file> [--as TOOL] [--no-redact]
```

- `--as TOOL` forces a specific adapter (`claude_code`, `codex`, `gemini`,
  `otel`, `cursor`, `windsurf`, `zed`, `vscode`, `copilot`). Omit it to infer
  the tool from the path; if inference fails the command tells you to pass
  `--as`.
- `--no-redact` emits verbatim content (used by golden tests that assert on
  exact text). Redaction is on otherwise.

```console
memscribe parse ~/.claude/projects/foo/session.jsonl --as claude_code
```

### `replay` — re-run preparation over a historical session

Re-prepares a transcript file with the current pipeline (redaction on). Useful
after an adapter or pipeline change to see the new node stream for an old
session.

```console
memscribe replay <file> [--as TOOL]
```

### `verify` — the conformance smoke suite

Parses every fixture under `fixtures/` and prints a per-tool `CASES / OK / NODES`
table, exiting non-zero on any failure. This is the fast, shellable summary the
daemon ships with; full cross-tool conformance and the §8.3 invariants live in
the testkit (`cargo test -p memscribe-testkit`).

```console
memscribe verify
memscribe verify --capture     # (planned) snapshot a live session into a new fixture
```

### `redact` — preview the redaction pass

Reads a file and prints it with secrets replaced by `[REDACTED:<label>]`,
warning on stderr if anything was stripped. `--no-content` elides all text and
keeps only structure.

```console
memscribe redact session.jsonl
memscribe redact session.jsonl --no-content
```

---

## Logging

Logs go to **stderr** (stdout is reserved for node output), filtered by the
standard `RUST_LOG` env var. The default level is `warn`.

```console
RUST_LOG=debug memscribe parse session.jsonl --as codex
```

---

## Build features

The CLI builds every adapter by default. The MemDB sink is feature-gated in
`memscribe-sink` and **off by default**; `--sink memdb` only does anything once
that feature is compiled in.

```console
cargo build -p memscribe-cli     # ndjson sink (default), all adapters
```

The CLI does not yet expose a passthrough feature for `memscribe-sink/memdb`, so
enabling the MemDB sink is a build-config follow-up: add a
`memdb = ["memscribe-sink/memdb"]` feature to `crates/memscribe-cli/Cargo.toml`
and build with `--features memdb`. Until then, `--sink ndjson` (the default) and
`--sink sqlite` are the available targets from the binary.
