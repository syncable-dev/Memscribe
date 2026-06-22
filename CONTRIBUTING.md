# Contributing to Memscribe

Thanks for helping. Memscribe is the **deterministic, zero-LLM** data layer
beneath MemCortex (which Memtrace builds on). The bar for a change is unusual: the output must be
an *exact function* of the input, so the test suite is the contract. Read
[ARCHITECTURE.md](./ARCHITECTURE.md) first — it explains the pipeline and the
contract types you must not break.

---

## Build & test

The toolchain is pinned in `rust-toolchain.toml` (**1.96.0**); `rustup` picks it
up automatically. The MSRV is **1.96**.

```console
# The whole gate, the way CI runs it:
cargo test  --workspace --all-features            # unit + golden + conformance + property
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt   --all --check
cargo deny  check                                 # license + advisory policy (deny.toml)
```

When you are iterating on a single crate or a single integration test, build it
in **isolation** so you do not compile peers' in-progress test files:

```console
cargo test -p memscribe-adapters --test <your_file_stem>
cargo test -p memscribe-core
```

Do **not** run `cargo fmt` to reformat in a PR that you want reviewed cleanly —
write rustfmt-clean code and let `cargo fmt --all --check` verify it.

---

## The rules that make Memscribe Memscribe

These are not style preferences; they are the invariants the test suite (and the
downstream consumer) rely on. A change that violates one is a bug.

### 1. Determinism

The same input bytes must always produce byte-identical output. No clocks, no
randomness, no hash-map iteration order leaking into output, no filesystem walk
order leaking into discovery (sort it). The property test
`invariants::check_determinism` runs two parses and asserts the serialized
events are identical — keep it green.

### 2. No LLM, ever

Capture is reading and parsing, never summarizing or inferring. Memscribe emits
nodes only with `FactStatus::Observed` (verbatim) or
`FactStatus::DeterministicallyDerived` (a pure function of observed data).
Anything that would require inference — fine-grained decision typing, concept
naming, statistical ranking — is **flagged** (`StatisticallyRanked`,
`LlmHypothesis`) for a downstream layer to compute, never guessed here. If you
find yourself reaching for a heuristic that "usually" gets it right, stop: that
belongs downstream.

### 3. Losslessness

Every non-blank source record maps to at least one event. Unrecognized records
and new fields are preserved verbatim and routed to `EventKind::Unknown` /
`Part::Other`, never dropped. `invariants::check_lossless` enforces the lower
bound.

### 4. Monotonic, unique, idempotent

`seq` is strictly increasing within a session and matches file order
(`check_monotonic_seq`). A record is deduplicated once on its tool-native
`event_id` (`check_unique_event_ids`), so re-ingesting the same input is
idempotent.

### 5. Never panic

A parser must never panic on any input — malformed, truncated, adversarial, or
from a tool version it has never seen. Every adapter parser has a `cargo-fuzz`
target that asserts this. Use the version-tolerant pattern: match the fields you
need, route the rest to `Unknown`.

---

## The adapter version-tolerance contract

Tool transcript formats churn constantly; that is precisely why the adapters are
the open, community-versioned part of Memscribe. An adapter must:

- **Pattern-match only the fields it needs** and route anything unrecognized to
  `EventKind::Unknown` (with the raw record preserved) rather than failing the
  stream. A new field in a record you understand must not break parsing.
- **Never panic** on any input (see rule 5).
- **Fingerprint** its input via `schema_fingerprint` → `SchemaVariant`, so the
  corpus and runtime can version-gate the parser (e.g. `claude_code/2.1`,
  `codex/rollout-v2`). When a tool ships an incompatible format, add a new
  variant and a fixture under that version — do not silently widen the old one.
- **Honor `DiscoverCfg`** in `discover`: read the per-tool override key from
  `DiscoverCfg.overrides` (e.g. `CLAUDE_CONFIG_DIR`, `CODEX_HOME`), fall back to
  `cfg.home_dir()`, and return handles in a **sorted** (deterministic) order.

The payoff is the conformance suite: the same canonical scenario, captured from
any tool, must normalize to the **same shape**. See ARCHITECTURE.md, "How to add
a new adapter," for the full five-step path.

---

## The fixture-corpus workflow: capture → golden → bless

Memscribe is tested the way a compiler is — fixtures in, exact expected output.
The corpus lives in two trees:

```text
fixtures/<tool>/<version>/<scenario>.jsonl                  # input transcript
fixtures-expected/<tool>/<version>/<scenario>.events.json   # expected CaptureEvent[]
fixtures-expected/<tool>/<version>/<scenario>.nodes.json    # expected PreparedNode[]
```

The canonical scenario slugs are defined once in
`memscribe-testkit::scenarios::SCENARIOS` (e.g.
`happy_path_decision_then_edits`, `rejected_alternative`, `ban`,
`tool_failure`, …). Every tool's fixtures should cover them so the cross-tool
conformance suite can assert equivalence.

**1. Capture.** Get a real (or hand-authored, minimal, redacted) transcript for
the scenario and drop it at `fixtures/<tool>/<version>/<scenario>.jsonl`. Real
captures must be scrubbed of secrets and personal paths first — run them through
the redactor and eyeball the result. For a live session you can snapshot:

```console
memscribe verify --capture     # (planned) snapshot a live session into a new fixture
```

**2. Golden.** Generate the expected output and inspect it by hand. The harness
parses with the adapter and runs the pipeline with **redaction off** so the
golden asserts on verbatim content:

```console
# Eyeball the normalized events and prepared nodes for a fixture:
memscribe parse fixtures/<tool>/<version>/<scenario>.jsonl --as <tool> --no-redact
```

Confirm it satisfies the invariants and the scenario's stated expectation
(e.g. for `ban`, the `DecisionRecord.is_ban` flag is `true`).

**3. Bless.** Write the reviewed output to the `fixtures-expected/` paths
(`*.events.json`, `*.nodes.json`). The golden tests use `insta`; accept a
reviewed snapshot with:

```console
cargo insta review          # interactively accept/reject changed snapshots
# or, after eyeballing the diff:
cargo insta accept
```

**Never bless a snapshot you have not read.** A blessed golden is a claim about
exactly what the deterministic pipeline produces; an unreviewed `accept` turns a
real regression into a permanent "expected" value. If a golden changes
unexpectedly, that is the suite doing its job — find out *why* before you
re-bless.

---

## Submitting a change

- Keep the change scoped; touch only the crate(s) you own. `memscribe-core` is a
  frozen contract — changing a public type or its output ripples through every
  consumer and every golden, so coordinate those separately.
- Make sure all five hard gates above pass locally before you open the PR.
- Add or update fixtures + tests for any behavior change. A behavior change with
  no golden delta is a red flag.

By contributing you agree your work is dual-licensed under **MIT OR Apache-2.0**,
matching the project.
