# Benchmarks

Criterion-based microbenchmarks comparing **holt** against
**RocksDB** and **SQLite** across three shapes of metadata
workload — `kv` (anti-pattern baseline), `objstore`, and `fs`
(holt's design target).

## Scenarios

| Group | Key shape | Value shape | Models |
|---|---|---|---|
| `kv` | 32-byte random | 64-byte random | Anonymous KV baseline — **pessimal for ART** (no prefix sharing, no key locality). |
| `objstore` | `bucket-NN/path/sub/file-NNNN.bin` | `{"size":...,"etag":...,"class":"STD"}` (~60 B fixed) | S3-style object metadata. |
| `fs` | `/usr/local/share/category-N/file-NNNN` | 32-byte packed inode (size + mtime + mode + uid + gid + nlink) | POSIX filesystem metadata. |

Each scenario runs three point-access operations:

- `*_get` — random key lookup over a pre-loaded dataset
- `*_put` — random key replacement (in-place update)
- `*_mixed` — 50% get / 50% put, key chosen at random

The `objstore` + `fs` scenarios additionally run
**metadata-native** operations — the common operations that a
metadata engine actually serves beyond blind point overwrite:

- `*_list` — prefix-anchored range scan, `take(100)` entries
- `*_list_dir` — S3-style delimiter rollup, take 8 distinct
  `CommonPrefix` entries (holt does the dedup in the engine;
  RocksDB + SQLite get the same logic done at the bench's app
  layer, since neither has a native `?delimiter=` API)
- `*_create_delete` — create a scratch metadata entry, then
  delete it to keep the benchmark state bounded
- `*_rename` — atomic rename round-trip. Holt uses `Tree::rename`;
  RocksDB uses `WriteBatch`; SQLite uses an explicit transaction.
- `*_metadata_mix` — weighted objstore/fs metadata mix:
  45% stat/get, 20% metadata update, 10% plain list, 10%
  delimiter list-dir, 10% create+delete, 5% rename round-trip.

`N_KEYS = 20 000` for the baseline scenarios — large enough that
the data spreads across **multiple holt blobs** (~6–8 × 512 KB),
so the bench exercises `BlobNode` crossings + cross-blob
spillover/compact retries, not just single-blob descent.

A second group — **scale curve** (`kv_scale_get` / `kv_scale_put`)
— parameterizes over `{ 20 000, 100 000, 500 000, 2 000 000 }`
keys. The 500 k tier (~48 MB payload) already exceeds the
default 32 MB buffer pool; the 2 M tier is the large-tree
pressure case used to judge path-put scalability.

A third group — **p95/p99 latency under maintenance interference**
— lives in `tests/bench_contention_p95.rs` (not criterion;
criterion measures means, not percentiles). Run via
`cargo test --release --test bench_contention_p95 -- --ignored --nocapture`.
It spins 4 writer threads + a 5 ms-cadence background
checkpointer + concurrent `Tree::compact()` calls triggered by a
put counter, and tracks every `put` latency via `hdrhistogram`.

A fourth probe — **large-tree shape quality** — lives in
`tests/bench_large_tree_shape.rs`. It is a holt-only regression
bench for skewed prefixes, hot directories, delete-heavy churn,
and working sets larger than a tiny buffer pool. It prints
`blob_count`, space/gap/tombstone totals, spillovers, merges, and
average/max blob hops so split-policy changes can be judged before
running the full RocksDB/SQLite comparator sweep.

A fifth probe — **manifest/checkpoint pressure** — lives in
`tests/bench_manifest_checkpoint.rs`. It repeatedly inserts
path-shaped keys, deletes most of each round, compacts, and
checkpoints. It reports checkpoint percentiles plus
`manifest.bin`, `manifest.log`, WAL, and data-file sizes so the
append-only manifest path can be tracked separately from point
lookup/insert microbenches.

A sixth probe — **WAL/checkpoint fast paths** — lives in
`tests/bench_wal_checkpoint.rs`. It separates clean foreground
checkpoints, durable group-commit reuse, default non-durable
checkpoint barriers, and background idle rounds. The timing table
is paired with journal `syncs` / checkpointer `truncates`
counters, so regressions show up even when data-file flush cost
dominates wall-clock latency.

A seventh probe — **2M path-put shape** — lives in
`tests/bench_path_put_2m.rs`. It is holt-only and focuses on the
large-tree objstore/fs put path, printing update latency together
with blob count, average/max blob hops, max cross-blob depth, and
spillovers. Use it before/after CPU hot-path changes; use
Criterion `_scale_put` for RocksDB/SQLite comparisons.

## Running

```sh
# Full criterion sweep (~5 min on M3 Pro):
cargo bench --bench main

# Quick smoke pass (~1 minute):
cargo bench --bench main -- --quick --noplot

# Scale curve only (Group B):
cargo bench --bench main -- kv_scale

# A single scenario:
cargo bench --bench main -- kv_get

# Just the range scans (the load-bearing metadata-engine test):
cargo bench --bench main -- _list

# Just the metadata-native mutation/mix groups:
cargo bench --bench main -- _create_delete
cargo bench --bench main -- _rename
cargo bench --bench main -- _metadata_mix

# p95/p99 under bg checkpoint + compact interference (Group C):
cargo test --release --test bench_contention_p95 \
    -- --ignored --nocapture

# Large-tree shape probe (holt only):
cargo test --release --test bench_large_tree_shape \
    -- --ignored --nocapture

# Short shape smoke:
HOLT_SHAPE_BENCH_KEYS=5000 \
cargo test --release --test bench_large_tree_shape \
    -- --ignored --nocapture

# Manifest/checkpoint pressure:
cargo test --release --test bench_manifest_checkpoint \
    -- --ignored --nocapture

# Short manifest/checkpoint smoke:
HOLT_MANIFEST_BENCH_ROUNDS=3 \
HOLT_MANIFEST_BENCH_KEYS_PER_ROUND=1000 \
cargo test --release --test bench_manifest_checkpoint \
    -- --ignored --nocapture

# WAL/checkpoint fast paths:
cargo test --release --test bench_wal_checkpoint \
    -- --ignored --nocapture

# Short WAL/checkpoint smoke:
HOLT_WAL_BENCH_CLEAN_ITERS=50 \
HOLT_WAL_BENCH_MUTATIONS=10 \
cargo test --release --test bench_wal_checkpoint \
    -- --ignored --nocapture

# Holt-only 2M path-put shape probe:
cargo test --release --test bench_path_put_2m \
    -- --ignored --nocapture

# Short 2M path-put smoke:
HOLT_PATH_PUT_KEYS=20000 \
HOLT_PATH_PUT_UPDATES=5000 \
cargo test --release --test bench_path_put_2m \
    -- --ignored --nocapture
```

HTML criterion reports land in `target/criterion/`. The
percentile bench prints its histogram table to stdout.

## Methodology — apples-to-apples

Two comparison modes, each with all three engines tuned to the
same durability profile:

### Memory / no-WAL mode (`*_get` / `*_put` / `*_mixed`)

Engine algorithm cost only — durability disabled across the board:

- **holt**: `TreeConfig::memory()` with `memory_flush_on_write =
  false`. Mutations stay in the BufferManager-pinned blobs.
- **RocksDB**: temp-dir DB, `disable_wal = true`, `sync = false`,
  64 MB memtable, compression disabled.
- **SQLite**: `:memory:` DB, `journal_mode=MEMORY`,
  `synchronous=OFF`, 64 MB page cache, `WITHOUT ROWID` schema.

### Persistent mode (`*_persist_get` / `*_persist_put` / `*_persist_mixed`)

All three engines disk-backed with WAL on, per-op durability to
the OS page cache (not fsync) — the "you survive a process
crash, not a power failure" mode high-throughput services target:

- **holt**: `TreeConfig::new(tempdir)` (PersistentBackend with
  `F_NOCACHE` on macOS / `O_DIRECT` on Linux). Every mutation
  submits an encoded record to the journal worker;
  `wal_sync_on_commit` stays at its default `false`. Blobs only
  hit disk at checkpoint.
- **RocksDB**: temp-dir DB, `disable_wal = false`, `sync = false`.
  Each `put` appends to the WAL (buffered) plus the memtable.
- **SQLite**: file-backed DB, `journal_mode=WAL`,
  `synchronous=NORMAL`, 64 MB page cache.

Shared settings: 20 000 unique keys preloaded; bench iterates a
seeded permutation of that set; `cargo bench` builds with
`lto="thin"`, `codegen-units=1`, `opt-level=3`; single-threaded.

### Metadata-native groups

`*_create_delete`, `*_rename`, and `*_metadata_mix` currently run
in the memory/no-WAL profile. They are meant to isolate operation
semantics and data-structure cost:

- create/delete is a bounded create+unlink pair, not a growing
  insert-only workload.
- rename is held to atomic move semantics for every engine.
- metadata_mix is deliberately heterogeneous; one iteration is
  one sampled metadata operation, and the operation mix is fixed
  by seed and percentage buckets.

## How to read the numbers

The `objstore` + `fs` scenarios are the **right** test for what
holt is designed to do. The `kv` scenario is the **wrong** test,
included on purpose — it tells you how badly an ART degrades when
the workload violates its assumptions.

| Scenario | What it actually measures | Expected outcome |
|---|---|---|
| `kv` (random 32-byte keys) | ART without prefix sharing or metadata semantics | anti-pattern baseline; useful mainly for checking constants and scale |
| `objstore` (path keys) | ART on hierarchical keys, plus S3 list/rename/create semantics | holt should win most clearly on list_dir and metadata_mix |
| `fs` (POSIX paths) | Long common prefixes, directory list, rename/create/delete | holt should win most clearly on directory/list-heavy mixes |

Pick the engine that matches your **key shape**. holt is for
hierarchical, prefix-rich keys; if your keys are random bytes
(hashes, UUIDs without a path prefix), reach for RocksDB / SQLite.

### Sample numbers — Apple M-series, `cargo bench --quick`

These are rough `--quick` numbers for orientation; **full-suite
results — including the scale curve and the p95/p99 contention
bench — live in [`RESULTS.md`](RESULTS.md)**, which is what to
quote. Either way, re-run on your hardware before quoting; the
**relative ordering** is what's load-bearing.

**Point lookup (memory mode), N=20 000:**

| Scenario | holt | RocksDB | SQLite | holt vs best other |
|---|---:|---:|---:|---:|
| `kv_get` (random key) | ~170 ns | ~680 ns | ~570 ns | **~3.4× faster** |
| `objstore_get` (path) | ~250 ns | ~700 ns | ~620 ns | **~2.5× faster** |
| `fs_get` (path) | ~240 ns | ~700 ns | ~630 ns | **~2.6× faster** |

**Range scan (memory mode), `take(100)` under an anchored prefix:**

| Scenario | holt | RocksDB | SQLite | holt vs best other |
|---|---|---|---|---|
| `objstore_list` (`bucket-05/`, ~625 leaves) | ~10.7 µs | ~17.7 µs | ~13.2 µs | **~1.23× faster** |
| `fs_list` (`/usr/local/share/category-5/`, ~1250 leaves) | ~10.7 µs | ~18.9 µs | ~13.4 µs | **~1.25× faster** |

**S3-style delim rollup (memory mode), `take(8)` distinct
`CommonPrefix` entries:**

| Scenario | holt | RocksDB | SQLite | holt vs best other |
|---|---|---|---|---|
| `objstore_list_dir` (8 of 32 buckets) | **~2.5 µs** | ~623 µs | ~440 µs | **~177× faster** |
| `fs_list_dir` (8 of 16 dirs) | **~2.85 µs** | ~1.31 ms | ~928 µs | **~326× faster** |

**Metadata-native operation mix (memory mode, quick smoke):**

| Scenario | holt | RocksDB | SQLite | holt vs best other |
|---|---:|---:|---:|---:|
| `objstore_create_delete` | ~211 ns | ~1.06 µs | ~2.84 µs | **~5.0× faster** |
| `objstore_rename` | ~1.12 µs | ~4.45 µs | ~11.41 µs | **~4.0× faster** |
| `objstore_metadata_mix` | ~1.44 µs | ~70.39 µs | ~49.05 µs | **~34× faster** |
| `fs_create_delete` | ~406 ns | ~1.15 µs | ~2.86 µs | **~2.8× faster** |
| `fs_rename` | ~1.54 µs | ~4.58 µs | ~11.34 µs | **~3.0× faster** |
| `fs_metadata_mix` | ~1.48 µs | ~136.84 µs | ~99.17 µs | **~67× faster** |

**Reading the LIST numbers:** plain prefix scans (`*_list`) are
the bread-and-butter metadata workload — `readdir`, `ListObjects`
with deep prefix — and holt wins those cleanly. The delimiter
rollup (`*_list_dir`) is the load-bearing test for S3-style
listings: holt's `Tree::range` does engine-level `CommonPrefix`
dedup **and** fast-forwards past a rolled-up subtree once it's
emitted, so the cost is `O(distinct_rollups)` rather than
`O(leaves_under_prefix)`. RocksDB and SQLite have no equivalent
API, so the bench rolls dedup at the app layer; even with a
tight inner loop they still pay the full leaf-scan cost. v0.2
fast-forward dropped `*_list_dir` from ~600 µs / ~1.3 ms down
to single µs.

## Caveats

1. **Single-threaded latency, not throughput.** Per-blob
   `HybridLatch` makes reads wait-free; concurrent-read
   throughput scales with cores, but the criterion bench measures
   single-thread latency. For concurrent-read throughput see
   `tests/bench_multi_reader.rs` (sample numbers on M-series:
   1 → 5.67 M ops/s, 4 → 14.73 M ops/s, 16 → 19.06 M ops/s). For
   tail-latency under maintenance interference see
   `tests/bench_contention_p95.rs`.
2. **No fsync.** Both modes set `sync=off`-equivalent — durable
   to OS page cache only. A real `fsync`-per-op workload is
   fsync-bound (~1–3 ms on consumer SSD) and overwhelms every
   engine's algorithm cost.
3. **Delim rollup uses fast-forward in holt only.** Holt's
   `Tree::range` ascends the descent stack past a rolled-up
   subtree after emitting its `CommonPrefix`, so the cost is
   `O(distinct_rollups)`. RocksDB and SQLite still do the naive
   `O(leaves_under_prefix)` scan with app-side dedup; both
   could implement an equivalent `seek(common_prefix + 0xff)`
   skip, but the bench's app-layer dedup doesn't.
4. **Bench numbers are machine-dependent.** Don't take any
   absolute throughput claim from this README at face value —
   re-run on your hardware. The relative ordering (holt wins on
   path-shaped point lookup, metadata-native mixes, and
   delimiter rollup; point put is a smaller win at large scale) is
   the load-bearing observation.

This bench is the right comparison for **metadata-engine
workloads** with bounded per-tree dataset and hierarchical keys —
directory listings, S3 metadata, inode tables, AI artefact
catalogs. It is not the right comparison for "100M-key analytics
datastore" workloads or "random UUID hot-path" workloads.
