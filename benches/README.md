# Benchmarks

Criterion-based microbenchmarks comparing **artisan** against
**RocksDB** across three realistic shapes of metadata workload.

## Scenarios

| Group | Key shape | Value shape | Models |
|---|---|---|---|
| `kv` | 32-byte random | 64-byte random | Anonymous KV baseline |
| `objstore` | `bucket-NN/path/sub/file-NNNN.bin` | `{"size":...,"etag":...,"class":"STD"}` (~60 B fixed) | S3-style object metadata |
| `fs` | `/usr/local/share/category-N/file-NNNN` | 32-byte packed inode (size + mtime + mode + uid + gid + nlink) | POSIX filesystem metadata |

Each scenario runs three workloads:

- `*_get` — random key lookup over a pre-loaded dataset
- `*_put` — random key replacement (in-place update)
- `*_mixed` — 50% get / 50% put, key chosen at random

## Running

```sh
# Full sweep (~3 minutes — criterion's default 5s/benchmark × 18 benches):
cargo bench --bench main

# Quick smoke pass (~1 minute):
cargo bench --bench main -- --quick --noplot

# A single scenario:
cargo bench --bench main -- kv_get
```

HTML reports land in `target/criterion/`.

## Methodology — apples-to-apples

Both engines run in their "no-WAL, batched-flush" configuration so
the comparison isolates engine throughput rather than durability
overhead:

- **artisan**: `TreeConfig::memory()` with `flush_on_write = false`.
  Mutations stay in the in-memory cached root blob; `checkpoint()`
  would flush through the backend.
- **RocksDB**: temp-dir database, `disable_wal = true`,
  `sync = false`, 64 MB memtable, compression disabled.

Other shared settings:

- 2000 unique keys preloaded; bench iterates over a random
  permutation of that set
- Seeded RNG → reproducible across runs
- `cargo bench` builds with `lto="thin"`, `codegen-units=1`,
  `opt-level=3`
- Single-threaded

## Sample results

Apple M-series laptop, `cargo bench --bench main -- --quick`:

| Scenario | Op | artisan | RocksDB | artisan / RocksDB |
|---|---|---|---|---|
| `kv` | get | **11.0 Melem/s** | 1.92 Melem/s | **5.7×** |
| `kv` | put | **6.44 Melem/s** | 1.13 Melem/s | **5.7×** |
| `kv` | mixed | **7.94 Melem/s** | 1.21 Melem/s | **6.5×** |
| `objstore` | get | **7.78 Melem/s** | 1.99 Melem/s | **3.9×** |
| `objstore` | put | **3.94 Melem/s** | 1.19 Melem/s | **3.3×** |
| `objstore` | mixed | **5.18 Melem/s** | 1.27 Melem/s | **4.1×** |
| `fs` | get | **7.98 Melem/s** | 1.79 Melem/s | **4.5×** |
| `fs` | put | **3.81 Melem/s** | 1.15 Melem/s | **3.3×** |
| `fs` | mixed | **5.05 Melem/s** | 1.10 Melem/s | **4.6×** |

(Per-op latency: get ≈ 88–128 ns; put ≈ 155–263 ns. RocksDB
roughly 500–900 ns per op across the board.)

### Why artisan wins on this shape

- **The whole tree fits in L2.** 200–250 KB of leaves + internal
  nodes for 2000 keys; the cached root blob is a single 512 KB
  buffer and stays hot. RocksDB's memtable adds skiplist
  pointer-chasing overhead.
- **SIMD Node16 lookup.** SSE2 / NEON `pcmpeqb`+`movemask` reduces
  the medium-fan-out byte-search to ~3 instructions.
- **In-place update on same-size values.** When the new value fits
  inside the existing leaf extent (very common — `objstore` /
  `fs` workloads pin value length, `kv` uses 64 B everywhere),
  artisan rewrites the bytes in place. Zero allocator activity,
  zero extent leak.

## Caveats — honest read

artisan's current implementation has several constraints that
matter once you go bigger or want durability:

1. **Single-blob cap (~512 KB).** Multi-blob auto-spillover lands
   in Stage 2d phase B. Until then, the working set must fit in
   one blob; the benchmark deliberately stays well inside that
   limit (~250 KB).
2. **No WAL.** Both engines run with WAL disabled for fairness.
   Once Stage 5 (WAL) lands, artisan will pay an fsync per commit
   on the persistent backend — RocksDB's design is more efficient
   under that constraint.
3. **Single Mutex for reads.** Reads still serialise behind the
   write lock. Stage 5+6 per-blob `HybridLatch` will allow
   concurrent optimistic reads.
4. **Small dataset.** 2000 keys is intentionally inside L2.
   RocksDB's strengths (block cache, bloom filters, compression)
   show at much larger sizes.

This bench is the right comparison for **metadata-engine
workloads** where the per-tree dataset is bounded — directory
listings, S3 metadata, inode tables, AI artefact catalogs. It is
not the right comparison for "100M-key analytics datastore"
workloads; that's RocksDB's home turf.
