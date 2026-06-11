# Scale-curve findings (2M → 20M, 2026-06-11)

Measured to answer: *does Holt stay shape-stable as the namespace grows, and is the
resident route cache the thing that degrades at scale?* (ROADMAP v0.4 P0.)

**Headline: the route cache is NOT the bottleneck — that hypothesis was measured and
killed. The real scale signal is blob fill efficiency: blobs settle at ~38% fill (the
spillover floor, not the 70% target), nothing consolidates them, so the on-disk tree —
and therefore the resident-cache miss rate — is ~1.8× larger than a well-packed tree.**

## Method

`benches/stress` objstore workload, single thread, file-backed WAL + background
checkpoint, `COMPACT_AFTER_PRELOAD=true` (route engaged), buffer pool = 64 frames
(32 MB). macOS / aarch64. Structural metrics (blob count, fill, depth, hops, route
stats) are **platform-independent** and are the load-bearing part of this curve; the
absolute `get` latency is I/O-bound and `F_NOCACHE` on macOS — the real NVMe latency
comes from `scripts/ubuntu-validate.sh`.

## The curve

| keys | blobs | avg fill | underfilled | max_depth | avg_hops | merges | route_entries (cap 16384) | route miss% | get (mac, I/O-bound) | list_dir |
|-----:|------:|---------:|------------:|----------:|---------:|-------:|--------------------------:|------------:|---------------------:|---------:|
| 1M  |   513 | 0.588 |  256 / 513 | 2 | 1.22 | 0 |  256 | ~78%* | 452 ns† | 92 ns |
| 2M  |  1537 | 0.392 | 1280 /1537 | 2 | 1.61 | 0 |  546 | (write-path) | 389 µs | 308 ns |
| 8M  |  6400 | 0.376 | 5376 /6400 | 5‡ | 3.97‡ | 0 |  194 | 0.6% | 688 µs | 239 ns |
| 20M | 15890 | 0.379 |13841/15890 | 3 | 2.25 | 0 | 2231 | 12.9% | 575 µs | 81 ns |

\* the 1M route-miss is the *write-path* (insert) descent rate, not read-path.
† 1M `get` is near-cache-resident (513 blobs ≈ 8× the 64-frame pool); 2M+ exceeds the
  pool so even "hot" random reads are disk-bound — the 389/688/575 µs are cache-miss
  cold reads, not a CPU signal.
‡ the 8M depth/hops are inflated by an incomplete compaction-settle (hit the bench's
  `MAX_PASSES` cap); treat the 8M depth as a transient, not a clean scale point. Fill
  (~0.38) and blob count are robust across all three.

## What the data says

1. **Route cache: not the problem (hypothesis killed).** `route_entries` peaks at 2231
   against a 16384 capacity (≤14% used), and read-path hit rate is 87–99%. Scaling the
   route cache with namespace size — the thing I assumed would help — would do nothing.
   *Measure before fixing.*

2. **83–87% of blobs are below the 35% fill floor.** The spillover picker targets
   `SPILLOVER_TARGET_CHILD_FILL_PCT = 70%` with a `MIN = 35%` floor
   (`engine/walker/spillover.rs:33-34`); the stats "underfilled" bucket is fill < 35%
   (`SHAPE_UNDERFILLED_CHILD_FILL_PER_MILLE = 350`, `api/tree.rs:47`). At every scale
   ≥2M, **83% (2M), 84% (8M), 87% (20M) of all blobs are below that 35% floor** — the
   `avg_fill ≈ 0.38` is propped up only by the ~13–17% minority that are well-packed
   (`max_fill` 0.85–0.94), so the *median child blob* is worse than the average. This is
   present *before* routing (pre-route "ready" line is also ~0.39), so routing is
   exonerated — it is the spillover fragmentation pattern. Result: blob count is ~linear
   in keys but at **~2× the count a 70%-packed tree would have**.

3. **`merges = 0` everywhere — nothing consolidates the underfill.** The existing
   `mergeBlob` primitive inlines a *child blob back into its parent* (`is_mergeable`:
   combined space + slots fit, no nested crossings, no tombstones). In a wide, shallow
   tree of thousands of ~38%-full *sibling* leaf-blobs, the parent (root) is a fan-out of
   BlobNode crossings that cannot absorb a child's full subtree, so parent-child inline
   never applies. There is **no sibling-merge / rebalance primitive**, so underfilled
   siblings are never consolidated.

4. **Consequence for cold reads.** Fixed 64-frame (32 MB) pool vs a ~768 MB (2M) … ~8 GB
   (20M) tree means cache coverage → ~0 as N grows; that is inherent (you can't cache an
   8 GB tree). But the ~38% fill makes the tree **~1.8× bigger than necessary**, so the
   resident-cache miss rate — and the cold-read count — is ~1.8× worse than a packed tree
   would give. This is exactly why the **cold-read read-amplification work matters**: at
   these scales reads *are* disk-bound, and the routing region (≈12 KB read vs 512 KB
   frame pin) is what keeps each unavoidable cold read cheap. The cold-read stack and the
   fill-efficiency problem are the same scale-stability story from two ends.

5. **`list_dir` (delimiter rollup) is scale-stable and fast** (81–308 ns across the
   curve) — the S3-style rollup fast-forward is unaffected by blob count. Holt's list
   advantage holds at scale.

## The actual fix (v0.4 P0 — substantial, not a constant tweak)

The lever is **blob fill efficiency**, not the route cache. Two non-trivial options,
both correctness-sensitive (spillover + merge touch the structural-mutation path):

- **(a) Pack tighter on spillover.** Tune the picker so post-split blobs land nearer the
  70% target instead of the 35% floor (the fragmentation comes from how subtrees are
  peeled). Lower risk (one primitive), but bounded upside — it improves *new* splits, not
  the already-fragmented steady state.
- **(b) Sibling consolidation / rebalance primitive.** A new maintenance op that merges
  adjacent underfilled sibling leaf-blobs (the current parent-child `mergeBlob` does not
  apply to wide shallow trees). Higher upside (drives steady-state fill toward the band
  and shrinks depth), higher risk (new structural primitive on the W2D path; needs the
  full proptest + crash-soak treatment).

Either way this is a dedicated, design-first effort with adversarial review — not a
one-line change. The route-cache-sizing idea is **closed as unnecessary** by this data.

## Caveats / next

- The 8M depth anomaly (`MAX_PASSES` settle cap) should be re-run with an uncapped settle
  to get a clean depth-vs-scale point.
- Absolute cold-read latency here is macOS `F_NOCACHE`; real NVMe numbers + the QD sweep
  come from `scripts/ubuntu-validate.sh`.
- A finer blob-fill histogram (vs the current avg/max/underfilled/overfull summary) would
  sharpen (a) vs (b), but the summary already establishes the ~38%-floor finding.
