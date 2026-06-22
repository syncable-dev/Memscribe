# Wiring Memscribe into MemDB — a turnkey integration guide

This guide is everything Memtrace needs to route a Memscribe `PreparedNode`
stream into **MemDB** with correct **bi-temporal** headers and a typed
**kind + property** layout. It is meant to be applied to a *clean* Memtrace tree
mechanically: add the submodule, add the path dependency, paste one `Sink`
impl, wire it where the transcript pipeline runs.

Two invariants frame the whole thing:

1. **The dependency points down.** Memtrace depends on Memscribe; Memscribe
   never depends on MemDB or on Memtrace. Default Memscribe builds open no
   MemDB and compile no MemDB code. (See §3.)
2. **MemDB is optional for Memscribe.** NDJSON is the default sink. The MemDB
   sink is behind a `memdb` Cargo feature that is **off by default** and that
   has **no dependency on the `memdb`/`memcore-*` crates** — it is a *reference*
   that prepares records into a neutral [`MemDbRecord`] shape. Memtrace supplies
   the real `memcore_client` and does the final RPC. (See §0, §2.)

Every MemDB type/method named below is grounded in the indexed `memdb` repo;
paths are relative to `memdb/memcore-rs/` and cite the real symbol so the wiring
targets the actual API, not a guess.

---

## 0. The headline: MemDB is optional for Memscribe

Memscribe's pipeline writes through one trait — `memscribe_core::Sink`
(`crates/memscribe-core/src/sink.rs`) — with exactly two methods,
`emit(&PreparedNode)` and `flush()`. Nothing upstream of the sink knows or cares
what the sink does.

The crate ships **three** sinks, and the default has no external dependency:

| Sink | Cargo feature | Default? | External service |
|------|---------------|----------|------------------|
| `NdjsonSink` | *(always built)* | **yes** (canonical) | none — one JSON line per node |
| `SqliteSink` | `sqlite` | on | none — local file / `:memory:` |
| `MemDbSink`  | `memdb`  | **off** | none in Memscribe — Memtrace supplies the client |

In `crates/memscribe-sink/Cargo.toml`:

```toml
[features]
default = ["sqlite"]
sqlite  = ["dep:rusqlite"]
memdb   = []   # OFF by default; NO dep on the memdb crate
```

Consequences:

- `cargo build` / `cargo test -p memscribe-sink` never compiles a line of MemDB
  code. `memdb.rs` is behind `#[cfg(feature = "memdb")]` in `src/lib.rs`.
- The whole module is golden-testable with `NdjsonSink`: one prepared node ⇒ one
  line of canonical JSON. CI, fixtures, and `replay` use it with zero
  infrastructure.
- **`MemDbSink` is itself dependency-free.** Even with `--features memdb`, the
  sink only produces [`MemDbRecord`]s (`{ header, kind, body, properties }`).
  The `memcore_client` round-trip is **Memtrace's** code (§4), so the open-source
  Memscribe workspace never pulls in MemDB.

So "what if I don't have MemDB?" → nothing changes. NDJSON is the transport;
MemDB is an opt-in destination that lives on the Memtrace side.

---

## 1. The MemDB API we write into

The SDK is `memcore_client` (`crates/memcore-client/src/lib.rs`). The relevant
public surface — all verified against the indexed repo:

### Connecting

```rust
// crates/memcore-client/src/lib.rs:97
MemcoreClient::connect(endpoint: impl Into<String>) -> Result<Self, ClientError>
// accepts "http://host:port" / "https://host:port".
// 64 MiB max encode/decode message size (MAX_GRPC_MESSAGE_BYTES, lib.rs:92).
```

### The bi-temporal record header

Every record is fronted by a fixed 32-byte header
(`crates/memcore-core/src/lib.rs:235`, layout asserted at `:279`):

```rust
// crates/memcore-core/src/lib.rs:237
#[repr(C)]
pub struct RecordHeader {
    pub rid:        Rid,        // u64  — server-assigned; pass Rid(0) on create
    pub valid_at:   Micros,     // i64 µs since Unix epoch → VALID time
    pub invalid_at: Micros,     // STILL_VALID = "not superseded"
    pub episode_id: EpisodeId,  // u32  — the arc/episode
    pub schema_ver: SchemaVer,  // u16
    pub kind:       RecordKind, // Node=1, Edge=2, EdgeSegment=3, Episode=4, VectorBlob=5
    pub flags:      u8,
}
```

Supporting newtypes (all `crates/memcore-core/src/lib.rs`), re-exported from the
client at `crates/memcore-client/src/lib.rs:51`:

- `Rid(pub u64)` (`:43`).
- `Micros(pub i64)` — microseconds since the Unix epoch (`:158`).
- `STILL_VALID: Micros = Micros::MAX` — the "never superseded" sentinel (`:172`);
  `invalid_at = STILL_VALID` means the row is currently live.
- `EpisodeId(pub u32)` (`:289`).
- `SchemaVer(pub u16)` (`:149`).
- `RecordKind` — closed enum, `Node=1 / Edge=2 / EdgeSegment=3 / Episode=4 /
  VectorBlob=5` (`:201`).
- `AsOf::now()` = `AsOf(STILL_VALID)`, `AsOf::at(Micros)` (`:177-185`);
  `RecordHeader::visible_at(t)` is `valid_at <= t < invalid_at` with the
  `STILL_VALID` branch (`:260`).

### Creating records (with typed properties)

```rust
// crates/memcore-client/src/lib.rs:492
MemcoreClient::create_record_with_properties(
    header:     RecordHeader,
    body:       Vec<u8>,
    properties: Vec<Property>,
) -> Result<Rid, ClientError>
```

This is the ergonomic create: it builds the prost `CreateRequest`
(`header`, `body`, `properties`, leaving `fencing_token`/`durability`/
`session_id` at server defaults — see `:499-509`) and returns the
server-assigned `Rid`. The SDK's own doc example (`lib.rs:464-487`) shows the
exact shape, and names `memtrace-mcp`'s `upsert_nodes` as the first real
consumer:

```rust
let header = RecordHeader {
    rid: Rid(0), valid_at: Micros(0), invalid_at: STILL_VALID,
    episode_id: EpisodeId(0), schema_ver: SchemaVer(1),
    kind: RecordKind::Node, flags: 0,
};
let properties = PropertyBuilder::new()
    .string("name", "validateToken")
    .string("file_path", "src/auth.ts")
    .int("start_line", 42)
    .build();
let rid = client
    .create_record_with_properties(header, b"<ast-bytes>".to_vec(), properties)
    .await?;
```

- `PropertyBuilder` is re-exported at `crates/memcore-client/src/lib.rs:43`;
  its methods are `.string` / `.int` / `.float` / `.bool` / `.bytes` / `.null`
  / `.with` / `.extend` / `.build`
  (`crates/memcore-client/src/properties.rs:84-162`).
- `Property` / `PropertyValue` are re-exported at `lib.rs:50`
  (`crates/memcore-core/src/properties.rs:42-58`); `PropertyValue` =
  `String|Int|Float|Bool|Bytes|Null`.
- For batches, `bulk_create_records(Vec<BulkRecordRequest>)`
  (`lib.rs:627`) streams `{header, body, properties}` tuples through one WAL tx
  and returns the rids in order.
- Lower-level `create_record(CreateRequest)` (`lib.rs:237`) exists if you ever
  need to set fencing/durability/session explicitly.

### Episodes and counting

- `record_episode(RecordEpisodeRequest) -> RecordEpisodeResponse`
  (`lib.rs:279`). The request carries `actor`, `intent` (mirrors
  `memcore_core::Intent`), `subject_rids`, `payload`, etc.
  (`proto/memcore.proto:780`); the response returns the **`episode_id: u32`**
  (and the global `episode_ulid`) — `proto/memcore.proto:803`. This is where an
  `Episode` node's arc is minted/anchored before the rows that reference it
  land, so its `EpisodeId(u32)` can be stamped into their `RecordHeader`.
- `count_records(kind: RecordKind, as_of: AsOf) -> CountRecordsAck`
  (`lib.rs:408`) — bi-temporal `count(*)` over a kind shard; `AsOf::now()` with
  no filter takes an O(1) counter path, any historical `as_of` takes the scan
  path. This is the natural post-ingest assertion; the canonical pattern is
  `crates/memcore-client/tests/count.rs` (its `node_header(valid_at)` helper,
  `:50`, is exactly the header this guide builds, and it round-trips 50 inserts
  → count 50).

### Edges, honestly

MemDB stores edges with a typed `(out_rid, in_rid)` body
(`memcore_graph::EdgeLayout::encode`, `crates/memcore-graph/src/edge.rs:8`,
server-side). **The client create RPC does not expose those fields** — it takes
only `(header, body, properties)`. So at the SDK layer a binding edge is written
as a record with `kind: RecordKind::Edge` whose **endpoints travel as indexed
properties** (`from`/`to`) plus the body. Memtrace, which assigns the node rids,
is the layer that resolves `from`/`to` ids to rids if/when it wants the
low-level edge layout. The reference sink encodes exactly this (`from`, `to`,
`relation` properties), so no fidelity is lost across the seam.

---

## 2. Mapping a `PreparedNode` onto a MemDB record

Memscribe emits four `PreparedNode` variants
(`crates/memscribe-core/src/node.rs:233`): `Conversation`, `Decision`,
`Episode`, `Binding`. The reference sink (`crates/memscribe-sink/src/memdb.rs`)
derives, for each, a neutral [`MemDbRecord`]:

```rust
pub struct MemDbRecord {
    pub header:     BiTemporal,       // valid_at / transaction_at / episode_id
    pub kind:       RecordKindTag,    // Node | Edge | Episode (matches RecordKind)
    pub node_json:  String,           // canonical JSON → the record body
    pub properties: Vec<Prop>,        // typed rows → Vec<Property> via PropertyBuilder
}
```

The two time axes are distinct and must not be conflated:

- **`valid_at` (valid time)** — when the fact was *true in the world*: the
  turn/episode time. From the node, never the clock.
- **`transaction_at` (transaction time)** — when MemDB *learned* the fact: our
  **ingest** instant. One `MemDbSink` stamps a single `transaction_at` for the
  whole batch (constructor arg) so a replayed transcript lands at one coherent
  transaction instant. MemDB takes **no** explicit `transaction_at`
  `RecordHeader` field; on the wire it is simply the wall-clock at which
  `create_record` is issued. The sink keeps it for audit and as the `valid_at`
  **fallback** when a node has no intrinsic valid time.
- **`episode_id` (the arc)** — `RecordHeader::episode_id`.

The full derivation (intentionally conservative — anchors only where they are
*intrinsic* to the prepared node, never inferred, because Memscribe is a
zero-LLM module):

| Variant | `RecordKind` | `valid_at` | `episode_id` | Key properties |
|---------|--------------|-----------|--------------|----------------|
| `Conversation(ConversationSpan)` | `Node` (1) | *(none — turn-seq only)* | `None` | `node`, `session_id`, `turn_start`, `turn_end`, `marker_count`, `fact_status` |
| `Decision(DecisionRecord)` | `Node` (1) | *(none — turn-seq only)* | `None` | `node`, `epitome`, `is_ban`, `option_count`, `source_span_start`, `source_span_end`, `fact_status` |
| `Episode(CodeEpisode)` | `Episode` (4) | *(none on prepared node)* | `Some(e.episode_id)` | `node`, `episode_id`, `path`, `fact_status` |
| `Binding(BindingEdge)` | `Edge` (2) | `Some(b.prov.t_gen)` — the `wasGeneratedBy` instant | `None` | `node`, `from`, `to`, `relation`, `fact_status` |

Why these choices:

- **Episode → `RecordKind::Episode`, `episode_id` set.** A `CodeEpisode` *is* an
  arc; its `episode_id` (`node.rs:154`) is the deterministic id Memtrace keys
  co-change/provenance off, so it maps straight to `RecordHeader::episode_id`
  (resolved to `EpisodeId(u32)` via `record_episode`). The prepared struct
  carries no in-band `OffsetDateTime`, so `valid_at` falls back to
  `transaction_at`; the git sha / path travel in the body and the `path`
  property.
- **Binding → `RecordKind::Edge`, `valid_at = prov.t_gen`.** A `BindingEdge` is
  a PROV edge with a `ProvRecord` (`node.rs:173`) whose invariant is
  `t_use <= t_gen` (`ProvRecord::is_temporally_valid`). The edit was *generated*
  at `t_gen`, so that is the binding's valid time. Endpoints ride as `from`/`to`
  properties (see §1 "Edges, honestly"). A binding is an edge, not an arc, so
  `episode_id` is `None`.
- **Decision / Conversation → `RecordKind::Node`.** These carry only
  `Range<u64>` turn-seq spans (`node.rs:91`, `:137`), not wall-clock timestamps,
  so there is no intrinsic `valid_at` to set without inference. Their valid time
  defaults to `transaction_at` downstream. (If a future `PreparedNode` carries a
  real timestamp, set `valid_at` from it in `header_for`.)

Converting `valid_at` to the wire type:
`Micros((odt.unix_timestamp_nanos() / 1_000) as i64)`, with
`invalid_at = STILL_VALID` for a live row. `RecordKindTag` carries the exact
`RecordKind` discriminant (`Node=1 / Edge=2 / Episode=4`), asserted in the
sink's tests.

---

## 3. The dependency direction (inverse-direction note)

```
  memtrace  ──depends on──▶  memscribe-core (+ optionally memscribe-sink)
                                   │
                                   ╳  never depends on
                                   ▼
                                memdb / memcore_client
```

- **Memscribe → MemDB: never.** The `memscribe-sink` `memdb` feature pulls in
  **zero** MemDB crates (`Cargo.toml` `memdb = []`). The reference sink only
  emits neutral [`MemDbRecord`]s.
- **Memtrace → Memscribe: yes**, as a vendored submodule + path dependency
  (§4.1). Memtrace already depends on `memcore_client` in its own tree, so the
  `memcore_client` ⇄ `MemDbRecord` glue (§4.2) lives entirely on the Memtrace
  side.
- **Default builds open no MemDB.** Neither a default Memscribe build nor a
  Memtrace build that doesn't enable the bridge touches a socket. The bridge is
  opt-in code Memtrace compiles only where it wants MemDB ingest.

This is the whole point of the `Sink` seam: turning the bridge on **adds a
destination**; it never changes what a prepared node *is*, and turning it off
leaves a complete, testable system on NDJSON.

---

## 4. Applying this to a clean Memtrace tree

### 4.1 Vendor Memscribe + add the path dependency

From the Memtrace repo root:

```bash
git submodule add https://github.com/syncable-dev/Memscribe vendor/memscribe
git -C vendor/memscribe checkout main      # pin to Memscribe main
git add .gitmodules vendor/memscribe
git commit -m "vendor: add Memscribe submodule (transcript capture seam)"
```

Then, in the Memtrace crate that runs transcript ingest (e.g.
`memtrace-mcp/Cargo.toml`), add the path dependencies. Depend on
`memscribe-core` always; pull in `memscribe-sink` with the `memdb` feature for
the [`MemDbRecord`] mapping:

```toml
[dependencies]
# Always: the contract types (PreparedNode, Sink, the four record structs).
memscribe-core = { path = "../vendor/memscribe/crates/memscribe-core" }

# Optional: the reference MemDB mapping (RecordKindTag / Prop / BiTemporal).
# `default-features = false` drops the SQLite sink Memtrace doesn't need.
memscribe-sink = { path = "../vendor/memscribe/crates/memscribe-sink", default-features = false, features = ["memdb"] }

# Memtrace already depends on the MemDB client in its own workspace:
memcore-client = { path = "../memdb/memcore-rs/crates/memcore-client" }
```

> Adjust the relative `path` to match where the submodule and the `memdb` repo
> sit relative to the consuming crate. If Memtrace prefers not to vendor the
> sink, depend on `memscribe-core` only and reproduce the §2 mapping table
> inline — the four-variant logic is small and fully specified above.

### 4.2 The Memtrace-side `Sink`: copy-paste

This is the bridge. It implements `memscribe_core::Sink` directly against the
real `memcore_client`, using the §2 mapping. It depends on
`memscribe-sink`'s [`MemDbRecord`]/[`RecordKindTag`]/[`Prop`] only to reuse the
mapping; if you skipped `memscribe-sink`, inline `kind_for`/`properties_for`
from §2.

```rust
//! memtrace-side bridge: Memscribe PreparedNode -> MemDB.
//! Lives in Memtrace, NOT in Memscribe. Compiled only where MemDB ingest runs.

use memcore_client::{
    AsOf, MemcoreClient, Property, PropertyBuilder, RecordHeader, RecordKind, Rid, STILL_VALID,
};
use memcore_core::{EpisodeId, Micros, SchemaVer};
use memscribe_core::{PreparedNode, Sink, SinkError};
use memscribe_sink::memdb::{MemDbRecord, PropValue, RecordKindTag};
use std::collections::HashMap;
use time::OffsetDateTime;

pub struct MemtraceMemDbSink {
    client: MemcoreClient,
    rt: tokio::runtime::Handle,        // bridge sync Sink::emit onto async RPCs
    transaction_at: OffsetDateTime,    // batch ingest instant (the "now" fallback)
    // Map a Memscribe deterministic episode_id string to the EpisodeId(u32)
    // minted by record_episode, so every row in the arc shares one id.
    episodes: HashMap<String, EpisodeId>,
    schema_ver: SchemaVer,
}

impl MemtraceMemDbSink {
    pub fn new(client: MemcoreClient, rt: tokio::runtime::Handle) -> Self {
        Self {
            client,
            rt,
            transaction_at: OffsetDateTime::now_utc(),
            episodes: HashMap::new(),
            schema_ver: SchemaVer(1),
        }
    }

    /// Reuse Memscribe's reference mapping to derive {header, kind, body, props}.
    /// (If you didn't vendor memscribe-sink, build the MemDbRecord inline from
    /// the §2 table instead — it is the same four-arm match.)
    fn prepare(&self, node: &PreparedNode) -> Result<MemDbRecord, SinkError> {
        // `MemDbSink` is a pure mapper: one node in, one MemDbRecord out.
        let mut sink = memscribe_sink::MemDbSink::new(self.transaction_at);
        sink.emit(node)?;
        // The sink stores by value; clone the single record we just produced.
        Ok(sink.records()[0].clone())
    }

    /// Resolve (or mint) the EpisodeId(u32) for a Memscribe episode_id string.
    fn resolve_episode_id(&mut self, episode_id: &str) -> Result<EpisodeId, SinkError> {
        if let Some(id) = self.episodes.get(episode_id) {
            return Ok(*id);
        }
        // Anchor the arc once. RecordEpisodeRequest carries actor/intent/payload;
        // the response returns the server-issued episode_id (u32).
        // proto/memcore.proto:780 (request) / :803 (response: episode_id field).
        let req = memcore_client::RecordEpisodeRequest {
            actor: "memscribe".into(),
            intent: 2, // Modify; pick from memcore_core::Intent per your semantics
            subject_rids: Vec::new(),
            propagation_rids: Vec::new(),
            parent_ulid: Vec::new(),
            payload: episode_id.as_bytes().to_vec(),
            compress_payload: false,
            durability: 0,
            session_id: Vec::new(),
            fencing_token: 0,
            touched_properties: Vec::new(),
        };
        let resp = self
            .rt
            .block_on(self.client.record_episode(req))
            .map_err(|e| SinkError::Backend(e.to_string()))?;
        let id = EpisodeId(resp.episode_id);
        self.episodes.insert(episode_id.to_string(), id);
        Ok(id)
    }

    fn kind_of(tag: RecordKindTag) -> RecordKind {
        match tag {
            RecordKindTag::Node => RecordKind::Node,
            RecordKindTag::Edge => RecordKind::Edge,
            RecordKindTag::Episode => RecordKind::Episode,
        }
    }

    fn to_micros(t: OffsetDateTime) -> Micros {
        Micros((t.unix_timestamp_nanos() / 1_000) as i64)
    }

    fn properties_of(rec: &MemDbRecord) -> Vec<Property> {
        let mut b = PropertyBuilder::new();
        for p in &rec.properties {
            b = match &p.value {
                PropValue::String(s) => b.string(p.key.clone(), s.clone()),
                PropValue::Int(i) => b.int(p.key.clone(), *i),
                PropValue::Bool(v) => b.bool(p.key.clone(), *v),
            };
        }
        b.build()
    }
}

impl Sink for MemtraceMemDbSink {
    fn emit(&mut self, node: &PreparedNode) -> Result<(), SinkError> {
        let rec = self.prepare(node)?;

        // valid_at: from the node, else fall back to the ingest instant.
        let valid_at = rec
            .header
            .valid_at
            .map(Self::to_micros)
            .unwrap_or_else(|| Self::to_micros(self.transaction_at));

        // episode_id: resolve the arc when the node carries one, else EpisodeId(0).
        let episode_id = match &rec.header.episode_id {
            Some(eid) => self.resolve_episode_id(eid)?,
            None => EpisodeId(0),
        };

        let header = RecordHeader {
            rid: Rid(0),                       // server assigns
            valid_at,
            invalid_at: STILL_VALID,           // live row
            episode_id,
            schema_ver: self.schema_ver,
            kind: Self::kind_of(rec.kind),     // Node / Edge / Episode
            flags: 0,
        };

        let props = Self::properties_of(&rec);
        let body = rec.node_json.into_bytes();

        self.rt
            .block_on(self.client.create_record_with_properties(header, body, props))
            .map_err(|e| SinkError::Backend(e.to_string()))?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}
```

Notes:

- **Sync `Sink`, async client.** `Sink::emit` is synchronous (object-safe,
  `&mut dyn Sink`), so the bridge holds a `tokio::runtime::Handle` and
  `block_on`s each RPC. If Memtrace's ingest is already async, batch into
  `bulk_create_records` (`lib.rs:627`) once per `flush()` for fewer round-trips
  — buffer the `MemDbRecord`s in `emit`, build `Vec<BulkRecordRequest>`, send in
  `flush`.
- **`SinkError::Backend`** is the variant for downstream failures; map any
  `ClientError` onto it (check `memscribe-core/src/error.rs` for the exact
  variant set in your pinned Memscribe and adjust the constructor name).
- **Edges.** The reference mapping puts the binding's endpoints in `from`/`to`
  properties; that is what the SDK create RPC supports. If Memtrace wants
  MemDB's native typed edge body (`out_rid`/`in_rid`), resolve `from`/`to` to
  the rids Memtrace assigned and use the graph-layer path
  (`memcore_graph::EdgeLayout`) instead of `create_record_with_properties`.
- **Schema version.** `SchemaVer(1)` matches the SDK's own example and the
  `count.rs` `node_header` helper. Bump only in lock-step with a MemDB schema
  change.

### 4.3 Wire the sink into Memtrace's pipeline

Wherever Memtrace constructs a Memscribe pipeline today (the place that picks
`NdjsonSink`), branch on config:

```rust
let mut sink: Box<dyn memscribe_core::Sink> = if cfg.memdb_enabled {
    let client = MemcoreClient::connect(&cfg.memdb_endpoint).await?;  // lib.rs:97
    Box::new(MemtraceMemDbSink::new(client, tokio::runtime::Handle::current()))
} else {
    Box::new(memscribe_sink::NdjsonSink::new(writer))
};
// ... run Memscribe's segmenter/binder, then sink.emit_all(&prepared_nodes)?;
```

---

## 5. Verify the wiring — checklist

Run inside Memscribe first (these are the gates this kit ships green):

```bash
cargo test  -p memscribe-sink --features memdb        # bi-temporal + kind + property tests
cargo test  -p memscribe-sink                         # default build: NDJSON + SQLite, no MemDB
cargo clippy -p memscribe-sink --all-targets --features memdb,sqlite -- -D warnings
cargo fmt   -p memscribe-sink --check
```

Then, on the Memtrace side after applying §4:

1. **Submodule pinned.** `git -C vendor/memscribe rev-parse HEAD` is on
   Memscribe `main`; `.gitmodules` committed.
2. **Default build stays MemDB-free.** Build Memtrace *without* the memdb
   feature/config and confirm no `memcore_client` connect happens — NDJSON path
   only. (Dependency direction holds: Memscribe pulls in nothing.)
3. **Bridge compiles.** `cargo check` the Memtrace crate with the bridge
   enabled; the `memscribe-sink`/`memcore-client` versions resolve.
4. **Header is correct, per variant** (assert against a live or `MockEngine`
   server, mirroring `crates/memcore-client/tests/count.rs`):
   - An **Episode** node ⇒ `record_episode` minted an `EpisodeId`, and the row's
     `RecordHeader::episode_id` equals it.
   - A **Binding** node ⇒ `RecordHeader::valid_at == Micros(prov.t_gen)` and
     `kind == RecordKind::Edge`; `from`/`to`/`relation` present as properties.
   - **Decision / Conversation** ⇒ `kind == RecordKind::Node`, `episode_id == 0`,
     `valid_at` = the batch ingest instant.
5. **Round-trip count.** After ingesting a transcript,
   `client.count_records(RecordKind::Node, AsOf::now())` (`lib.rs:408`) returns
   the expected node count; `RecordKind::Episode` / `RecordKind::Edge` counts
   match the episode/binding totals.
6. **Property index hit.** `client.find_by_property(...)` (`lib.rs:361`) on
   `node = "decision"` (or `file_path`/`path`) returns the rows — confirming the
   typed properties reached MemDB's index that backs `find_symbol` /
   `find_code`.

The invariant this preserves: turning the bridge on adds a **destination**; it
never changes what a prepared node *is*, and turning it off leaves a complete,
testable system on NDJSON.
