# holt — benchmark results

End-to-end criterion micro-benches comparing holt against
**RocksDB** (`rocksdb` crate, default-features-off + bundled
`librocksdb-sys`) and **SQLite** (`rusqlite` with the
`bundled` libsqlite3, so contributors don't need a system
SQLite installation). Three workload shapes (KV, S3
object-store metadata, POSIX filesystem metadata) ×
{ memory, persistent } × { get, put, mixed, list, list-delim }.

## Reproducing

```bash
# Full suite (~5 min on M3 Pro).
cargo bench --bench main -- --output-format bencher

# One group only — e.g. just KV.
cargo bench --bench main -- kv_ --output-format bencher
```

Each criterion sample is one op. Numbers are mean ± noise band
in nanoseconds; lower is better. Holt's per-op numbers are
randomised over a 10 000-key dataset (see `gen_*_dataset`);
RocksDB / SQLite are driven by the same dataset for fair
comparison.

## Test environment

- **Hardware**: Apple M3 Pro (12 cores), 36 GB RAM
- **OS**: macOS 26.3 (Darwin 25.0.0)
- **Rust**: 1.94.0 stable, release profile (`lto=thin`,
  `codegen-units=1`, `opt-level=3`)
- **holt**: commit `63b181d` (v0.2 release-class — `wal.lock`
  W2D protocol, sharded BufferManager, 3-thread bg
  checkpointer, SIMD CRC32 + node scans).
- **RocksDB**: 0.24 (`librocksdb-sys` 0.18, bundled)
- **SQLite**: rusqlite 0.39 (bundled libsqlite3)
- **Knob alignment**: all three engines use comparable
  "per-op durable to OS page cache, not fsync'd" semantics —
  see the durability matrix at the top of `benches/main.rs`.

## Headline numbers

24 baseline benches across KV / objstore / fs shapes, memory
+ persistent variants. **Holt wins all 24** vs RocksDB and
SQLite. Margin range: 1.3× (in-memory fs_put vs SQLite — both
short codepaths) to **467×** (`fs_list_dir` S3-style rollup
vs RocksDB — fast-forward over `BlobNode` crossings beats
seek-iterator-per-leaf hands down).

Plus the scale curve (Group B below) running across `{20 k,
100 k, 500 k, 2 M}` keys: **holt wins every cell at every
tier**, with the lead vs RocksDB on get widening to **5.6× at
2 M** as the LSM's read-amplification finally bites.

## KV workload (short random keys + short values)

| Bench               | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| ------------------- | --------: | -----------: | ----------: | ---------: | --------: |
| **memory** get      |  **169**  |          684 |         567 |       4.0× |      3.4× |
| **memory** put      |  **344**  |        1 201 |         629 |       3.5× |      1.8× |
| **memory** mixed    |  **351**  |        2 138 |         663 |       6.1× |      1.9× |
| **persist** get     |  **187**  |          637 |       1 508 |       3.4× |      8.1× |
| **persist** put     |  **473**  |        3 470 |       2 310 |       7.3× |      4.9× |
| **persist** mixed   |  **328**  |        3 294 |       1 951 |      10.0× |      5.9× |

## Object-store workload (S3-shaped path keys + metadata values)

| Bench                       | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| --------------------------- | --------: | -----------: | ----------: | ---------: | --------: |
| **memory** get              |  **250**  |          702 |         622 |       2.8× |      2.5× |
| **memory** put              |  **481**  |        1 441 |         664 |       3.0× |      1.4× |
| **memory** mixed            |  **377**  |        2 152 |         663 |       5.7× |      1.8× |
| **memory** list             |  **10 808** |     16 815 |      16 637 |       1.6× |      1.5× |
| **persist** get             |  **247**  |          740 |       1 508 |       3.0× |      6.1× |
| **persist** put             |  **567**  |        3 499 |       2 319 |       6.2× |      4.1× |
| **persist** mixed           |  **420**  |        3 264 |       1 954 |       7.8× |      4.7× |
| **persist** list            |  **10 651** |     16 937 |      17 801 |       1.6× |      1.7× |
| **list_dir** (S3 rollup)    |  **2 463** |    624 672 |     436 204 |     **254×** |  **177×** |

## Filesystem-metadata workload (inode + dirent path keys)

| Bench                | Holt (ns) | RocksDB (ns) |  SQLite (ns) | vs RocksDB | vs SQLite |
| -------------------- | --------: | -----------: | -----------: | ---------: | --------: |
| **memory** get       |  **239**  |          700 |          630 |       2.9× |      2.6× |
| **memory** put       |  **488**  |        1 452 |          660 |       3.0× |      1.4× |
| **memory** mixed     |  **372**  |        2 469 |          668 |       6.6× |      1.8× |
| **memory** list      |  **10 854** |    17 887 |       16 775 |       1.6× |      1.5× |
| **persist** get      |  **251**  |          701 |        1 516 |       2.8× |      6.0× |
| **persist** put      |  **555**  |        3 456 |        2 292 |       6.2× |      4.1× |
| **persist** mixed    |  **411**  |        3 165 |        1 961 |       7.7× |      4.8× |
| **persist** list     |  **11 111** |    17 842 |       17 727 |       1.6× |      1.6× |
| **list_dir**         |  **2 812** |  1 317 457 |      917 245 |     **468×** |  **326×** |

## Note on `wal_sync_on_commit=true`

A previous draft tried to bench all three engines at the
"flip the strongest fsync knob" tier. The result wasn't a
fair comparison: each engine's "sync=true" knob actually
maps to a different syscall on macOS (`F_FULLFSYNC` vs
`F_BARRIERFSYNC` vs just `write()`+lazy-fsync), so we ended
up measuring drive-cache flush latency for some engines and
kernel-page-cache flushes for others. The numbers said more
about the platform than the engines, so that bench group was
removed. The numbers above (`*_persist_put`) are the honest
"per-op durable to OS page cache, not fsync'd" tier, which
all three engines actually do reach with comparable
semantics.

## Workload notes

- **`*_get` / `*_put`**: 10 000-key dataset, randomly sampled
  with `StdRng(seed=SEED)`. Pre-load happens once outside the
  measured region.
- **`*_mixed`**: 80 % gets, 20 % puts, same dataset.
- **`*_list`** (plain): prefix narrows to ~625 keys
  (`objstore`) / ~1 250 keys (`fs`); each criterion sample
  iterates up to 100 results.
- **`*_list_dir`** (S3-style rollup): prefix + delimiter `/`;
  emits 32 (`objstore`) / 16 (`fs`) `CommonPrefix` entries per
  pass, then stops. Holt's iterator's fast-forward — ascend
  past each rollup's subtree — turns the walk from
  `O(leaves_under_prefix)` into `O(distinct_rollups)`. RocksDB
  + SQLite both scan every leaf and dedupe in the host loop,
  which is what the 100–500× gap measures.

## Group B — Scale curve (20 k → 100 k → 500 k → 2 M keys)

Parameterized `kv_get` and `kv_put` over four dataset sizes so
the comparison is not biased by the "everything fits in L2 cache"
effect at 20 k. The 500 k tier (~48 MB) already exceeds holt's
default 32 MB (64-blob) buffer pool; the **2 M tier (~192 MB) is
6× the pool**, so every miss pays the full `read_blob` + descent
cost.

```bash
cargo bench --bench main -- kv_scale --output-format bencher
```

### `kv_scale_get` (random point lookup)

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **172** |          609 |         557 |       3.5× |      3.2× |
| **100 k** |   **336** |          938 |         812 |       2.8× |      2.4× |
| **500 k** |   **645** |        1 598 |       1 156 |       2.5× |      1.8× |
|  **2 M**  | **1 030** |    **5 771** |       1 425 |   **5.6×** |      1.4× |

### `kv_scale_put` (random point upsert)

| n        | Holt (ns) | RocksDB (ns) | SQLite (ns) | vs RocksDB | vs SQLite |
| -------- | --------: | -----------: | ----------: | ---------: | --------: |
|  **20 k** |   **323** |        1 170 |         589 |       3.6× |      1.8× |
| **100 k** |   **578** |        1 312 |         879 |       2.3× |      1.5× |
| **500 k** |   **971** |        1 665 |       1 278 |       1.7× |      1.3× |
|  **2 M**  | **1 435** |        1 607 |       1 549 |       1.1× |      1.1× |

### Observations

- **Holt still wins every cell at 2 M.** No regression on the
  largest tier we test.
- **Get-latency spikes for RocksDB at 2 M** (1 598 → 5 771 ns,
  3.6× jump from 500 k). That's the LSM read-amplification cost
  finally showing up — at 2 M the bloom filters miss more often
  and the read has to descend multiple SST levels. Holt only
  grows 1.6× (645 → 1 030 ns) over the same 4× data growth
  because its descent depth scales with `log(N)` of distinct
  prefixes, not with SST level count. Result: holt's **lead vs
  RocksDB on get widens to 5.6× at 2 M** (up from 2.5× at 500 k).
- **SQLite get tightens to 1.4×** at 2 M (from 1.8× at 500 k).
  Its B-tree handles cache pressure gracefully — bounded fan-out
  + 64 MB page cache means lookup depth is dominated by index
  height, which grows slowly.
- **Holt put gap narrows to ~1.1× over both** at 2 M. Holt grows
  4.4× across 100× data growth (323 → 1 435 ns) — pure
  eviction-churn cost on cross-blob descent. RocksDB grows just
  1.4× (1 170 → 1 607 ns) because the LSM write path is bounded
  by WAL append + memtable insert, not by key count or working
  set. SQLite grows 2.6× (589 → 1 549 ns). The takeaway: at
  cache-busting working-set sizes, LSM-style amortization
  catches up; holt is still ahead, but the lead is the closest
  it gets at any tier.

## Group C — p95 / p99 latency under maintenance interference

`tests/bench_contention_p95.rs` runs four `put` writers + a
background checkpointer (5 ms cadence) + a compaction thread
that periodically calls `tree.compact()` — the worst-case
"engine is doing maintenance while users keep writing"
shape. Every `put` records its wall-clock latency to a
`hdrhistogram` for percentile reporting.

```bash
cargo test --release --test bench_contention_p95 \
    -- --ignored --nocapture
```

### Result (20-second window, 4 writers + bg checkpoint + compact)

| Metric           | Value         |
| ---------------- | ------------: |
| ops              |   6 152 095   |
| throughput       |   306 918 ops/s |
| **mean**         |     12.79 µs  |
| **p50**          |      1.96 µs  |
| **p95**          |     28.54 µs  |
| **p99**          |    107.58 µs  |
| p99.9            |   2 310.14 µs |
| max              |  30 654.46 µs |

### Observations

- **307 k ops/s sustained** with 4 writer threads + a
  background checkpointer + concurrent `compact()`. Each
  writer averages ~77 k ops/s on its own, so the wal-lock
  serialization tax is modest.
- **p50 ≈ 2 µs** — most puts hit only the common "walker
  mutate + mark_dirty + wal.append + flush" critical section
  with no maintenance interference.
- **p99 ≈ 100 µs** — tail dominated by the wal.lock
  serialization point during checkpoint snapshots (rounds run
  every ~5 ms and briefly take the lock to drain dirty +
  pending_deletes + flush WAL).
- **p99.9 ≈ 2 ms** and **max ≈ 30 ms** — the spikes are
  `compact()` calls themselves (which take the wal.lock for
  the duration of phase 1 / 1.5 / 2 since `compact` is not
  yet online — see the docstring on `Tree::compact`). These
  bound the worst case under maintenance; the v0.3 maintenance
  latch will reduce them further by serializing compact
  against writers more cleanly.

The mean-vs-p50 gap (12.8 µs mean vs 2 µs p50) reflects that
the slow tail (compact calls hit a handful of writes hard) is
real but bounded — the distribution isn't long-tailed enough
to perturb the median.
