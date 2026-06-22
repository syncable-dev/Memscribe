# Writing Memscribe into MemDB

How `memscribe-sink` ingests prepared nodes into **MemDB**, and — just as
important — how Memscribe stays **fully usable without MemDB**.

This document is grounded in the real MemDB SDK as it exists in the indexed
`memdb` repo. Every type and method named below is cited with its source
location so the wiring can be implemented against the actual API rather than a
guess. All paths are relative to
`memdb/memcore-rs/crates/`.

---

## 0. The headline: MemDB is optional

Memscribe's pipeline writes through one trait — `memscribe_core::Sink`
(`memscribe-core/src/sink.rs`) — with exactly two methods, `emit(&PreparedNode)`
and `flush()`. Nothing upstream of the sink knows or cares what the sink does.

The crate ships **three** sinks, and the default has no external dependency at
all:

| Sink | Cargo feature | Default? | External service |
|------|---------------|----------|------------------|
| `NdjsonSink` | *(always built)* | **yes** (canonical) | none — one JSON line per node |
| `SqliteSink` | `sqlite` | on | none — local file/`:memory:` |
| `MemDbSink`  | `memdb`  | **off** | MemDB (gRPC) |

In `Cargo.toml`:

```toml
[features]
default = ["sqlite"]
sqlite  = ["dep:rusqlite"]
memdb   = []   # OFF by default
```

Consequences:

- `cargo build` / `cargo test -p memscribe-sink` never compiles a line of
  MemDB code. `memdb.rs` is behind `#[cfg(feature = "memdb")]` in
  `src/lib.rs`, so the MemDB types do not even exist in the default build.
- The whole module is observable and golden-testable with `NdjsonSink`: one
  prepared node ⇒ one line of canonical JSON. CI, fixtures, and `replay` use
  it with zero infrastructure.
- `SqliteSink` gives a queryable local store (indexed by variant tag and
  `FactStatus`, deduplicated by a deterministic primary key) for anyone who
  wants SQL without standing up MemDB.
- **Memtrace** — and only Memtrace — flips on `--features memdb` to route the
  same `PreparedNode` stream into MemDB. No other consumer pays for the
  dependency.

So the answer to "what if I don't have MemDB?" is: nothing changes. NDJSON is
the system of record's transport; MemDB is an opt-in destination.

---

## 1. The MemDB API we write into

The SDK is `memcore_client` (`memcore-client/src/lib.rs`). The relevant public
surface:

### Connecting

```rust
// memcore-client/src/lib.rs:97
MemcoreClient::connect(endpoint: impl Into<String>) -> Result<Self, ClientError>
// accepts "http://host:port" / "https://host:port"; 5s connect timeout;
// 64 MiB max message size (MAX_GRPC_MESSAGE_BYTES, lib.rs:92).
```

### The bi-temporal record header

Every record on disk in MemDB is fronted by a fixed 32-byte header
(`memcore-core/src/lib.rs:237`, layout documented there):

```rust
// memcore-core/src/lib.rs
pub struct RecordHeader {
    pub rid:        Rid,        // u64  — server-assigned (pass Rid(0) on create)
    pub valid_at:   Micros,     // i64 µs since Unix epoch  → VALID time
    pub invalid_at: Micros,     // STILL_VALID = "not superseded"
    pub episode_id: EpisodeId,  // u32  — the arc/episode
    pub schema_ver: SchemaVer,  // u16
    pub kind:       RecordKind, // Node=1, Edge=2, EdgeSegment=3, Episode=4, VectorBlob=5
    pub flags:      u8,
}
```

Supporting newtypes (all `memcore-core/src/lib.rs`):

- `Micros(pub i64)` — microseconds since the Unix epoch (`:158`).
- `STILL_VALID: Micros = Micros::MAX` — the "never superseded" sentinel
  (`:172`); `invalid_at = STILL_VALID` means the row is currently live.
- `EpisodeId(pub u32)` (`:289`).
- `SchemaVer(pub u16)` (`:149`).
- `RecordKind` — closed enum, no extension point (`:201`).
- `AsOf::now()` = `AsOf(STILL_VALID)`, `AsOf::at(Micros)` (`:179`) — the read
  side's as-of clock. `RecordHeader::visible_at(t)` is `valid_at <= t <
  invalid_at` with the `STILL_VALID` branch (`:260`).

### Creating records (with typed properties)

```rust
// memcore-client/src/lib.rs:492
MemcoreClient::create_record_with_properties(
    header:     RecordHeader,
    body:       Vec<u8>,
    properties: Vec<Property>,
) -> Result<Rid, ClientError>
```

This is the ergonomic create: it builds the prost `CreateRequest`
(`header`, `body`, `properties`, leaving `fencing_token`/`durability`/
`session_id` at server defaults) and returns the server-assigned `Rid`. The
typed properties feed MemDB's property index, which is what backs Memtrace's
`find_symbol` / `find_code`. Properties are built with `PropertyBuilder`
(`memcore-client` re-exports it at `lib.rs:43`); from the SDK's own doc example
(`lib.rs:465-487`):

```rust
let properties = PropertyBuilder::new()
    .string("name", "validateToken")
    .string("file_path", "src/auth.ts")
    .int("start_line", 42)
    .build();
let rid = client
    .create_record_with_properties(header, body, properties)
    .await?;
```

There is also a lower-level `create_record(CreateRequest)` (`lib.rs:237`) if we
ever need to set fencing/durability/session explicitly.

### Episodes and counting

- `record_episode(RecordEpisodeRequest) -> RecordEpisodeResponse`
  (`lib.rs:279`) — registers an episode (the arc) with its coordination
  payload. This is where an `Episode` node's `episode_id` is minted/anchored
  before the record rows that reference it land.
- `count_records(kind: RecordKind, as_of: AsOf) -> CountRecordsAck`
  (`lib.rs:408`) — bi-temporal `count(*)` over a kind shard; `AsOf::now()` +
  no filter takes an O(1) counter path, any historical `as_of` takes the
  scan path. This is the natural post-ingest assertion (see
  `memcore-client/tests/count.rs`, which builds a `node_header(valid_at)` and
  round-trips 50 inserts → count 50).

---

## 2. Mapping a `PreparedNode` onto a `RecordHeader`

Memscribe emits four `PreparedNode` variants (`memscribe-core/src/node.rs:233`):
`Conversation`, `Decision`, `Episode`, `Binding`. The sink's job is to derive a
correct **bi-temporal header** for each. The two axes are distinct and must not
be conflated:

- **`valid_at` (valid time)** — when the fact was *true in the world*: the
  turn/episode time. This comes **from the node**, never from the clock.
- **`transaction_at` (transaction time)** — when MemDB *learned* the fact: our
  **ingest** instant. One `MemDbSink` stamps a single `transaction_at` for the
  whole batch (constructor arg, `src/memdb.rs`) so a replayed transcript lands
  at one coherent transaction instant. On the wire this is simply the
  wall-clock at which `create_record` is issued; MemDB does not take it as an
  explicit `RecordHeader` field, so the sink keeps it in its own `BiTemporal`
  header for audit and defers to the RPC instant on send.
- **`episode_id` (the arc)** — `RecordHeader::episode_id`.

The current derivation (`src/memdb.rs`, `header_for`) — intentionally
conservative, deriving anchors only where they are *intrinsic* to the prepared
node and never inferred:

| Variant | `valid_at` | `episode_id` | Maps to MemDB |
|---------|-----------|--------------|---------------|
| `Episode(CodeEpisode)` | *(none on prepared node)* | `Some(e.episode_id)` | `RecordKind::Episode` (4); register via `record_episode`, then a `Node`/`Episode` record with `episode_id` set |
| `Binding(BindingEdge)` | `Some(b.prov.t_gen)` — the `wasGeneratedBy` instant | `None` | `RecordKind::Edge` (2) `from → to`, typed by `relation` |
| `Decision(DecisionRecord)` | *(none — only turn-seq spans)* | `None` | `RecordKind::Node` (1) |
| `Conversation(ConversationSpan)` | *(none — only turn-seq spans)* | `None` | `RecordKind::Node` (1) |

Why these choices:

- **Episode → `episode_id`.** A `CodeEpisode` *is* an arc; its `episode_id`
  (`node.rs:154`) is the deterministic id Memtrace keys co-change/provenance
  off, so it maps straight to `RecordHeader::episode_id`. The prepared struct
  carries no in-band `OffsetDateTime`, so `valid_at` is left for the consumer
  (it falls back to `transaction_at`); the git sha / path travel in the body
  and properties.
- **Binding → `valid_at = prov.t_gen`.** A `BindingEdge` is a PROV edge with a
  `ProvRecord` (`node.rs:173`) whose invariant is `t_use <= t_gen`
  (`ProvRecord::is_temporally_valid`). The edit was *generated* at `t_gen`, so
  that is the binding's valid time. A binding is an edge, not an arc, so
  `episode_id` is `None`.
- **Decision / Conversation.** These carry only `Range<u64>` turn-seq spans
  (`node.rs:91`, `:137`), not wall-clock timestamps, so there is no intrinsic
  `valid_at` to set without inference — and Memscribe is a zero-LLM,
  no-guessing module (`node.rs:1-7`). Their valid time defaults to
  `transaction_at` downstream. (If a future `PreparedNode` carries a real
  timestamp, set `valid_at` from it here.)

Converting `valid_at` to the wire type: `Micros((odt.unix_timestamp_nanos() /
1_000) as i64)`, with `invalid_at = STILL_VALID` for a live row. `episode_id`
(currently a deterministic `String`) is resolved to the `EpisodeId(u32)`
returned/anchored by `record_episode`.

---

## 3. End-to-end ingest sketch (when `record_episode` and the client are wired)

```rust
// feature = "memdb"
let client = MemcoreClient::connect("http://127.0.0.1:7878").await?;

for record in sink.records() {          // each is a MemDbRecord { header, node_json }
    let valid_at = record.header.valid_at
        .map(|t| Micros((t.unix_timestamp_nanos() / 1_000) as i64))
        .unwrap_or(Micros(/* ingest fallback */));

    // Resolve the arc id (anchored once per episode via record_episode).
    let episode_id = resolve_episode_id(&record.header.episode_id, &client).await?;

    let header = RecordHeader {
        rid: Rid(0),                    // server assigns
        valid_at,
        invalid_at: STILL_VALID,
        episode_id,
        schema_ver: SchemaVer(1),
        kind: kind_for(&record),        // Node / Edge / Episode
        flags: 0,
    };
    let props = property_rows(&record); // name/file_path/start_line/… via PropertyBuilder
    client
        .create_record_with_properties(header, record.node_json.into_bytes(), props)
        .await?;
}
// Post-ingest assertion mirrors memcore-client/tests/count.rs:
let ack = client.count_records(RecordKind::Node, AsOf::now()).await?;
```

The current `MemDbSink` stops one step short of this: it prepares every node
into an in-memory `MemDbRecord` carrying the **correct bi-temporal shape**
(`BiTemporal { valid_at, transaction_at, episode_id }`), which Memtrace's own
integration test asserts against. Swapping the `Vec<MemDbRecord>` push for the
`create_record_with_properties` call above is the only remaining wiring, and it
is isolated entirely inside this one feature-gated module — no other crate
changes.

---

## 4. Test posture

- **Default build** (`cargo test -p memscribe-sink`): exercises `NdjsonSink`
  and `SqliteSink` only. No MemDB, no network. `SqliteSink` is tested by
  inserting each of the four `PreparedNode` variants and reading them back,
  asserting the stored `fact_status` and JSON round-trip per variant, plus
  primary-key upsert behaviour.
- **MemDB build** (`cargo test -p memscribe-sink --features memdb`): adds the
  `MemDbSink` bi-temporal tests — an `Episode` node yields a header with
  `episode_id` set, and a `Binding` node yields `valid_at = prov.t_gen` — with
  `transaction_at` shared across a batch. These run **without** a live MemDB
  because the sink prepares records in memory; the real gRPC round-trip is
  covered on Memtrace's side against a `MockEngine` (the pattern in
  `memcore-client/tests/count.rs`).

The invariant this preserves: turning the `memdb` feature on adds a
destination; it never changes what a prepared node *is*, and turning it off
leaves a complete, testable system on NDJSON.
