# artisan — roadmap

## Where things stand

This is **the inception commit**. The crate skeleton is in place:
Cargo.toml, README, ARCHITECTURE, the module layout convention,
and starter implementations of `layout/` + `concurrency/hybrid_latch`
+ `store/blob_frame`. Everything else is stubbed with `todo!()` and
a short comment describing what each piece should do.

The goal is **v0.1: a usable embedded library** for path-shaped
metadata, single-node + persistent + crash-safe. After that we
extend (background checkpointer, async backends, MVCC snapshots,
etc.).

## v0.1 — Usable embedded library

Required for the v0.1 tag:

### Core engine

- [x] `NodeType` enum + all per-NodeType struct layouts
      (Leaf 16 B, Prefix 128 B, Blob 128 B, Node{4,16,48,256},
      EmptyRoot 8 B)
- [x] 4096-byte `BlobHeader` with compile-time-asserted field
      offsets (num_slots, root_slot, space_used, gap_space,
      free_list_head, blob_guid)
- [x] Bit-packed `SlotEntry` (`ntype << 17 | offset / 8`)
- [x] `BlobFrame` bump allocator with per-NodeType free list
- [x] 3-mode `HybridLatch` (optimistic / shared / exclusive)
- [ ] Recursive walker: insert / lookup / erase
  - [ ] Leaf + EmptyRoot arms
  - [ ] Node4 arm + promotion to Node16
  - [ ] Node16 + promotion to Node48
  - [ ] Node48 + promotion to Node256
  - [ ] Prefix arm (full-match descent + mismatch split)
  - [ ] BlobNode arm (cross-blob descent)
  - [ ] Shrink chain on erase (Node256 → 48 → 16 → 4 → Leaf)
  - [ ] Tombstone + lazy reclaim
- [ ] Multi-blob (BufferManager + BlobNode crossings)
- [ ] split_blob / make_blob_from_node / compact_blob
- [ ] merge_blob (compaction inverse)
- [ ] Atomic rename (single-latch RenameObject TxnOp)
- [ ] Stateful iterator with prefix + start_after + delimiter

### Persistence + crash safety

- [ ] Physiological WAL with 13+ TxnOp variants
- [ ] WAL replay on startup
- [ ] Snapshot to disk + reload
- [ ] sanity_info validation on replay
- [ ] Synchronous checkpoint (caller invokes `tree.checkpoint()`)

### Storage backends

- [ ] `MemoryBackend` (HashMap + Vec<u8>, for tests + ephemeral)
- [ ] `FileBackend` (one file per blob, POSIX pread/pwrite)
- [ ] Backend trait + builder integration

### Concurrency

- [ ] Wire HybridLatch into the walker (insert takes exclusive,
      lookup takes optimistic, escalates on restart)
- [ ] Cross-blob lock-coupling (`BlobNode` descent acquires the
      target blob's latch)
- [ ] MVCC seq counter actually bumped on writes
- [ ] Per-blob `ext_bfs_latch` (second-tier latch for the ext-blob
      cache)

### Public API

- [ ] `Tree::open(path)` / `Tree::open_in_memory()`
- [ ] `Tree::put / get / delete / rename / contains`
- [ ] `Tree::range(prefix)` + `.delimiter(b'/')` + `.start_after(key)`
      + `.take(n)`
- [ ] `Tree::txn(|t| { ... })` for batch ops under one WAL record
- [ ] `Tree::flush()` / `Tree::checkpoint()`
- [ ] `Tree::stats()` — per-blob compact_times, tombstone count,
      slot utilization
- [ ] `TreeBuilder` for config (buffer_pool_size, wal_dir,
      checkpoint_interval, ...)
- [ ] Typed errors with `thiserror`-style variants

### Testing + benchmarks

- [ ] Unit tests for every NodeType arm of the walker
- [ ] Property-based tests (random key insertion, random erase,
      verify lookup consistency)
- [ ] Recovery tests (insert, kill process mid-write, recover,
      verify)
- [ ] Concurrent stress test (N readers + M writers, no torn reads)
- [ ] Criterion benchmarks: insert throughput, lookup p99, range
      scan over N keys, mixed read/write

### Docs + examples

- [ ] `examples/basic_kv.rs` — minimal "open, put, get, close"
- [ ] `examples/filesystem_meta.rs` — artisan as the metadata layer
      for a toy POSIX filesystem
- [ ] `examples/session_store.rs` — multi-tenant chat session storage
- [ ] `examples/s3_metadata.rs` — artisan as an S3-compatible object
      metadata backend
- [ ] Rendered docs.rs documentation (every public type + method)
- [ ] `docs/benchmarks.md` with numbers vs LMDB / RocksDB / Sled

### Polish

- [ ] CI (cargo test + clippy + rustfmt + miri on a subset)
- [ ] Cross-platform (Linux + macOS + Windows tier-1)
- [ ] MSRV bump policy
- [ ] Versioning policy
- [ ] CHANGELOG.md
- [ ] CONTRIBUTING.md
- [ ] CODE_OF_CONDUCT.md

## v0.2 — Performance

- Async checkpointer (3 background threads: checkpoint / io / eviction)
- io_uring backend (Linux, behind feature flag)
- SIMD-accelerated Node16 keys[] scan (vpcmpeqb)
- Lock-free reader fast path (validated optimistic snapshot)
- Buffer-pool tuning + adaptive eviction
- Metrics export (Prometheus + OpenTelemetry traces)

## v0.3 — Advanced features

- Full MVCC snapshots (read at a specific seq, snapshot iteration)
- Online compaction (background, doesn't block writers)
- Change feed / subscription API (consumers receive a stream of
  TxnOps)
- Column families (multiple independent ARTs in one Tree)
- Encryption-at-rest (per-blob AES-GCM)
- Compression (per-blob Zstd, transparent)

## v1.0 — Production-ready

- Comprehensive feature set covered.
- Multi-platform stability (Linux + macOS + Windows + BSDs).
- Real production deployments + case studies.
- Long-term API stability commitment.

## Not on the roadmap

The library deliberately stays single-node. Things outside scope:

- **Replication / consensus**: build it above this. We expose hooks
  (change feed, snapshot transfer) but don't implement Raft.
- **Network server**: this is a library. Wrap it in your gRPC /
  HTTP / whatever.
- **SQL**: not the right abstraction for this data shape.
- **Vector search**: combine with a dedicated vector DB.
- **Full-text search**: combine with Tantivy / Lucene-rs.

## Contributing

We're at very early stage; ideas + design feedback most welcome.
PRs welcome too, but please open an issue first for non-trivial
changes — the architecture is being shaped and we want to avoid
churn.
