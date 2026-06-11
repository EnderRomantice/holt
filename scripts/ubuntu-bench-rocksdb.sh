#!/usr/bin/env bash
#
# ubuntu-bench-rocksdb.sh — comprehensive holt vs RocksDB comparison at scale.
#
# WHY / WHERE TO RUN IT
# --------------------
# The representative numbers need Linux + NVMe: that is where holt uses io_uring
# + O_DIRECT and RocksDB uses real direct reads. It will RUN on macOS too (both
# engines on the same box, matched memory — the comparison is platform-fair), but
# the absolute latencies on macOS are F_NOCACHE / no-io_uring and do NOT represent
# production. Run it on the ubuntu NVMe box for the numbers that count.
#
# WHAT IT COMPARES
#   For each data scale, two cache regimes, holt vs RocksDB on the SAME objstore
#   metadata workload (~30 B path keys, ~60 B values), matched memory:
#
#   1. HOT / large buffer pool (buffered, "warm service"): pool sized to hold most
#      of the tree; RocksDB buffered (OS page cache). holt Wal{sync:false} + bg
#      checkpoint vs RocksDB WAL-on/sync-off. Measures cache-resident reads + writes.
#   2. COLD / medium buffer pool (direct, matched memory): pool a fraction of the
#      tree; RocksDB set_use_direct_reads + block cache == buffer_pool * 512 KB
#      (the bench matches them — stress.rs:1078-1091). holt reopened so reads are
#      cold. This is the apples-to-apples cold-read comparison. Plus a holt
#      queue-depth sweep (1/8/16 threads) showing the free-QD scaling.
#
# Fairness: under --rocksdb-direct the bench sizes RocksDB's LRU block cache to
# exactly buffer_pool_size * 512 KB, the same bytes holt's pool gets. The HOT
# regime is the realistic "both warm" service profile; the COLD regime is the
# matched-memory direct-I/O profile.
#
# USAGE
#   scripts/ubuntu-bench-rocksdb.sh [--dir DIR] [--scales N,N,...]
#       [--large-bf N] [--medium-bf N] [--point-ops N] [--quick]
#
#   --dir DIR        working dir on NVMe (default ./target/holt-bench)
#   --scales LIST    key counts, comma-sep (default 10000000,50000000)
#   --large-bf N     large pool in 512KB frames (default: ~tree size, capped 16384)
#   --medium-bf N    medium pool in frames (default: ~6% of tree, min 512)
#   --point-ops N    point-read/write sample count (default 200000)
#   --quick          scales 2000000,10000000 + 100000 ops
#
# Needs the comparators feature (RocksDB → libclang; see PERF_FINDINGS.md repro notes).
#
set -euo pipefail

DIR="./target/holt-bench"
SCALES="10000000,50000000"
LARGE_BF=""
MEDIUM_BF=""
POINT_OPS=200000
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dir)       DIR="$2"; shift 2 ;;
    --scales)    SCALES="$2"; shift 2 ;;
    --large-bf)  LARGE_BF="$2"; shift 2 ;;
    --medium-bf) MEDIUM_BF="$2"; shift 2 ;;
    --point-ops) POINT_OPS="$2"; shift 2 ;;
    --quick)     SCALES="2000000,10000000"; POINT_OPS=100000; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
mkdir -p "$DIR"
STAMP="$(date +%Y%m%d-%H%M%S)"
REPORT="$DIR/bench-rocksdb-$STAMP.txt"
log()  { echo -e "$*" | tee -a "$REPORT"; }
rule() { log "\n================ $* ================"; }

rule "0. Environment"
log "date    : $(date -u +%FT%TZ)"
log "host    : $(uname -a)"
log "cargo   : $(cargo --version 2>/dev/null || echo MISSING)"
if [[ "$(uname -s)" == "Linux" ]]; then
  log "platform: Linux — io_uring + O_DIRECT active (representative)"
  DEV="$(df --output=source "$DIR" 2>/dev/null | tail -1 | sed 's,/dev/,,;s,[0-9]*$,,')" || DEV=""
  if [[ -n "$DEV" && -r "/sys/block/$DEV/queue/rotational" ]]; then
    R="$(cat "/sys/block/$DEV/queue/rotational")"
    log "device  : $DEV rotational=$R $([[ "$R" == 0 ]] && echo '(NVMe/SSD ✓)' || echo '(ROTATIONAL — not representative!)')"
  fi
  MEMKB="$(awk '/MemTotal/{print $2}' /proc/meminfo 2>/dev/null || echo 0)"
  log "mem     : $((MEMKB/1024/1024)) GiB total"
else
  log "platform: $(uname -s) — WARNING: F_NOCACHE, no io_uring. Comparison is fair but absolute"
  log "          latencies are NOT representative of the ubuntu+NVMe target. Sanity only."
fi

rule "1. Build stress bench with comparators (RocksDB)"
if ! cargo bench --manifest-path benches/Cargo.toml --bench stress --features comparators --no-run 2>&1 | tail -4 | tee -a "$REPORT"; then
  log "!! comparators build failed — libclang missing? See PERF_FINDINGS.md repro notes. Cannot compare RocksDB."
  exit 1
fi
BIN="$(ls -t benches/target/release/deps/stress-* | grep -v '\.d$' | head -1)"
log "stress bin: $BIN"

# blobs ~= keys / 1300 (measured ~1300 keys/blob at the ~0.38 steady-state fill).
est_blobs () { echo $(( ${1} / 1300 + 1 )); }

run_case () { # $1=label  then KEY=VAL env pairs (consumed by env)
  local label="$1"; shift
  log "\n--- $label ---"
  mkdir -p "$DIR/run"
  ( cd "$REPO_ROOT" && env TMPDIR="$DIR/run" "$@" "$BIN" objstore ) 2>&1 \
    | grep -E "profile=|holt_shape (routed|reopened)|holt +(get|put|list|list_dir)|rocksdb +(get|put|list|list_dir)" \
    | tee -a "$REPORT" || true
}

IFS=',' read -ra SCALE_ARR <<< "$SCALES"
for N in "${SCALE_ARR[@]}"; do
  BLOBS="$(est_blobs "$N")"
  LBF="${LARGE_BF:-$(( BLOBS < 16384 ? BLOBS + 64 : 16384 ))}"
  MBF="${MEDIUM_BF:-$(( BLOBS/16 < 512 ? 512 : BLOBS/16 ))}"
  rule "Scale N=$N (~$BLOBS blobs, ~$((BLOBS/2)) MiB tree)  large_bf=$LBF (~$((LBF/2)) MiB)  medium_bf=$MBF (~$((MBF/2)) MiB)"

  # Regime 1: HOT / large pool, both buffered (warm service).
  run_case "HOT  large_bf=$LBF  holt vs rocksdb (buffered, warm service)" \
    HOLT_STRESS_N="$N" HOLT_STRESS_POINT_OPS="$POINT_OPS" HOLT_STRESS_LIST_OPS=20000 \
    HOLT_STRESS_BUFFER_POOL="$LBF" HOLT_STRESS_COMPACT_AFTER_PRELOAD=true \
    HOLT_STRESS_REOPEN_AFTER_PRELOAD=false HOLT_STRESS_ROCKSDB_DIRECT=false \
    HOLT_STRESS_ENGINES=holt,rocksdb HOLT_STRESS_OPS=get,put,list,list_dir HOLT_STRESS_GET_THREADS=1

  # Regime 2: COLD / medium pool, both direct + matched block cache (apples-to-apples cold).
  run_case "COLD medium_bf=$MBF  holt vs rocksdb (direct, matched memory, reopened)" \
    HOLT_STRESS_N="$N" HOLT_STRESS_POINT_OPS="$POINT_OPS" HOLT_STRESS_LIST_OPS=20000 \
    HOLT_STRESS_BUFFER_POOL="$MBF" HOLT_STRESS_COMPACT_AFTER_PRELOAD=true \
    HOLT_STRESS_REOPEN_AFTER_PRELOAD=true HOLT_STRESS_ROCKSDB_DIRECT=true \
    HOLT_STRESS_ENGINES=holt,rocksdb HOLT_STRESS_OPS=get,list,list_dir HOLT_STRESS_GET_THREADS=1

  # Regime 2b: holt cold queue-depth sweep (free-QD: aggregate throughput ~ device QD).
  for T in 1 8 16; do
    run_case "COLD medium_bf=$MBF  holt get QD=$T (free queue depth)" \
      HOLT_STRESS_N="$N" HOLT_STRESS_POINT_OPS="$POINT_OPS" \
      HOLT_STRESS_BUFFER_POOL="$MBF" HOLT_STRESS_COMPACT_AFTER_PRELOAD=true \
      HOLT_STRESS_REOPEN_AFTER_PRELOAD=true HOLT_STRESS_ROCKSDB_DIRECT=true \
      HOLT_STRESS_ENGINES=holt HOLT_STRESS_OPS=get HOLT_STRESS_GET_THREADS="$T"
  done
done

rule "Summary (read these)"
log "[ ] HOT/large-bf: holt point read should beat RocksDB (the read-engine claim); writes ~parity/behind at scale (in-place tree vs LSM)."
log "[ ] COLD/medium-bf: holt vs RocksDB cold positive point read — holt is ~2:1 read-count behind on a positive miss; holt should win list_dir and negatives."
log "[ ] COLD QD sweep: holt aggregate get throughput should scale ~linearly 1->8->16 (free device queue depth from the lock-free read path)."
log "[ ] Watch holt_shape: blobs / avg_fill (~0.38) / avg_hops — the scale-stability shape (see docs/design/scale-curve-findings.md)."
log "\nNOTE: 50M single-thread preload is long (tens of minutes) and the large-bf pool for 50M needs ~20 GiB RAM — override --large-bf or drop the 50M scale on smaller boxes."
log "\nFull report: $REPORT"
echo "Full report: $REPORT"
