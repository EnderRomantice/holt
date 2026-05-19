# Changelog

All notable changes to **holt** are documented in this file. Format
adapted from [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
versioning follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Breaking — public API closure

The v0.1 crate exposed `holt::layout`, `holt::journal`, `holt::store`
as `pub mod`s. That leaked the on-disk struct layout, WAL record
codec, `BufferManager`, allocator outcomes, and three `HybridLatch`
guard types as part of the SemVer surface — any future change to
those (and there will be many) would break downstream users.

The supported, SemVer-stable user surface is now narrow:

```text
holt::Tree                holt::Storage             holt::TreeStats
holt::TreeBuilder         holt::CheckpointConfig    holt::BlobStats
holt::TreeConfig          holt::CheckpointerStats   holt::TxnBatch
holt::Error               holt::RangeBuilder
holt::Result              holt::RangeEntry
                          holt::RangeIter

holt::Backend             holt::AlignedBlobBuf      holt::BlobGuid
holt::MemoryBackend       holt::PersistentBackend
```

`metrics::render_prometheus` is gated behind the `metrics` feature
and is part of that feature's surface.

What changed:

- **`layout`, `journal`, `store` are now `pub(crate)`.** Their
  contents (`BlobHeader`, `SlotEntry`, `Node{4,16,48,256}`, the WAL
  codec, `BufferManager`, `BlobFrame`, `*Guard` types) are crate-
  internal. Users who reached into them (chiefly tools that
  introspected on-disk frames) need to either pin to v0.1.* or open
  an issue describing the use case — most can be served by stable
  helpers added to the public surface.
- **`pub use holt::BufferManager` removed.** It was never part of
  what users should be touching.
- **`BlobGuid` re-exported at the crate root.** Custom `Backend`
  implementations need to name blobs; `holt::BlobGuid` is the
  supported path.
- **`RangeBuilder::new(bm, root_guid)` is `pub(crate)`.** It was
  always an internal constructor; the user surface is
  `Tree::range()` / `Tree::scan_prefix()`. The signature also
  references `BufferManager`, which is no longer public.
- **`TreeConfig::checkpoint_byte_interval` field and
  `TreeBuilder::checkpoint_byte_interval(bytes)` builder method
  removed.** The field was marked `Reserved` and never read
  anywhere in the engine — a classic "experimental that escaped".
  Configure the WAL drain cadence via `CheckpointConfig`
  (background) or by calling `Tree::checkpoint()` explicitly.
- **`AllocOutcome` shrunk to `{ slot }`**; the `byte_offset` /
  `size` fields were dead.
- **`ExtentAllocOutcome` shrunk to `{ byte_offset }`**; the
  `aligned_size` field was dead.
- **`encode_record` returns `()` instead of `Result<()>`** — it
  has no fallible step (purely buffer append + CRC). Callers
  drop the `?` / `.unwrap()`.
- **`BufferManager::capacity()` and `BufferManager::clear()`
  removed.** Both were dead code; `BufferManager` itself is no
  longer public anyway.

### Breaking

- **`TreeConfig::flush_on_write` renamed to
  `memory_flush_on_write`** — the field is meaningless under the
  persistent backend (the WAL+BufferManager dirty-set pair already
  decides when bytes hit the backend); leaving the v0.1 name in
  place made it look like a per-write fsync knob it never was.
  Callers using `TreeBuilder::flush_on_write(b)` switch to
  `TreeBuilder::memory_flush_on_write(b)`; the field is a no-op
  on persistent trees.
- **`Error::NodeCorrupt` carries optional `blob_guid` + `slot`
  fields** — construct via `Error::node_corrupt(ctx)` and enrich
  via `.with_blob_guid(g)` / `.with_slot(s)`. Pattern-matching
  call sites must spread the new fields (`NodeCorrupt { context,
  .. }`); the buffer manager + walker cross-blob arms attach the
  GUID automatically.

### Added — errors

- **`Error::Internal(&'static str)`** — new variant for
  invariant-violation paths that were previously surfacing as
  `Error::NotYetImplemented` (now reserved for genuine
  walker-arm feature gaps like degenerate inline-prefix
  `BlobNode` splits). Five defensive guards in
  `checkpoint::round` migrated. Non-breaking thanks to
  `Error`'s `#[non_exhaustive]` marker.

### Fixed — durability (W2D-strict)

- **Writer ↔ background-checkpoint W2D race** (`round.rs`) — the
  pending-delete snapshot now happens inside the same `wal.lock`
  critical section as `snapshot_dirty` and `wal.flush`. Previously
  a writer could land a fresh blob into `dirty` between
  `snapshot_dirty` and `snapshot_pending_deletes`, then the round
  would observe a pending-delete for that blob without seeing
  its WAL record, opening a tiny W2D inversion window.
- **Checkpoint error paths no longer drop drained state.** Both
  `Tree::checkpoint` and the background round now restore every
  snapshot they drained on every error return:
  - WAL flush failure under `wal.lock` restores both `dirty` and
    `pending` (previously `Tree::checkpoint`'s `w.flush()?`
    propagated without restore, so the next round saw
    `dirty == 0 && pending == 0` and could truncate the WAL,
    losing the cache mutations the failed flush had just
    drained).
  - I/O worker channel-closed / pre-delete Sync failure in the
    bg round now restores `pending` on every early-return path
    (previously these returns drained `pending` and never
    restored it, losing unlink intent).
- **Abort-on-dirty-failure gate before pending-delete.** When any
  `write_through` at phase 2 fails, the round still runs the
  pre-delete `backend.flush` to fsync the writes that *did*
  succeed, but skips phase 5 entirely and restores the whole
  pending snapshot. Previously the round flushed the partial
  parent write, then applied the dependent child's manifest
  delete — a crash there would leave the on-disk parent
  referencing a slot that was no longer in the manifest, so
  WAL replay's walker descent through the `BlobNode` crossing
  would fail. The new gate keeps parent + child consistent
  across the failure window: the next round retries the parent
  write and only then processes its child's deletion.
- **`scan.rs::refresh_blob_node_pointers` inline `bm.commit`** —
  replaced with `bm.mark_dirty(parent_guid, STRUCTURAL_SEQ)` so
  the post-compact pointer repair stages through the unified
  dirty-set protocol. The previous inline commit pushed the cache
  image (possibly containing unflushed user-write effects on the
  parent) straight to backend, re-opening the W2D hole that
  `Tree::compact` phase 1/2 had just closed.
- **`Tree::compact` is documented `NOT online-safe`** — running
  concurrently with reads or writes can torn-read across
  `BlobNode` crossings. Pause user traffic before calling. The
  v0.3 maintenance latch will lift this restriction.

### Added — metrics export

- **`metrics` feature flag** (off by default, zero-cost when off).
  Enables `holt::metrics::render_prometheus(&stats) -> String`
  which emits Prometheus text format covering blob / space /
  dirty / pending-delete / cache hit-miss / optimistic-restart /
  checkpointer counters. Pure Rust, no extra deps. Gauge / counter
  naming follows Prometheus conventions — gauges drop the
  `_total` suffix (`holt_slots`, `holt_tombstones`,
  `holt_compactions`), counters keep it.

### Added — diagnostics

- **`Tree::scan_prefix(p)`** — one-line wrapper for
  `tree.range().prefix(p)`.
- **Range-iter tombstone fix** — `RangeIter::next_inner` now
  skips tombstone leaves in the same `advance_to_next_leaf` loop
  rather than emitting them and relying on the caller to filter.
  Caught by the new tombstone property test.

### Added — benchmarks

- **Group B — scale curve** (`kv_scale_get` / `kv_scale_put`).
  Criterion-parameterized over `{ 20 000, 100 000, 500 000 }`
  keys. The 500 k tier (~48 MB payload) exceeds the default
  32 MB buffer pool so real cache-miss + cross-blob descent
  becomes visible. Bench results: holt still ~2× faster than
  RocksDB / SQLite on `get` at every tier; on `put`, RocksDB's
  LSM amortizes flat (~1 500 ns), holt grows 2.4× (365 → 867 ns)
  but stays ahead.
- **Group C — p95/p99 latency under maintenance interference**
  (`tests/bench_contention_p95.rs`, `#[ignore]`). Uses
  `hdrhistogram` to track every `put`'s latency under 4 writer
  threads + a 5 ms-cadence background checkpointer + concurrent
  `tree.compact()` calls. Reports mean / p50 / p95 / p99 / p99.9
  / max + sustained throughput. Sample result on M3 Pro: 307 k
  ops/s sustained, p50 = 2 µs, p99 = 108 µs, max = 30 ms
  (compact-call worst case).
- **Removed Group A** (`*_durable_put` with
  `wal_sync_on_commit = true`) — each engine's "sync=true" knob
  maps to a different macOS syscall (`F_FULLFSYNC` vs
  `F_BARRIERFSYNC` vs lazy fsync), so the numbers measured
  drive-cache flush latency for some engines and OS-page-cache
  flushes for others. Not apples-to-apples; removed.

### Polish

- **PGO build profile docs** (`PGO.md`) — two-stage
  `cargo pgo build` → `cargo pgo optimize build` walkthrough,
  plus the workload-shape table for when PGO helps vs when it
  doesn't (helps on CPU-bound point lookup; rounding error on
  WAL-fsync-bound or blob-memcpy-bound workloads).

### Performance

- **SIMD Node48 / Node256 range-iter scans** — two new primitives
  in `engine::simd`:
  - `find_next_nonzero_byte(bytes, start)` for the
    `Node48::index[256]` lex-order walk
  - `find_next_nonzero_u32(words, start)` for the
    `Node256::children[256]` slot-index array
  Wired into `range::next_inner_child_from` and `Node16` arm of
  `range::find_inner_child_and_cursor` (which now reuses the
  existing `node16_find_byte` SIMD). On a Node48 with sparse
  children the saved scan cost is ~80 ns per `next()`; on
  Node256, ~120 ns. Worth most on `*_list_dir` queries that walk
  many branch nodes.
- **Hardware-accelerated CRC32** — the WAL record footer hash
  now routes through `crc32fast`. Auto-detects PCLMULQDQ on
  x86_64 and the AArch64 CRC32 instruction at runtime; falls
  back to slice-by-16 on older / non-x86 cores. Drops per-record
  CRC cost from ~110 ns to ~20 ns on supported hardware. v0.1's
  256-entry table + byte-at-a-time loop is gone.
- **Cached `Tree.root_pin`** (commit `a6f5c78`) — every
  `get` / `put` / `delete` keeps the root pinned via
  `Arc<CachedBlob>` and skips the BM `Mutex<HashMap>` lookup on the
  root hop. ≈300 ns / op on the hot path.
- **`RangeIter` delimiter fast-forward** (commit `861dba9`) — after
  emitting a `CommonPrefix(C)`, ascend the descent stack past `C`'s
  subtree instead of scanning every leaf under it to dedup.
  `*_list_dir` is now `O(distinct_rollups)`.

### Changed — concurrency

- **Sharded `BufferManager` cache** — v0.1's
  `Mutex<HashMap<BlobGuid, _>>` + `VecDeque<BlobGuid>` inline LRU
  is replaced by `DashMap<BlobGuid, Arc<CachedBlob>>`. `pin` /
  `get_cached` on different blobs hit different shards instead of
  contending on a single mutex.
- **Tick-based inline overflow eviction** — `try_evict_lru` walks
  the cache for the entry with the lowest `last_touched` tick
  whose `Arc::strong_count == 1` instead of using the
  front-of-deque order. Same primitive as the bg eviction sweep.

### Added — observability

- **`tracing` feature flag** (off by default). When enabled,
  the rare-but-important events fire structured `tracing` calls
  with named fields:
  - `holt::checkpoint` — `info!("round complete", dirty_snapshot,
    blobs_flushed, blobs_failed, merged, truncated_wal,
    elapsed_us)`
  - `holt::engine::spillover` — `debug!` on each fresh child blob
  - `holt::engine::merge` — `debug!` on each child folded into parent
  - `holt::engine::compact` — `debug!` on each in-place rebuild
  - `holt::wal` — `info!` on truncate
  - `holt::checkpoint::eviction` — `debug!` on each non-empty sweep

  All call sites are `#[cfg(feature = "tracing")]`-gated so users
  who don't enable the feature pay zero runtime cost.

- **`Tree::stats` extended** with bg-checkpointer + dirty-set
  + cache + concurrency counters:
  - `TreeStats::bm_dirty_count` — current count of unflushed blobs
  - `TreeStats::bm_pending_delete_count` — pending backend deletes
    queued by `delete_blob`
  - `TreeStats::bm_cache_hits` / `bm_cache_misses` — cumulative
    counts since `Tree::open`
  - `TreeStats::bm_optimistic_restarts` — count of optimistic-read
    walker restarts (load-bearing signal for latch contention)
  - `TreeStats::checkpointer: Option<CheckpointerStats>` — when bg
    is running, returns cumulative `rounds_attempted` /
    `rounds_succeeded` / `blobs_flushed` / `merges_total` /
    `truncates` / `evictions` counters.

  `CheckpointerStats` is re-exported at the crate root for
  convenience.

- **Silent observability reads** — `BufferManager::pin_silent` /
  `get_cached_silent` and `walker::scan::collect_blob_guids_silent`
  do not bump `cache_hits` / `cache_misses` and do not refresh
  the LRU `last_touched` tick. `Tree::stats` uses them so a
  metrics scrape doesn't pollute the counters it's about to
  report (observer effect).

### Added — I/O backend

- **`io-uring` feature flag** (Linux only). When enabled,
  `PersistentBackend::{read_blob, write_blob}` route through a
  single per-backend `io_uring` ring instead of `pread`/`pwrite`.
  Default builds + macOS / other Unix builds are unaffected.
  Behind a `Mutex<UringContext>` so concurrent callers serialise
  on the submission queue; with the new I/O worker thread the
  lock is uncontended on the hot path.

### Added — durability + background work

- **Background checkpointer** (`pub(crate) mod checkpoint` + opt-in
  `CheckpointConfig`). One thread, round-driven, parked between
  rounds via `park_timeout(idle_interval)`. Each round:
  (1) folds mergeable child blobs back into parents
  (`engine::try_merge_children`),
  (2) snapshots the `BufferManager` dirty set,
  (3) flushes the WAL writer (`sync_data`),
  (4) commits each snapshotted blob to backend,
  (5) `fdatasync`s the backend,
  (6) atomically truncates the WAL when `dirty_count == 0` under
  the WAL lock.
  `Drop` joins the bg thread then runs one final synchronous round
  on the calling thread to close the window between the last bg
  round and Tree shutdown.
- **`BufferManager` dirty-tracking** (`mark_dirty` / `snapshot_dirty`
  / `restore_dirty` / `min_unflushed_txn`). Per-blob lowest unflushed
  WAL seq, drained atomically via `mem::take` so concurrent writers'
  `mark_dirty` calls land in the fresh empty map. `commit` drains
  on success and restores on failure. The WAL trim watermark falls
  out as `min(dirty.values()) − 1`.
- **`CheckpointConfig`** + **`TreeBuilder::checkpoint(cfg)`** —
  user-facing opt-in for the background thread. Default is
  `enabled = false`; flipping it on flips on `auto_merge = true` by
  default. `idle_interval` / `dirty_blob_threshold` knobs match the
  fjall / sled flusher conventions.
- **`idle_interval` default 100 ms** (down from 200 ms) — based on
  the `bench_checkpoint_sweep` integration measurement: 100 ms
  cuts paced peak WAL by 4× vs 200 ms with no measurable writer
  overhead. Tighter intervals see diminishing returns; looser
  intervals leak WAL bytes between rounds.

### Changed — public surface

- **`engine` + `concurrency` are now `pub(crate)`** (commit
  `3cfa80f`). Their public types (`RangeBuilder` / `RangeEntry` /
  `RangeIter`) are now curated by new `api::range` and `api::stats`
  re-export modules; the top-level `holt::*` flat surface is
  unchanged from a user's perspective.
- **`api::stats`** is the canonical home for `BlobStats` /
  `TreeStats` (moved here from `api::tree`).
- **`lib.rs` re-exports grouped + commented** (Core / Range / Stats /
  Txn / Backend / Checkpoint sections).

### Removed — dead code surfaced by the lockdown

- `HybridLatch::try_upgrade`, `Guard::{state, upgrade_to_shared,
  upgrade_to_exclusive}`, `LatchMode` — no callers post-lockdown.
- `engine::walker::types::CompactStats` — `compact_blob` now returns
  `Result<()>`. Test sites that read the stats counters read
  `space_used` straight off the frame header instead.

### Internal

- Drop dead `journal/checkpoint.rs` stub and `engine/compact.rs`
  shim; fold `CompactReason` into `journal::txn_op`.
- Move the cross-blob descent unit test from `tests/tree_smoke.rs`
  into the walker's internal `tests` module — the
  `make_blob_from_node` primitive it pokes is now crate-private.

## [0.1.0] — 2026-05-19

First crates.io release. The v0.1 cycle was "build the engine
end-to-end" — all algorithmic + API surfaces below are landed:
ART core, multi-blob `splitBlob` / `mergeBlob` / `compactBlob`,
persistent backend (Linux `O_DIRECT` + macOS `F_NOCACHE`),
physiological WAL with batched transactions, S3-style range
iteration with delimiter rollup. 203 tests on Ubuntu + macOS
CI. See [`ROADMAP.md`](ROADMAP.md) for what's queued for v0.2.

### Added — algorithm core

- **9-NodeType ART layout** with compile-time-asserted byte offsets
  (`Leaf` 16 B, `Prefix` 128 B, `Blob` 128 B, `Node4` 24 B, `Node16`
  88 B, `Node48` 456 B, `Node256` 1032 B, `EmptyRoot` 8 B, `Invalid`).
- **4 KB `BlobHeader`** with bit-packed `SlotEntry` (`ntype << 17 |
  offset / 8`); 10 240-slot table per 512 KB blob.
- **Recursive walker**: `insert` / `lookup` / `erase` / `rename` —
  every arm cross-blob via `BlobNode` crossings.
- **`splitBlob` auto-spillover** on `OutOfSpace`; victim heuristic
  picks the largest non-`Blob` subtree under the root's first
  branching node.
- **`compactBlob` in-place repack** via deep-clone-into-scratch +
  memcpy-back; paired with `splitBlob` on every retry so churn
  workloads (insert + delete + reinsert) stay in fewer blobs.
- **`make_blob_from_node` deep-clone primitive** + `free_subtree`
  recursive slot reclaim.
- **`mergeBlob` inverse of splitBlob** — `engine::merge_blob`
  inlines a child blob's subtree back into its parent at the
  `BlobNode` slot, preserves the BlobNode's inline prefix as a
  `Prefix` chain over the inlined root, and deletes the child
  blob. `engine::is_mergeable` guards the fold (combined data
  area + slot count fit, child has no nested crossings, no
  tombstones). `engine::try_merge_children` walks a parent's
  tree and folds every direct mergeable `BlobNode` child.
  `Tree::compact` runs it after the per-blob compact pass —
  heavy-erase workloads collapse multi-blob trees back toward a
  single root.
- **`refresh_blob_node_pointers` post-compact invariant repair**
  — `compact_blob` rebuilds a child's `header.root_slot` in
  isolation, breaking the lock-step
  `BlobNode.child_entry_ptr == child.header.root_slot`
  invariant that insert / erase keep inline.
  `Tree::compact` runs `refresh_blob_node_pointers` between the
  per-blob compact pass and the merge pass to walk every
  `BlobNode` crossing and re-point it at the child's current
  root slot.
- **`SPILLOVER_RESERVATION = 128 B`** bump-area headroom so
  `spillover_blob` always has room to allocate its emergency
  `BlobNode` placeholder.
- **Cross-type free-list fallback** (`Prefix` ↔ `Blob`, both 128 B).
- **Erase-time node shrink** (Node256 → 48 → 16 → 4) with hysteresis
  thresholds 37 / 12 / 3.
- **`Node4 → Prefix([byte])` lone-child collapse** preserves descent-
  depth invariants when an inner node empties to a single child.
- **Strict-prefix support** via a Tree-layer terminator byte.
- **In-place leaf-value update on same-size writes** — zero
  allocator activity when an update fits the existing extent.
- **SIMD Node16 byte search** (SSE2 + NEON + scalar fallback).
- **SIMD `longest_common_prefix`** (SSE2 + NEON + scalar) for
  leaf-split / prefix-split hot paths.

### Added — concurrency

- **3-mode `HybridLatch`** (LeanStore: optimistic / shared /
  exclusive) wired into `CachedBlob` over an
  `UnsafeCell<AlignedBlobBuf>`.
- **Wait-free `Tree::get`** — walks every blob under an optimistic
  snapshot, restarts from the root on a torn read. No Tree-wide
  reader lock.
- **No Tree-wide writer mutex** — `put` / `delete` serialise on the
  root blob's per-blob exclusive latch; mutations on disjoint child
  blobs proceed in parallel. `rename` keeps a small `rename_lock`
  scoped to its multi-step atomicity.

### Added — buffer manager

- **`BufferManager`** — LRU-bounded blob cache wrapping any
  `Backend`, transparent (itself implements `Backend`).
  `TreeConfig::buffer_pool_size` (default 64) sets capacity.
- **`pin` / `commit` API** with the three-guard family
  (`OptimisticGuard`, `BlobReadGuard`, `BlobWriteGuard`) — pin-and-
  operate for zero-copy reads and writes against the cached buffer.

### Added — persistence

- **`MemoryBackend`** for in-memory trees and tests.
- **`PersistentBackend`** — single packed `blobs.dat` + atomic-
  rename `manifest.bin`; `O_DIRECT` on Linux, `F_NOCACHE` on macOS.
- **`AlignedBlobBuf`** — 4 KB-aligned heap buffer required by
  `O_DIRECT`.

### Added — WAL (Stage 5a-5e)

- **`TxnOp` record codec** — 10 variants (`Insert` / `Erase` /
  `Split` / `Merge` / `Compact` / `RenameObject` / `Rename` /
  `NewTree` / `RmTree` / `MemMarker`) encoded as
  `MAGIC | LEN | SEQ | TY | BODY | CRC32`.
- **CRC32 (table-driven, IEEE 802.3)** with a 256-entry compile-
  time table — ≈1.5 GB/s, ~110 ns per typical 175-byte record.
- **`WalWriter`** — append-only file with `sync_data`-on-flush
  durability boundary + 64 KB group-commit auto-flush.
- **`replay()` forward scanner** — torn-tail-tolerant; real mid-
  file corruption surfaces as `Error::ReplaySanityFailed` with the
  bad record's byte offset.
- **Tree ↔ WAL integration** — `put` / `delete` / `rename` emit
  ops; `Tree::open` replays onto the BM-cached blob; `Tree::checkpoint`
  flushes WAL + commits BM + atomically truncates the WAL.
- **Reference-based `WalWriter::append_insert` / `append_erase` /
  `append_rename_object`** fast paths — skip the three `Vec` clones
  the `TxnOp` enum's owned-data shape would force.
- **`TxnOp::Batch` + `TY_BATCH = 10`** — single WAL record carrying
  N primitive ops (`Insert` / `Erase` / `RenameObject` today);
  nested batches are rejected at encode + decode. Inner ops share
  the outer record's CRC and derive their seqs from
  `outer_seq + index` via a contiguous WAL-level seq reservation.
  Replay (`journal::reader::replay_bytes`) transparently flattens
  a `Batch` into per-inner callbacks so existing consumers don't
  need a new arm.

### Added — public API

- **`Tree::open(TreeConfig)`** — single entry point;
  `TreeConfig::new(dir)` opens persistent (default),
  `TreeConfig::memory()` is volatile.
- **`Tree::put / get / delete / rename`** — bytes-in, bytes-out.
- **`Tree::checkpoint`** — flush WAL + commit BM + truncate WAL.
- **`Tree::range()` stateful iterator** — `RangeBuilder` /
  `RangeIter` / `RangeEntry`. Builder chains `.prefix(p)`
  (anchored descent, no full-tree scan), `.start_after(k)`
  (strict-greater lower bound), `.delimiter(b)` (S3-style rollup
  with `CommonPrefix` dedup). Walks transparently across `BlobNode`
  crossings. Forward-only. Best-effort snapshot — writers can
  interleave between `next()` calls; mirrors the upstream
  `fa_iter` contract extracted from binary log strings.
- **`Tree::txn(|batch| { ... })`** — closure-based batched
  transaction. [`TxnBatch`] buffers `put` / `delete` / `rename`;
  on closure return, holt takes `rename_lock`, applies each op in
  order, then emits one `TxnOp::Batch` WAL record. Crash atomicity:
  on recovery the whole batch is either replayed or lost. Runtime
  isolation is best-effort — concurrent `put` / `delete` on
  disjoint blobs are not blocked, so the contract is "crash-
  atomic, not serialisable." Mid-batch failure (e.g., rename
  `NotFound`) leaves earlier ops applied to the BM but skips the
  WAL emit; documented on the method.
- **`TreeConfig::wal_sync_on_commit`** — opt-in per-op WAL fsync
  (default `false`, matching RocksDB's `sync=false` baseline).
- **`TreeBuilder`** — chainable config (`memory()`,
  `buffer_pool_size(n)`, `wal_sync_on_commit(bool)`,
  `checkpoint_byte_interval(b)`).
- **Typed `Error`** — `BackendIo` / `Alloc` / `Free` /
  `KeyTooLong` / `ValueTooLong` / `NotYetImplemented` /
  `NodeCorrupt` / `ReplaySanityFailed` / `NotFound` / `DstExists`.

### Added — tests / benches / examples

- **202 tests**: 117 unit + 51 tree_smoke + 15 wal_round_trip + 12
  wal_tree_integration + 2 property-based + 5 layout-invariants.
- **Property-based tests** (`proptest`) — random put / delete /
  rename traces cross-checked against a `HashMap` oracle in memory
  mode + crash-and-replay in persistent mode.
- **Criterion benchmarks** vs RocksDB across three workload shapes
  (`kv` / `objstore` / `fs`) × three ops (get / put / mixed) ×
  two storage modes (memory / persistent) = 18 microbenchmarks.
  See [`benches/README.md`](benches/README.md) for headline
  numbers.
- **Four examples**: `basic_kv`, `filesystem_meta`, `session_store`,
  `s3_metadata`. Each `cargo run --example` prints golden output.

### Added — tooling / project polish

- **GitHub Actions CI** — matrix of ubuntu + macOS × build / test /
  doctest + lint (`cargo fmt --check`, `cargo clippy -D warnings`)
  + docs (`cargo doc -D warnings`) + MSRV (1.82) build.
- **Platform scope locked**: holt is Unix-only by design. Building
  on Windows fires a top-of-crate `compile_error!`; the persistent
  backend's `O_DIRECT` (Linux) / `F_NOCACHE` (macOS) fast path has
  no Windows analog worth carrying.
- **Zero clippy / rustdoc warnings** under `-D warnings`. The
  curated `#![allow]` block in `src/lib.rs` lists the
  `clippy::pedantic` lints we've reviewed and judged either
  intentional or noise.
- **`CONTRIBUTING.md`** / **`CODE_OF_CONDUCT.md`** / this changelog.

### Notes

- The crate is pinned to MSRV **1.82**.
- License: MIT.
- v0.2 will add the 3-thread async checkpointer, `io_uring` backend
  (Linux), SIMD CRC32, and the buffer-pool tuning knobs.
