# Internal benchmark probes

This directory holds historical one-off probes used while tuning
checkpointing, contention, tree shape, and large-path update
behavior. They are intentionally outside `benches/` and `tests/`:

- `benches/` is the public comparison surface for Holt vs RocksDB
  vs SQLite workload claims.
- `tests/` is for integration and correctness coverage.
- these probes are internal engineering scratchpads and are not
  Cargo targets.

When a probe becomes release-relevant, port it into `benches/` as a
proper comparator or into `tests/` as a correctness integration
test. Do not quote numbers from these files as public benchmark
results without first promoting the workload, restoring any
probe-only dependencies such as `hdrhistogram`, and refreshing
`benches/RESULTS.md`.
