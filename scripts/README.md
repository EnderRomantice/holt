# scripts/

## `ubuntu-validate.sh` — real-hardware validation kit

Turns the cold-read / io_uring **projected** wins into **proven** ones. The cold-read
stack (in-blob routing region, per-blob bloom, the batched-read `pread_many` path) and
the io_uring backend are developed on macOS, where they can only be **compile-checked**:
Darwin has no io_uring and no `O_DIRECT`. The unsafe ring submission, whether
`IORING_OP_FSYNC|DATASYNC` actually flushes the device, crash-consistency under
`SIGKILL`, and real cold-read latency on NVMe **cannot be observed on macOS**. This
script runs exactly that validation on a Linux + NVMe box and writes a report.

It does **not** self-assert success — a human reads the report. The crash soaks must
show **zero** recovery failures; the latency numbers are the measurement.

### Run it

On the Linux box, from the repo root, with the working dir on a real NVMe device:

```sh
scripts/ubuntu-validate.sh --dir /mnt/nvme/holt-validate          # full run
scripts/ubuntu-validate.sh --dir /mnt/nvme/holt-validate --quick  # fast smoke
```

Flags: `--dir DIR` (NVMe working dir), `--soak-rounds N` (default 300),
`--soak-secs S` (default 600), `--scale N` (cold-read dataset keys, default 2,000,000),
`--quick` (30 rounds / 60 s / 500k).

### What it does (5 sections, all written to `report-<stamp>.txt`)

0. **Environment** — kernel, cargo, and a best-effort check that `--dir` is on a
   non-rotational device. If it's rotational or tmpfs, the cold numbers are meaningless.
1. **Build** with `--features io-uring` (release).
2. **io_uring tests** — the full suite under io-uring (the `FileBlobStore` temp-dir tests
   drive the real ring on Linux), plus the explicit `pread_many_at` batched-read tests.
   Every `test result` must show `0 failed`; a failure here is a ring submission / CQE /
   buffer-lifetime bug invisible on macOS.
3. **Crash soak** — `examples/wal_crash_soak` (N SIGKILL rounds, sync+async, the
   contiguous-prefix invariant) and `tools/soak --mode crash --wal-sync true` (ack-log
   verification). These exercise the async RAM→page-cache window + flusher-mid-drain +
   mid-checkpoint-truncate under a real `kill -9` on the io_uring backend. **Any failure
   line = durability/corruption bug.**
4. **Cold-read latency on NVMe** — `benches/stress` reopened+routed: a single-thread
   cold get, a QD 1→16 sweep (the "concurrency gives free queue depth" claim), a second
   scale point (cost should stay bounded as N grows), and the fair vs-RocksDB comparison
   (`HOLT_STRESS_ROCKSDB_DIRECT=true`, both cold + direct I/O at matched memory; needs
   libclang — see `PERF_FINDINGS.md`).
5. **Summary checklist** — the boxes a human ticks off from the captured numbers.

### Why a human reads the report

The macOS single-thread cold get (~349 µs) is `F_NOCACHE`, **not** the NVMe number. The
whole point is to get the real NVMe figure and confirm: ring tests pass, soaks show zero
failures, the QD sweep scales near-linearly, and the cold-read cost stays bounded with
scale. Those are the facts that move "projected" to "proven".
