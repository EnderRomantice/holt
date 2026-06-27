#!/usr/bin/env bash
# Strict-RSS stress profile. Each engine runs in its own cgroup with the same
# MemoryMax, so the comparison is based on total process RSS rather than only
# each engine's internal cache knob.
set -euo pipefail

cd "$(dirname "$0")"

MEMORY_MAX=${HOLT_CGROUP_MEMORY_MAX:-1G}
WORKLOAD=${HOLT_CGROUP_WORKLOAD:-objstore}
ENGINES=${HOLT_CGROUP_ENGINES:-holt,rocksdb,sqlite}
OPS=${HOLT_CGROUP_OPS:-get,list_dir,prefix_empty}
N=${HOLT_CGROUP_N:-1000000}
POINT_OPS=${HOLT_CGROUP_POINT_OPS:-100000}
LIST_OPS=${HOLT_CGROUP_LIST_OPS:-10000}
LIST_TAKE=${HOLT_CGROUP_LIST_TAKE:-100}
DIR_TAKE=${HOLT_CGROUP_DIR_TAKE:-8}
BUFFER_POOL=${HOLT_CGROUP_BUFFER_POOL:-64}
VALUE_BYTES=${HOLT_CGROUP_VALUE_BYTES:-}
OUT=${HOLT_CGROUP_OUT:-/tmp/holt-cgroup-rss}

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "eval_cgroup_rss.sh requires Linux cgroup v2" >&2
  exit 1
fi

if ! command -v systemd-run >/dev/null 2>&1; then
  echo "systemd-run is required for strict cgroup RSS runs" >&2
  exit 1
fi

mkdir -p "$OUT"

cargo bench --manifest-path Cargo.toml --bench stress --features io-uring --no-run
BIN="$(pwd)/$(ls -t target/release/deps/stress-* | grep -v '\.d$' | head -1)"
echo "BIN=$BIN"
echo "cgroup_memory_max=$MEMORY_MAX workload=$WORKLOAD engines=$ENGINES ops=$OPS n=$N"

run_under_cgroup() {
  local engine=$1
  local log="$OUT/${WORKLOAD}_${engine}.log"
  local data="$OUT/data/${WORKLOAD}_${engine}"
  rm -rf "$data"
  mkdir -p "$data"

  echo "=== $engine memory=$MEMORY_MAX ==="
  local env_args=(
    "HOLT_STRESS_N=$N"
    "HOLT_STRESS_POINT_OPS=$POINT_OPS"
    "HOLT_STRESS_LIST_OPS=$LIST_OPS"
    "HOLT_STRESS_LIST_TAKE=$LIST_TAKE"
    "HOLT_STRESS_DIR_TAKE=$DIR_TAKE"
    "HOLT_STRESS_BUFFER_POOL=$BUFFER_POOL"
    "HOLT_STRESS_ENGINES=$engine"
    "HOLT_STRESS_OPS=$OPS"
    "HOLT_STRESS_WAL_SYNC=false"
    "HOLT_STRESS_ROCKSDB_DIRECT=1"
    "HOLT_STRESS_REOPEN_AFTER_PRELOAD=1"
    "HOLT_STRESS_DATA_DIR=$data"
  )
  if [[ -n "$VALUE_BYTES" ]]; then
    env_args+=("HOLT_STRESS_VALUE_BYTES=$VALUE_BYTES")
  fi

  if systemd-run --user --quiet --wait --collect --pipe \
      -p "MemoryMax=$MEMORY_MAX" -p "MemorySwapMax=0" \
      /usr/bin/time -v env "${env_args[@]}" "$BIN" "$WORKLOAD" \
      >"$log" 2>&1; then
    :
  else
    echo "user systemd-run failed for $engine; trying system scope via sudo -n" >&2
    sudo -n systemd-run --quiet --wait --collect --pipe \
      -p "MemoryMax=$MEMORY_MAX" -p "MemorySwapMax=0" \
      /usr/bin/time -v env "${env_args[@]}" "$BIN" "$WORKLOAD" \
      >"$log" 2>&1
  fi

  grep -E '^(holt|rocksdb|sqlite|sled|lmdb) |^space |^read_amp |^holt_shape final|Maximum resident set size' "$log"
}

IFS=',' read -ra engine_list <<<"$ENGINES"
for engine in "${engine_list[@]}"; do
  run_under_cgroup "$engine"
done

echo "logs=$OUT"
