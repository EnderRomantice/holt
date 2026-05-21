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

- `*_list` — marker-aware prefix range scan, `take(100)` entries
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
```

HTML criterion reports land in `target/criterion/`.

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

### Hot persistent mode (`*_persist_get` / `*_persist_put` / `*_persist_mixed`)

All three engines disk-backed with WAL on, per-op durability to
the OS page cache (not fsync) — the "you survive a process
crash, not a power failure" mode high-throughput services target.
The service is warm: the Holt BufferManager, RocksDB cache/memtable,
and SQLite page cache may all contain data touched during preload
or Criterion warmup. This is a foreground WAL/cache benchmark, not
a cold data-file I/O benchmark:

- **holt**: `TreeConfig::new(tempdir)` (FileBlobStore with
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

## Results

This README defines the workload surface and methodology only.
Concrete release numbers live in [`RESULTS.md`](RESULTS.md), so
there is one source of truth for quoted performance data.

When reading those results, keep the profiles separate:

- Memory/no-WAL rows isolate ART/data-structure and
  metadata-operation semantics.
- Hot persistent rows use disk-backed engines with WAL enabled and
  no per-op fsync.
- The current public harness has persistent rows for point
  get/put/mixed and plain prefix list. `metadata_mix`,
  create/delete, rename, delimiter `list_dir`, and the scale curve
  are memory/no-WAL unless the bench name says `persist`.

Plain prefix scans (`*_list`) model `readdir` / `ListObjects` with
a bounded prefix range. Delimiter rollup (`*_list_dir`) is the
S3-style listing test: Holt's `Tree::range` emits
`RangeEntry::CommonPrefix` inside the engine and fast-forwards past
the rolled-up subtree; RocksDB and SQLite use generic ordered
iteration plus app-layer dedup because neither exposes a native
delimiter-list API.

## Caveats

1. **Single-threaded latency, not throughput.** Point reads use
   optimistic per-blob latching; range scans use shared guards plus
   versioned cursor validation. The public benchmark surface
   measures single-thread latency, not multi-core throughput.
2. **No fsync.** Both modes set `sync=off`-equivalent — durable
   to OS page cache only. A real `fsync`-per-op workload is
   fsync-bound and overwhelms every engine's algorithm cost.
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
5. **Range is restart-on-conflict, not MVCC.** `Tree::range`
   stores blob versions in its cursor path and seeks from the last
   emitted lower bound if a writer invalidates that path. A long
   scan can still observe keys committed after iterator creation if
   they sort after the current cursor.

This bench is the right comparison for **metadata-engine
workloads** with bounded per-tree dataset and hierarchical keys —
directory listings, S3 metadata, inode tables, AI artefact
catalogs. It is not the right comparison for "100M-key analytics
datastore" workloads or "random UUID hot-path" workloads.
