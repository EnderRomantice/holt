#!/usr/bin/env bash
#
# ubuntu-validate.sh — turn the cold-read / io_uring "projected" wins into
# "proven" ones, on a real Linux + NVMe box.
#
# WHY THIS EXISTS
# ---------------
# Holt's cold-read stack (in-blob routing region, per-blob bloom, the
# batched-read pread_many path) and the io_uring backend are developed on
# macOS, where they can only be COMPILE-checked: Darwin has no io_uring and
# no O_DIRECT, so the unsafe ring submission, the fdatasync-actually-flushes
# behavior, crash-consistency under SIGKILL, and real cold-read latency
# CANNOT be observed there. This script runs the validation that only a
# Linux+NVMe box can run, and writes a report you read to decide whether the
# projected wins hold.
#
# It does not assert success on its own — a human reads the report. Crash
# soaks must show ZERO recovery failures; the latency numbers are the
# measurement, not a pass/fail by themselves.
#
# REQUIREMENTS
#   - Linux (io_uring; kernel >= 5.6 recommended for the ops Holt uses)
#   - The data dir (--dir) on a real NVMe device (not tmpfs, not a VM
#     overlay) — otherwise the cold-read numbers are meaningless.
#   - rustup/cargo. For the RocksDB fair comparison: libclang (see
#     PERF_FINDINGS.md "Benchmark reproduction notes").
#
# USAGE
#   scripts/ubuntu-validate.sh [--dir DIR] [--soak-rounds N] [--soak-secs S]
#                              [--scale N] [--quick]
#
#   --dir DIR        working dir on NVMe (default: ./target/ubuntu-validate)
#   --soak-rounds N  wal_crash_soak SIGKILL rounds (default: 300)
#   --soak-secs S    tools/soak crash-mode duration seconds (default: 600)
#   --scale N        cold-read latency dataset size in keys (default: 2000000)
#   --quick          fast smoke: 30 soak rounds, 60s crash, 500k scale
#
set -euo pipefail

# ----- args -----
DIR="./target/ubuntu-validate"
SOAK_ROUNDS=300
SOAK_SECS=600
SCALE=2000000
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dir)         DIR="$2"; shift 2 ;;
    --soak-rounds) SOAK_ROUNDS="$2"; shift 2 ;;
    --soak-secs)   SOAK_SECS="$2"; shift 2 ;;
    --scale)       SCALE="$2"; shift 2 ;;
    --quick)       SOAK_ROUNDS=30; SOAK_SECS=60; SCALE=500000; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
mkdir -p "$DIR"
STAMP="$(date +%Y%m%d-%H%M%S)"
REPORT="$DIR/report-$STAMP.txt"
SCALE_NEXT=$((SCALE * 4))   # a second scale point for the cold-read curve

log()  { echo -e "$*" | tee -a "$REPORT"; }
rule() { log "\n================ $* ================"; }

# ----- guard: must be Linux -----
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "REFUSING TO RUN: this validation is meaningful only on Linux+NVMe." >&2
  echo "On $(uname -s) the io_uring path is cfg'd out and cold I/O is not real." >&2
  exit 1
fi

rule "0. Environment"
log "date            : $(date -u +%FT%TZ)"
log "host            : $(uname -a)"
log "kernel io_uring : $(grep -qi io_uring /proc/kallsyms 2>/dev/null && echo present || echo 'UNKNOWN (need root for kallsyms; assume present on >=5.6)')"
log "cargo           : $(cargo --version 2>/dev/null || echo MISSING)"
log "rustc           : $(rustc --version 2>/dev/null || echo MISSING)"
log "working dir     : $DIR"
# Best-effort: is DIR on rotational media? rota=0 means SSD/NVMe.
DEV="$(df --output=source "$DIR" 2>/dev/null | tail -1 | sed 's,/dev/,,;s,[0-9]*$,,')" || DEV=""
if [[ -n "$DEV" && -r "/sys/block/$DEV/queue/rotational" ]]; then
  ROTA="$(cat "/sys/block/$DEV/queue/rotational")"
  log "block dev       : $DEV rotational=$ROTA $([[ "$ROTA" == "0" ]] && echo '(SSD/NVMe ✓)' || echo '(ROTATIONAL — cold-read numbers will NOT represent NVMe!)')"
else
  log "block dev       : could not determine for $DIR — VERIFY it is on NVMe by hand"
fi

# ============================================================
rule "1. Build (io_uring feature, release)"
# default features include io-uring; be explicit so the report is unambiguous.
cargo build --release --features io-uring 2>&1 | tee -a "$REPORT" | tail -3
cargo build --release --example wal_crash_soak --features io-uring 2>&1 | tail -2 | tee -a "$REPORT"

# ============================================================
rule "2. io_uring unit + integration tests (the ring path actually runs here)"
log "-- full suite under io-uring (FileBlobStore temp-dir tests exercise the real ring) --"
cargo test --release --features io-uring 2>&1 | tee -a "$REPORT" | grep -E "test result|FAILED|error\[" || true
log "\n-- explicit batched-read ring tests (the pread_many path added for cold-scan read-ahead) --"
cargo test --release --features io-uring pread_many_at -- --nocapture 2>&1 \
  | tee -a "$REPORT" | grep -E "test |result|FAILED" || true
log "\nEXPECT: every 'test result' line shows 0 failed. A failure here is a real"
log "        io_uring submission/CQE/lifetime bug invisible on macOS."

# ============================================================
rule "3. Crash-soak (SIGKILL durability — the gate macOS compile-check cannot give)"
log "-- examples/wal_crash_soak: $SOAK_ROUNDS rounds, sync+async, ordered-prefix invariant --"
log "   (recovered state must be a CONTIGUOUS prefix {0..K}; any gap/torn/extra key = bug)"
# The example allocates its own tempfile::tempdir() and reuses it across rounds;
# it honors $TMPDIR for placement, so point TMPDIR at the NVMe working dir.
SOAK_DIR="$DIR/wal-crash-soak"
rm -rf "$SOAK_DIR"; mkdir -p "$SOAK_DIR"
( cd "$REPO_ROOT" && TMPDIR="$SOAK_DIR" \
  cargo run --release --example wal_crash_soak --features io-uring -- "$SOAK_ROUNDS" ) \
  2>&1 | tee -a "$REPORT" | tail -20 || log "!! wal_crash_soak EXITED NONZERO — durability/corruption FAILURE, investigate"

log "\n-- tools/soak crash mode: ${SOAK_SECS}s, ack-log verification, wal-sync=true --"
SOAK2_DIR="$DIR/soak-crash"
rm -rf "$SOAK2_DIR"; mkdir -p "$SOAK2_DIR"
( cd "$REPO_ROOT" && cargo run --release --manifest-path tools/soak/Cargo.toml --locked -- \
    --mode crash --dir "$SOAK2_DIR" --reset \
    --duration-secs "$SOAK_SECS" --keys 200000 --ops 2000000 \
    --threads 4 --buffer-pool 64 --wal-sync true ) \
  2>&1 | tee -a "$REPORT" | tail -20 || log "!! tools/soak crash EXITED NONZERO — every acked op must replay; FAILURE"
log "\nEXPECT: both soaks complete with no failure line. These exercise the async"
log "        RAM->page-cache window + flusher-mid-drain + mid-checkpoint-truncate"
log "        under a real kill -9 on the io_uring backend."

# ============================================================
rule "4. Cold-read latency on real NVMe (the projected win, now measured)"
mkdir -p "$DIR/stress" "$DIR/stress-cmp"
cargo bench --manifest-path benches/Cargo.toml --bench stress --no-run 2>&1 | tail -3 | tee -a "$REPORT"
STRESS_BIN="$(ls -t benches/target/release/deps/stress-* | grep -v '\.d$' | head -1)"
if [[ -z "${STRESS_BIN:-}" || ! -x "$STRESS_BIN" ]]; then
  log "!! could not locate stress bench binary; skipping section 4"; STRESS_BIN=""
fi
log "stress bin: ${STRESS_BIN:-<none>}"

cold_run () { # $1=label  $2=N  $3=threads  extra env via $4...
  local label="$1" n="$2" t="$3"; shift 3
  log "\n--- $label : N=$n cold(reopen) routed get_threads=$t ---"
  TMPDIR="$DIR/stress" HOLT_STRESS_N="$n" HOLT_STRESS_POINT_OPS=100000 \
    HOLT_STRESS_COMPACT_AFTER_PRELOAD=true HOLT_STRESS_REOPEN_AFTER_PRELOAD=true \
    HOLT_STRESS_OPS=get,list_dir HOLT_STRESS_GET_THREADS="$t" "$@" \
    "$STRESS_BIN" objstore 2>&1 \
    | grep -E "profile=|holt_shape (routed|reopened)|holt     (get|list_dir)" | tee -a "$REPORT" || true
}

if [[ -n "$STRESS_BIN" ]]; then
log "# Cold single-thread + queue-depth sweep (free-QD claim: ~linear to device QD)"
cold_run "QD1"  "$SCALE" 1
cold_run "QD4"  "$SCALE" 4
cold_run "QD8"  "$SCALE" 8
cold_run "QD16" "$SCALE" 16

log "\n# Scale point 2 (cold-read cost should stay flat-ish per-op as N grows)"
cold_run "QD1 @scale2" "$SCALE_NEXT" 1
cold_run "QD8 @scale2" "$SCALE_NEXT" 8

log "\n# FAIR vs RocksDB (both cold + direct I/O at matched memory)."
log "# Requires the comparators feature (libclang). If it fails to build, skip — holt numbers above still stand."
if cargo bench --manifest-path benches/Cargo.toml --bench stress --features comparators --no-run 2>>"$REPORT"; then
  RBIN="$(ls -t benches/target/release/deps/stress-* | grep -v '\.d$' | head -1)"
  TMPDIR="$DIR/stress-cmp" HOLT_STRESS_N="$SCALE" HOLT_STRESS_POINT_OPS=100000 \
    HOLT_STRESS_COMPACT_AFTER_PRELOAD=true HOLT_STRESS_REOPEN_AFTER_PRELOAD=true \
    HOLT_STRESS_ROCKSDB_DIRECT=true HOLT_STRESS_ENGINES=holt,rocksdb \
    HOLT_STRESS_OPS=get,list,list_dir HOLT_STRESS_GET_THREADS=1 \
    "$RBIN" objstore 2>&1 | grep -E "holt |rocksdb " | tee -a "$REPORT" || true
else
  log "(comparators build failed — libclang missing? See PERF_FINDINGS.md repro notes. Skipping RocksDB compare.)"
fi
else
  log "(stress bench binary unavailable — skipped section 4 cold-read measurement)"
fi

# ============================================================
rule "5. Summary checklist (read these)"
log "[ ] Section 2: every 'test result' shows 0 failed (io_uring ring path correct on real kernel)."
log "[ ] Section 3: both crash soaks completed with NO failure line (durability holds under kill -9 on io_uring)."
log "[ ] Section 4: cold get QD1 latency on NVMe — record it; this is the real cold-read number (macOS ~349us is F_NOCACHE, not this)."
log "[ ] Section 4: QD1->QD16 shows near-linear aggregate speedup (the free-QD claim holds on real NVMe)."
log "[ ] Section 4: per-op cold get at scale2 (4x N) is not dramatically worse than scale1 (cold-read cost stays bounded)."
log "[ ] Section 4: vs RocksDB-direct — holt cold positive point read is ~2:1 read-count behind; holt should still win list_dir + QD."
log "\nFull report: $REPORT"
echo "Full report written to: $REPORT"
