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

The `objstore` + `fs` scenarios additionally run **range-scan**
operations — the dominant access pattern for real metadata
workloads (`readdir`, S3 `ListObjects`):

- `*_list` — prefix-anchored range scan, `take(100)` entries
- `*_list_dir` — S3-style delimiter rollup, take 8 distinct
  `CommonPrefix` entries (holt does the dedup in the engine;
  RocksDB + SQLite get the same logic done at the bench's app
  layer, since neither has a native `?delimiter=` API)

`N_KEYS = 20 000` — large enough that the data spreads across
**multiple holt blobs** (~6–8 × 512 KB), so the bench exercises
`BlobNode` crossings + cross-blob spillover/compact retries, not
just single-blob descent.

## Running

```sh
# Full sweep (~7 minutes — 24 bench groups × 3 engines × ~5s/measure):
cargo bench --bench main

# Quick smoke pass (~1 minute):
cargo bench --bench main -- --quick --noplot

# A single scenario:
cargo bench --bench main -- kv_get

# Just the range scans (the load-bearing metadata-engine test):
cargo bench --bench main -- _list
```

HTML reports land in `target/criterion/`.

## Methodology — apples-to-apples

Two comparison modes, each with all three engines tuned to the
same durability profile:

### Memory / no-WAL mode (`*_get` / `*_put` / `*_mixed`)

Engine algorithm cost only — durability disabled across the board:

- **holt**: `TreeConfig::memory()` with `flush_on_write = false`.
  Mutations stay in the BufferManager-pinned blobs.
- **RocksDB**: temp-dir DB, `disable_wal = true`, `sync = false`,
  64 MB memtable, compression disabled.
- **SQLite**: `:memory:` DB, `journal_mode=MEMORY`,
  `synchronous=OFF`, 64 MB page cache, `WITHOUT ROWID` schema.

### Persistent mode (`*_persist_get` / `*_persist_put` / `*_persist_mixed`)

All three engines disk-backed with WAL on, per-op durability to
the OS page cache (not fsync) — the "you survive a process
crash, not a power failure" mode high-throughput services target:

- **holt**: `TreeConfig::new(tempdir)` (PersistentBackend with
  `F_NOCACHE` on macOS / `O_DIRECT` on Linux). Every `put` /
  `delete` / `rename` emits a `TxnOp` to the WAL writer;
  `wal_sync_on_commit` stays at its default `false`. Blobs only
  hit disk at spillover or `Tree::checkpoint`.
- **RocksDB**: temp-dir DB, `disable_wal = false`, `sync = false`.
  Each `put` appends to the WAL (buffered) plus the memtable.
- **SQLite**: file-backed DB, `journal_mode=WAL`,
  `synchronous=NORMAL`, 64 MB page cache.

Shared settings: 20 000 unique keys preloaded; bench iterates a
seeded permutation of that set; `cargo bench` builds with
`lto="thin"`, `codegen-units=1`, `opt-level=3`; single-threaded.

## How to read the numbers

The `objstore` + `fs` scenarios are the **right** test for what
holt is designed to do. The `kv` scenario is the **wrong** test,
included on purpose — it tells you how badly an ART degrades when
the workload violates its assumptions.

| Scenario | What it actually measures | Expected outcome |
|---|---|---|
| `kv` (random 32-byte keys) | ART without prefix sharing or locality | holt loses — every lookup chases fresh nodes across multiple blobs; B+tree (LMDB), LSM (RocksDB), and B-tree (SQLite) all win |
| `objstore` (path keys) | ART on hierarchical keys, ~30-byte shared prefix | holt wins on point lookup + range scan |
| `fs` (POSIX paths) | Same, with very long common prefix | holt wins biggest on point lookup; close to even on range scan |

Pick the engine that matches your **key shape**. holt is for
hierarchical, prefix-rich keys; if your keys are random bytes
(hashes, UUIDs without a path prefix), reach for RocksDB / SQLite.

### Sample numbers — Apple M-series, `cargo bench --quick`

These are local-laptop numbers; re-run on your hardware before
quoting them. The **relative ordering** is what's load-bearing.

**Point lookup (memory mode), 32-byte key, 64-byte value, N=20 000:**

| Scenario | holt | RocksDB | SQLite | holt vs best other |
|---|---|---|---|---|
| `kv_get` (random key) | ~17 µs | ~720 ns | ~580 ns | **30× slower** (anti-pattern) |
| `objstore_get` (path) | ~190 ns | ~554 ns | ~534 ns | **~2.8× faster** |
| `fs_get` (path) | ~196 ns | ~625 ns | ~538 ns | **~2.7× faster** |

**Range scan (memory mode), `take(100)` under an anchored prefix:**

| Scenario | holt | RocksDB | SQLite | holt vs best other |
|---|---|---|---|---|
| `objstore_list` (`bucket-05/`, ~625 leaves) | ~10.7 µs | ~17.7 µs | ~13.2 µs | **~1.23× faster** |
| `fs_list` (`/usr/local/share/category-5/`, ~1250 leaves) | ~10.7 µs | ~18.9 µs | ~13.4 µs | **~1.25× faster** |

**S3-style delim rollup (memory mode), `take(8)` distinct
`CommonPrefix` entries — naive O(N) scan in all three engines
(no fast-forward; on the v0.2 backlog):**

| Scenario | holt | RocksDB | SQLite | Notes |
|---|---|---|---|---|
| `objstore_list_dir` (8 of 32 buckets) | ~579 µs | ~673 µs | **~463 µs** | SQLite wins on raw scan throughput; holt mid-pack |
| `fs_list_dir` (8 of 16 dirs) | ~1.18 ms | ~1.33 ms | **~940 µs** | Same shape — SQLite's tight B-tree scan beats holt's leaf walk |

**Reading the LIST numbers:** plain prefix scans (`*_list`) are
the bread-and-butter metadata workload — `readdir`, `ListObjects`
with deep prefix — and holt wins those cleanly. The delimiter
rollup (`*_list_dir`) is a fairness check: every engine has to
scan all leaves under the prefix to dedup the rollups (none of
the three fast-forwards past a rolled-up subtree). holt's per-leaf
overhead from `BlobFrameRef` allocation + `RangeEntry::Key { ... }`
materialisation costs it the win here; SQLite's tight C-loop wins
on raw scan throughput. holt's qualitative advantage — built-in
`CommonPrefix` semantics so callers don't reimplement dedup —
isn't captured by the bench numbers.

## Caveats

1. **Single-threaded.** Per-blob `HybridLatch` makes reads
   wait-free; concurrent-read throughput scales with cores, but
   this bench measures single-thread latency. A multi-reader
   stress is on the bench backlog.
2. **No fsync.** Both modes set `sync=off`-equivalent — durable
   to OS page cache only. A real `fsync`-per-op workload is
   fsync-bound (~1–3 ms on consumer SSD) and overwhelms every
   engine's algorithm cost.
3. **Delim rollup is currently O(all leaves under prefix)** in
   all three engines. holt v0.2 has fast-forward (skip past a
   rolled-up subtree after emitting the `CommonPrefix`) on the
   backlog; the same trick is available to RocksDB/SQLite via
   `seek(common_prefix + 0xff)` but the bench's app-layer dedup
   doesn't implement it either. Once holt gets fast-forward,
   `*_list_dir` should drop by a factor of `leaves_per_rollup`.
4. **Bench numbers are machine-dependent.** Don't take any
   absolute throughput claim from this README at face value —
   re-run on your hardware. The relative ordering (holt wins on
   path-shaped point lookup + plain list, loses on random-kv +
   close on delim list) is the load-bearing observation.

This bench is the right comparison for **metadata-engine
workloads** with bounded per-tree dataset and hierarchical keys —
directory listings, S3 metadata, inode tables, AI artefact
catalogs. It is not the right comparison for "100M-key analytics
datastore" workloads or "random UUID hot-path" workloads.
