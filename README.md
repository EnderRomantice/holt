# artisan

> A carefully crafted **adaptive radix tree** for path-shaped metadata.

`artisan` is an embedded Rust library for storing **hierarchical
keys** — file paths, S3 object names, multi-tenant namespaces,
time-bucketed identifiers — with sub-microsecond lookups, per-blob
concurrency, and crash-safe persistence.

It is **not** a general-purpose KV store. It targets the workloads
where:

- Keys are **hierarchical / path-shaped** (so prefix compression pays).
- The dominant access pattern is **point lookup + prefix range scan**.
- Concurrency is **high** (many readers + writers across disjoint
  subtrees).
- Latency is **micro-critical** — no LSM compaction stalls, no
  single-writer locks.

If you need full-text search or vector similarity, use a different
tool. If you need exactly this shape, artisan should beat
LMDB / RocksDB / SQLite on its target workload.

## When to reach for artisan

| Engine          | Data structure | Persistence       | Concurrency        | Notes                                                       |
|-----------------|----------------|-------------------|--------------------|-------------------------------------------------------------|
| LMDB            | B+tree         | mmap              | Single-writer MVCC | Battle-tested; cross-page chasing for short hot keys.       |
| RocksDB         | LSM            | SST + WAL         | MVCC               | Compaction stalls; large hot dataset is RAM-heavy.          |
| SQLite          | B-tree         | File              | Single writer      | Convenient, but writer is the bottleneck under load.        |
| Sled            | Hybrid LSM     | Log-structured    | Lock-free          | Rust-native, but largely unmaintained.                      |
| **artisan**     | **Adaptive Radix Tree** | **512 KB blobs** | **Per-blob 3-mode latch** | **Path compression + lookup is O(key.len)**     |

ART's lookup cost is `O(key.len)`, not `O(log N)`. For short hot keys
(say, < 64 bytes), that's faster than any tree-based competitor. The
per-blob HybridLatch lets N readers traverse different subtrees in
parallel without coordinating with each other.

## Project status

**Early development.** v0.1 is being built out. See
[ROADMAP.md](ROADMAP.md) for the feature timeline. The layout layer
(extern-struct types + slot encoding + 4 KB blob header) is in
place and `cargo test` passes (28 tests, no external dependencies).
The walker, persistence, journal, and backend layers are being
filled in.

## Quick taste (when v0.1 ships)

```rust
use artisan::TreeBuilder;

let tree = TreeBuilder::new("/var/lib/myapp/meta.artisan")
    .buffer_pool_size(128)
    .open()?;

// Single-key put / get / delete.
tree.put(b"img/01.jpg", b"rgb_data_blob_id_abc")?;
let value = tree.get(b"img/01.jpg")?.unwrap();
tree.delete(b"img/01.jpg")?;

// Path-prefix range scan with S3-style delimiter (folder rollup).
for entry in tree.range(b"img/").delimiter(b'/').take(100) {
    println!("{} -> {}", entry.key_str(), entry.value_str());
}

// Atomic rename — single per-blob latch hold.
tree.rename(b"old/path", b"new/path", /*force=*/false)?;

// Batch / transaction (groups multiple ops under one WAL record).
tree.txn(|t| {
    t.put(b"users/alice/email", b"alice@example.com")?;
    t.put(b"users/alice/created_at", b"2026-05-19T08:00:00Z")?;
    Ok(())
})?;
```

## Architecture at a glance

```
┌────────────────────────────────────────────────────────────┐
│ Public API: Tree, Iter, Txn, TreeBuilder                    │
├────────────────────────────────────────────────────────────┤
│ Engine: insert / lookup / erase / scan / rename / compact   │
├────────────────────────────────────────────────────────────┤
│ Concurrency: HybridLatch (3-mode) + lock-coupling           │
├────────────────────────────────────────────────────────────┤
│ Journal: physiological WAL + replay + checkpoint            │
├────────────────────────────────────────────────────────────┤
│ Store: BufferManager + BlobFrame (512 KB, bump alloc)       │
├────────────────────────────────────────────────────────────┤
│ Layout: NodeType variants + SlotEntry + BlobHeader          │
├────────────────────────────────────────────────────────────┤
│ Backend: file / mmap / memory (pluggable trait)             │
└────────────────────────────────────────────────────────────┘
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the deep dive.

## Design notes

- **Adaptive Radix Tree** core: four internal node sizes
  (4 / 16 / 48 / 256 children) chosen at runtime to keep the tree
  dense. Lookup walks one byte of the key per level.
- **Path compression** via a dedicated `Prefix` node variant — long
  shared paths cost one node, not one per byte.
- **Multi-blob via in-tree crossings**: when a 512 KB blob fills,
  a subtree is migrated to a fresh blob and a `BlobNode` crossing is
  written into the parent. Trees grow to arbitrary size in 512 KB
  increments.
- **Per-blob `HybridLatch`** (LeanStore-style 3-mode lock):
  optimistic readers take a version snapshot and validate; shared
  readers take a reader counter; writers take exclusive. Reader
  fast path is wait-free under no contention.
- **Crash safety** via a physiological WAL with 13+ TxnOp variants
  and a synchronous (or eventually asynchronous) checkpointer.
- **Per-blob free-list** lets recycled slot indices feed back into
  the bump allocator with zero overhead.

The design is informed by:

- Leis et al., "The Adaptive Radix Tree: ARTful Indexing for
  Main-Memory Databases" (ICDE 2013) — the four-node-size scheme.
- Leis et al., "LeanStore: In-Memory Data Management Beyond Main
  Memory" (ICDE 2018) — the HybridLatch contract.

## What this is NOT

To avoid surprise:

- **Not a SQL database.** No joins, no aggregates, no query planner.
- **Not a vector DB.** No kNN, no embeddings, no similarity search.
- **Not a full-text index.** No tokenization, no inverted index.
- **Not a replication / consensus layer.** The library is single-node
  + persistent. Replication is a layer above this.
- **Not a network server.** This is a library you embed; bring your
  own RPC.

For these, combine artisan with a domain-appropriate engine:

- artisan + FAISS / Qdrant / pgvector → AI workspace metadata + vectors
- artisan + Tantivy → FS metadata + full-text
- artisan + custom Raft → distributed deployments

## License

Dual-licensed under Apache-2.0 OR MIT.
