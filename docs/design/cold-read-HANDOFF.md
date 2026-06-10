# Cold-read fundamental fix — session handoff

Hand this to the next session. Everything below is grounded in committed code +
measurements; no re-derivation needed.

## TL;DR — start here

- **Branch:** `perf/cold-read-observability` (clean tree).
- **Goal:** stop reading a whole 512 KB blob frame to answer one cold point
  lookup. Measured amplification today: **~529 KB read per cold point read.**
- **Approach (decided):** an **in-blob routing region** — at compaction, cluster
  a blob's internal nodes into a contiguous front region and page-align the
  leaves after it, so a cold read loads the small routing region + **one leaf
  page** (~8–12 KB, or ~4 KB with the routing region resident) and **reuses the
  existing descent**. Full design: `docs/design/cold-read-oracle.md`.
- **Done:** design + page-read primitive + header fields (stages 0–1).
- **Next:** **stage 2 — the two-arena compaction build in
  `src/engine/walker/migrate.rs`** (`clone_subtree` / `clone_leaf`), gated by a
  `routing == full` invariant test. This is the meaty, durability-critical part;
  give it a full session.
- **Validation cadence (unchanged):** correctness/compile on **mac (aarch64)**;
  real I/O + benches on **ubuntu (x86)** via rsync (see "Validation" below).

## Why (measured, don't re-measure)

Run the committed analysis any time:
```
cargo test --release -p holt cold_read_page_touch_ceiling -- --ignored --nocapture
```
objstore 300k keys / 48 B values / 225 blobs (~1333 keys/blob):
- A point-lookup descent touches **mean 4.64 distinct 4 KB pages (~18.6 KB), p95
  24 KB** — vs the 512 KB pin (~27× less cold I/O just by paging touched pages).
- **structure/value = 78% / 22%** at 48 B values. ⇒ "keep all *structure*
  resident" is NOT universal (for small values the structure *is* the data). The
  routing region keeps only the **internal nodes** resident-able (small), which
  is value-size-agnostic.

The `cold.idx` sidecar (current `b3a08ac` and below) is **not** the fundamental
fix: it caches `(key→value)` in a second, **unbounded, unaccounted** in-RAM table
(≈1 GB+ for 5 M keys) — a hit-rate play no better than enlarging the buffer pool,
useless when working set >> RAM, and it carries a class of crash/staleness bugs
(see "cold.idx review" below). The routing region is a **miss-cost** play and is
crash-safe by construction.

## What's committed (this session)

| commit | what it gives you |
|---|---|
| `137d5ba` | **Design doc** `docs/design/cold-read-oracle.md` — routing region layout, build, read path, crash/compat, 6-stage plan. |
| `808a5fa` | **Page-read primitive** `BlobStore::read_blob_range(guid, byte_offset, dst)`. FileBlobStore = positional O_DIRECT/F_NOCACHE `pread` (4 KB-aligned, bypasses the 512 KB io_uring ring); Memory = RAM copy; default = read-whole-and-copy. **Dual-arch validated** (`range_read_test::page_reads_reconstruct_each_blob`: page-reads reconstruct every real blob byte-for-byte; x86 O_DIRECT no EINVAL). Also the `cold_read_page_touch_ceiling` analysis in `cold.rs`. |
| `12ce05a` | **Stage 1 — header fields (transparent).** `BlobHeader` gains `routing_off/routing_len/leaf_region_start` (u32, at 0xb0/b4/b8, carved from pad; size still 4096; offset asserts extended). `BlobHeader::routing_region() -> Option<RoutingRegion>` (None ⇒ legacy whole-frame). **Safety:** `BlobFrameMut::init` zeroes the whole frame ⇒ every old/not-yet-recompacted blob reads `routing_len==0` ⇒ full-pin fallback; **no manifest bump needed.** Pinned by `header::tests::zeroed_header_is_legacy_layout`. The reader is `#[allow(dead_code)]` until stage 3. |

(The WAL ring work — the other big effort — is on a **separate branch**
(`perf/u16-children`) and is unrelated to this cold-read line; don't conflate.)

## Remaining plan (stages 2–6) with concrete entry points

### Stage 2 — two-arena compaction build  ← **START HERE**
**Files:** `src/engine/walker/migrate.rs` (`clone_subtree`, `clone_leaf`,
`compact_blob`), `src/engine/walker/spillover.rs` (`install_new_blob`).
- `clone_subtree` already DFS-walks the source in key order. Make it write into
  **two cursors**: internal nodes (root, `Prefix`, `Node4/16/48/256`, `BlobNode`)
  → routing arena starting at `DATA_AREA_START`; leaves (`[16B hdr][key][value]`)
  → leaf arena, **page-aligned**, after the routing arena. Child offsets are
  back-patched exactly as today (R1 offset_div8 addressing unchanged; offsets
  just land in two zones).
- Set `header.routing_off = DATA_AREA_START`, `routing_len = <internal bytes>`,
  `leaf_region_start = <page-aligned start of leaf arena>`.
- **Invariant the build must guarantee:** every offset `< leaf_region_start` is
  an internal node; every offset `>= leaf_region_start` is a leaf. (This is what
  lets the cold descent tell "internal vs leaf" from the offset without reading
  the node.)
- **Gate (write it first):** a `routing == full` test — build a blob, then assert
  the key set + values obtained by a routing-aware descent equal a full-frame
  descent (and a BTreeMap oracle). Add to proptest.
- Watch: routing region must fit (≤ ~2–3 pages typ.); if a blob's internals
  exceed a budget, leave `routing_len=0` (full-pin fallback) for that blob.
- Spillover (`install_new_blob`) writes fresh blobs too — apply the same layout
  there, or leave spillover blobs legacy and let the next compaction route them.

### Stage 3 — cold routed read
**File:** `src/engine/walker/lookup.rs` — `cold_lookup_or_pin` (currently ~line
356; the `ColdBlobLookup::Unknown` arm at the non-resident fallback does
`bm.pin(child_guid)` = the 512 KB read). Add `cold_read_routed`:
1. `header.routing_region()` is `None` ⇒ keep the full pin (legacy).
2. Else `read_blob_range(guid, routing_off, …)` the routing region (1–2 pages),
   wrap `[header ++ routing region]`, run the **existing descent**.
3. When the descent reaches a child offset `>= leaf_region_start`:
   `read_blob_range` that one leaf page (two if the leaf straddles / value > 4 KB
   — `value_len` is known), read `[hdr][key][value]`, compare the full key (with
   terminator), return `Found{value,seq}` / `NotFound`. `BlobNode` ⇒ recurse the
   crossing loop.
- **DATA-INTEGRITY GATE:** `routed_get(key) == tree.get(key)` for ≥100k random
  keys incl. **absent** and **crossing** keys. A wrong `NotFound` = silent data
  loss. Dual-arch + cold `bm_read_bytes` drop bench (target ~512 KB → ~8–12 KB).

### Stage 4 — bounded resident routing cache
Keep routing regions hot in a **bounded, accounted** cache (~15–30 MB for 5 M
keys, vs cold.idx's 1 GB+). Cold read → 1 leaf pread. Account it in/alongside the
BM pool budget (do NOT repeat cold.idx's unbounded sin).

### Stage 5 — remove `cold.idx`
The routing region subsumes the sidecar. Delete `src/store/blob_store/file/
cold_index.rs` + the `cold_lookup_blob` sidecar path + `summarize_blob_for_cold_
index` + the manifest generation field if only the sidecar used it. **This
deletes the entire cold.idx bug class** (below). Gate: full suite + the SIGKILL
crash-soak (`cargo run --release --example wal_crash_soak -- 40`).

### Stage 6 — per-blob bloom (later)
A bloom in the header for free *within-prefix* negatives. Orthogonal/additive.

### Stage 3.5/4 addendum — push io_uring to the extreme (cold-read I/O backend)

**Today's io_uring is NOT optimized** (`src/store/blob_store/file/uring.rs`): one
ring behind a **global Mutex**, **synchronous `submit_and_wait(1)` per read** (no
read batching — only checkpoint writes batch via `pwrite_many_at`), **no SQPOLL,
no IOPOLL**. It captures only the *static* registration wins (`register_files` +
`register_buffers` → `ReadFixed`/`WriteFixed`, O_DIRECT). So the cold-read path
runs at **device queue depth 1** — exactly wrong for random reads over a working
set >> RAM. And `read_blob_range` (the page-granular primitive) currently
**bypasses the ring** (plain `read_exact_at`).

This is the right place to fix io_uring, because the only paths that touch disk
are cold misses + checkpoint writes; the warm read path (holt's 2.1–2.4× win)
has **no I/O**, so optimizing io_uring only pays on the cold path — the one this
work redesigns. Fold this into stages 3–4:

1. **Route page-granular cold reads through a batched-async read interface**, not
   plain `pread`. The BM cold path issues a *wave* of leaf-page reads (across
   concurrent queries and/or a single query's pages) as one batch.
2. **Linux backend — io_uring to the extreme:**
   - **Multi-SQE submit**: push N page-read SQEs, `submit_and_wait(N)` → device
     QD = N (the big lever for random cold reads). (Today reads are QD 1.)
   - **Per-core / small pool of rings** to drop the single global Mutex →
     concurrent submission from multiple threads.
   - Optional **IOPOLL** (NVMe busy-poll completions, lowest latency, burns a
     poller core) and **SQPOLL** (kernel-side submit, cuts the `io_uring_enter`
     syscall) behind config flags.
   - Keep the existing fixed files + fixed buffers + ordered batched writes.
3. **Cross-platform backend (macOS + Linux-without-io_uring): a small thread pool
   of blocking `pread`s** → the same device parallelism without io_uring. Do
   **NOT** use Darwin POSIX aio (`aio_read`/`lio_listio`) — it is libc
   thread-pool-emulated and weak. Keep `F_NOCACHE`.
4. **Interface**: extend `BlobStore` with a batched read (e.g.
   `read_pages_batch(&[(guid, page, dst)])` or a submit/poll pair), backed by
   io_uring (multi-SQE) on Linux and the thread pool elsewhere — one interface,
   two backends. macOS is dev/test (prod = Linux NVMe), so it needs *correct +
   parallel*, not *extreme*.
5. **Measure on a REAL cold bench** (ubuntu/x86): dataset >> RAM, **drop the OS
   page cache** (the current 137× is page-cache-warm and not representative),
   sweep **QD = 1 vs 8 / 32 / 64**, report cold p50/p99 + throughput; compare to
   RocksDB at matched block-cache bytes.

Sequencing: do this *after* stage 3 lands the routed read (so there is a real
page-read load to batch), as part of or right after stage 4 (resident routing
cache). Until then, single-op reads are fine.

## cold.idx safety review (why stage 5 deletes a bug class)

A multi-agent review of the cold.idx stack (`ae0c524..b3a08ac`) found (steady
state is sound — residency mutex + manifest-v5 generation are the load-bearing
guards — but the crash boundary + resource discipline have real holes). If
cold.idx is kept as an interim, these need fixing; the routing region avoids
them by construction:

1. **Crash-window generation aliasing (data-integrity):** cold.idx append isn't
   fsync'd and is fsync'd *after* manifest.log; a generation bump lost in a crash
   can be re-issued for different content, so a stale cold record can match the
   manifest generation → resurrected deleted keys / stale values after recovery.
   Cheap fix if kept: **truncate/delete cold.idx whenever reopen replays ≥1 WAL
   record.**
2. **Spurious `Err(NotFound)` on a live key:** `931e055` dropped the parent
   shared guard before resolving the child; a concurrent merge/erase unlinks the
   child between edge-validate and probe → `cold_lookup_or_pin`'s uncaught `?`
   surfaces `Err(BlobStoreIo NotFound)` from `get()`. Fix: hold the parent guard
   across `cold_lookup_or_pin`, or treat `is_blob_store_not_found` as
   restart-from-root.
3. **Unbounded table cache** (no eviction/accounting) — the 137× is "unbounded
   RAM vs 8 MB pool", holt-vs-holt, page-cache-warm (not real cold). Don't quote
   137× as structural/competitive.
4. **Torn-tail `cold.idx` replay** corrupts future opens (valid_len includes the
   orphan header). **Sidecar I/O errors fail authoritative ops / user gets**
   (violates "rebuildable, not source of truth"). `entry_of` miss → `Err` not
   `Unknown`.

## Key layout facts / gotchas

- Blob frame = **512 KB** (`PAGE_SIZE = 0x80000`, confusingly named). Pages = 4 KB.
  Layout: `[0,4KB)` BlobHeader (page 0); `[4KB,44KB)` slot table (40 KB, pages
  1–10, **off the read path since R1**); `[44KB,512KB)` data area (`DATA_AREA_
  START=0xB000`, pages 11–127).
- R1: children store `offset_div8` inline (`decode_child_off`/`child_offset`),
  not slot indices. R3 leaf = `[16B hdr: key_fp@0, node_type@1, value_len@2,
  key_len@4, tombstone@6, seq@8][key][value]`, inline in the blob. `cold.rs`'s
  `summarize_*` is the canonical node-walk template (reuse it).
- `BlobFrameMut::init` **zeroes the whole 512 KB** — the reason new header fields
  default safe.
- New header fields at 0xb0/b4/b8; `blob_guid` ends at 0xb0; size assert pins 4096.
- O_DIRECT (Linux) needs 4 KB-aligned offset+len+buffer; whole-page reads into a
  page-aligned slice of an `AlignedBlobBuf` satisfy it (proven on x86).

## Validation

- **mac (aarch64), local:** `cargo test --lib`, `cargo clippy --all-targets`,
  the on-disk suites (`wal_tree_integration`, `checkpoint`, `tree_smoke`).
- **ubuntu (x86), real I/O + O_DIRECT + io_uring + benches:**
  `export LIBCLANG_PATH=$HOME/libclang-shim` (rocksdb comparator needs a
  libclang shim: `ln -sf /usr/lib/llvm-18/lib/libclang.so.1
  ~/libclang-shim/libclang.so`), then
  `rsync -az --exclude target/ --exclude .git/ --exclude benches/target/ ./
  ubuntu:~/holt/` and run there.
- **Cold-read bench:** the stress bench supports `--no-default-features` Holt-only
  runs and `HOLT_STRESS_DROP_COLD_INDEX_AFTER_PRELOAD=1`. For a *true* cold
  number, also drop the OS page cache (the current 137× is page-cache-warm).
- **Gates:** stage 2 = `routing==full` invariant; stage 3 = `routed_get==tree.get`
  for present/absent/crossing (data-integrity); stage 5 = SIGKILL crash-soak.

## Tasks (mirror of the tracker)

- **#18 (in progress):** Cold-read in-blob routing region. Stage 1 done
  (`12ce05a`); primitive done (`808a5fa`); design (`137d5ba`). **Next: stage 2**
  two-arena compaction build + `routing==full` test → stage 3 `cold_read_routed`
  (+ data-integrity gate + cold bench) → **stage 3.5/4 io_uring-to-the-extreme
  cold-read backend** (batched multi-SQE async reads / per-core rings /
  IOPOLL+SQPOLL on Linux; thread-pool backend for macOS+fallback; QD-sweep cold
  bench) → stage 4 resident routing cache → stage 5 remove cold.idx (+ crash-soak)
  → stage 6 bloom.
- **#10 (pending):** R2 BlobNode prefix bloom — folds into stage 6.
- **#12 (pending):** hot-scan residual ~4% (separate, low priority).
