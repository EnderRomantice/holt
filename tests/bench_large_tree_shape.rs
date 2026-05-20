//! Large-tree shape probe for holt's ART-over-blobs layout.
//!
//! This is not a comparator bench. It measures holt's own tree
//! shape after workloads that stress spillover policy quality:
//! skewed prefixes, hot directories, delete-heavy churn, and a
//! working set larger than a deliberately tiny buffer pool.
//!
//! Run explicitly:
//!
//! ```bash
//! cargo test --release --test bench_large_tree_shape -- --ignored --nocapture
//! ```
//!
//! For a short smoke run:
//!
//! ```bash
//! HOLT_SHAPE_BENCH_KEYS=5000 \
//! cargo test --release --test bench_large_tree_shape -- --ignored --nocapture
//! ```

use std::env;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use holt::{Tree, TreeBuilder, TreeConfig, TreeStats};
use tempfile::tempdir;

const DEFAULT_KEYS: usize = 80_000;
const DEFAULT_VALUE_LEN: usize = 192;
const DEFAULT_BUFFER_POOL_KEYS: usize = 60_000;
const BUFFER_POOL_SIZE: usize = 8;
const HIST_MAX_NS: u64 = 60_000_000_000;

#[derive(Debug, Clone, Copy)]
enum ShapeWorkload {
    SkewedPrefix,
    HotDirectory,
    DeleteHeavyChurn,
}

impl ShapeWorkload {
    fn name(self) -> &'static str {
        match self {
            Self::SkewedPrefix => "skewed-prefix",
            Self::HotDirectory => "hot-directory",
            Self::DeleteHeavyChurn => "delete-heavy-churn",
        }
    }

    fn key(self, i: usize, keys: usize) -> Vec<u8> {
        match self {
            Self::SkewedPrefix => {
                let hot_cutoff = keys.saturating_mul(8) / 10;
                if i < hot_cutoff {
                    format!(
                        "tenant-00/bucket-hot/shard-{}/path/sub/file-{i:08}.bin",
                        i % 4
                    )
                    .into_bytes()
                } else {
                    let cold = i - hot_cutoff;
                    format!(
                        "tenant-{}/bucket-{}/path/sub/file-{cold:08}.bin",
                        cold % 31 + 1,
                        cold % 16,
                    )
                    .into_bytes()
                }
            }
            Self::HotDirectory => format!(
                "/usr/local/share/category-hot/dir-{}/sub-{}/file-{i:08}",
                i % 16,
                (i / 16) % 8,
            )
            .into_bytes(),
            Self::DeleteHeavyChurn => {
                format!("churn/bucket-{}/segment-{}/file-{i:08}", i % 8, (i / 8) % 8).into_bytes()
            }
        }
    }

    fn deletes_key(self, i: usize) -> bool {
        matches!(self, Self::DeleteHeavyChurn) && i % 10 < 7
    }
}

struct ShapeReport {
    name: &'static str,
    keys: usize,
    deletes: usize,
    put_total: Duration,
    delete_total: Duration,
    compact_total: Duration,
    put_hist: Histogram<u64>,
    delete_hist: Histogram<u64>,
    after_put: TreeStats,
    final_stats: TreeStats,
}

#[test]
#[ignore = "shape bench; use `cargo test --release --test bench_large_tree_shape -- --ignored --nocapture`"]
fn large_tree_shape_matrix() {
    let keys = env_usize("HOLT_SHAPE_BENCH_KEYS", DEFAULT_KEYS);
    let value_len = env_usize("HOLT_SHAPE_BENCH_VALUE_LEN", DEFAULT_VALUE_LEN);

    println!("\n=== Large-tree shape matrix (memory, keys={keys}, value_len={value_len}) ===\n");

    for workload in [
        ShapeWorkload::SkewedPrefix,
        ShapeWorkload::HotDirectory,
        ShapeWorkload::DeleteHeavyChurn,
    ] {
        let report = run_shape_workload(workload, keys, value_len);
        print_shape_report(&report);
    }
}

#[test]
#[ignore = "shape bench; use `cargo test --release --test bench_large_tree_shape -- --ignored --nocapture`"]
fn working_set_larger_than_buffer_pool_probe() {
    let keys = env_usize("HOLT_SHAPE_BENCH_BUFFER_KEYS", DEFAULT_BUFFER_POOL_KEYS);
    let value_len = env_usize("HOLT_SHAPE_BENCH_VALUE_LEN", DEFAULT_VALUE_LEN);
    let dir = tempdir().unwrap();
    let value = vec![0xA5; value_len];

    let tree = TreeBuilder::new(dir.path())
        .buffer_pool_size(BUFFER_POOL_SIZE)
        .open()
        .unwrap();
    let mut put_hist = new_hist();
    let put_start = Instant::now();
    for i in 0..keys {
        let key = buffer_pressure_key(i);
        record_elapsed(&mut put_hist, || tree.put(&key, &value).unwrap());
    }
    let put_total = put_start.elapsed();
    tree.checkpoint().unwrap();
    let loaded = tree.stats().unwrap();
    drop(tree);

    let tree = TreeBuilder::new(dir.path())
        .buffer_pool_size(BUFFER_POOL_SIZE)
        .open()
        .unwrap();
    let mut get_hist = new_hist();
    let get_start = Instant::now();
    for i in 0..keys {
        let idx = (i.wrapping_mul(7919)) % keys;
        let key = buffer_pressure_key(idx);
        record_elapsed(&mut get_hist, || {
            assert_eq!(tree.get(&key).unwrap().as_deref(), Some(&value[..]));
        });
    }
    let get_total = get_start.elapsed();
    let after_get = tree.stats().unwrap();

    println!(
        "\n=== Working-set > buffer-pool probe (persistent, keys={keys}, value_len={value_len}, pool={BUFFER_POOL_SIZE}) ===\n"
    );
    println!(
        "preload: total={put_total:.2?} p50={:.2}us p95={:.2}us p99={:.2}us",
        hist_us(&put_hist, 50.0),
        hist_us(&put_hist, 95.0),
        hist_us(&put_hist, 99.0),
    );
    println!(
        "get-after-reopen: total={get_total:.2?} p50={:.2}us p95={:.2}us p99={:.2}us",
        hist_us(&get_hist, 50.0),
        hist_us(&get_hist, 95.0),
        hist_us(&get_hist, 99.0),
    );
    print_stats("after preload", &loaded);
    print_stats("after get probe", &after_get);
}

fn run_shape_workload(workload: ShapeWorkload, keys: usize, value_len: usize) -> ShapeReport {
    let tree = memory_tree();
    let value = vec![0x5A; value_len];

    let mut put_hist = new_hist();
    let put_start = Instant::now();
    for i in 0..keys {
        let key = workload.key(i, keys);
        record_elapsed(&mut put_hist, || tree.put(&key, &value).unwrap());
    }
    let put_total = put_start.elapsed();
    verify_samples(&tree, workload, keys, &value, false);
    let after_put = tree.stats().unwrap();

    let mut delete_hist = new_hist();
    let mut deletes = 0usize;
    let delete_start = Instant::now();
    for i in 0..keys {
        if workload.deletes_key(i) {
            let key = workload.key(i, keys);
            record_elapsed(&mut delete_hist, || {
                assert!(tree.delete(&key).unwrap());
            });
            deletes += 1;
        }
    }
    let delete_total = delete_start.elapsed();

    let compact_start = Instant::now();
    if deletes != 0 {
        tree.compact().unwrap();
    }
    let compact_total = compact_start.elapsed();
    verify_samples(&tree, workload, keys, &value, deletes != 0);
    let final_stats = tree.stats().unwrap();

    ShapeReport {
        name: workload.name(),
        keys,
        deletes,
        put_total,
        delete_total,
        compact_total,
        put_hist,
        delete_hist,
        after_put,
        final_stats,
    }
}

fn memory_tree() -> Tree {
    let mut cfg = TreeConfig::memory();
    // Shape probe only: keep mutations in the BM cache until the
    // benchmark asks for stats. This avoids measuring memory-backend
    // memcpy instead of walker/spillover shape.
    cfg.memory_flush_on_write = false;
    Tree::open(cfg).unwrap()
}

fn verify_samples(
    tree: &Tree,
    workload: ShapeWorkload,
    keys: usize,
    value: &[u8],
    deletes_applied: bool,
) {
    if keys == 0 {
        return;
    }
    let mut samples = vec![0usize, keys / 7, keys / 3, keys / 2, keys - 1];
    samples.sort_unstable();
    samples.dedup();
    for i in samples {
        let key = workload.key(i, keys);
        let got = tree.get(&key).unwrap();
        if deletes_applied && workload.deletes_key(i) {
            assert_eq!(
                got,
                None,
                "{} sample {i} should be deleted",
                workload.name()
            );
        } else {
            assert_eq!(
                got.as_deref(),
                Some(value),
                "{} sample {i} should be present",
                workload.name(),
            );
        }
    }
}

fn buffer_pressure_key(i: usize) -> Vec<u8> {
    format!(
        "pressure/tenant-{}/bucket-{}/path/sub/file-{i:08}.bin",
        i % 64,
        (i / 64) % 32,
    )
    .into_bytes()
}

fn print_shape_report(report: &ShapeReport) {
    println!(
        "\n-- {} (keys={}, deletes={}) --",
        report.name, report.keys, report.deletes
    );
    println!(
        "put:    total={:.2?} p50={:.2}us p95={:.2}us p99={:.2}us max={:.2}us",
        report.put_total,
        hist_us(&report.put_hist, 50.0),
        hist_us(&report.put_hist, 95.0),
        hist_us(&report.put_hist, 99.0),
        hist_max_us(&report.put_hist),
    );
    if report.deletes != 0 {
        println!(
            "delete: total={:.2?} p50={:.2}us p95={:.2}us p99={:.2}us compact={:.2?}",
            report.delete_total,
            hist_us(&report.delete_hist, 50.0),
            hist_us(&report.delete_hist, 95.0),
            hist_us(&report.delete_hist, 99.0),
            report.compact_total,
        );
    }
    print_stats("after put", &report.after_put);
    print_stats("final", &report.final_stats);
}

fn print_stats(label: &str, stats: &TreeStats) {
    println!(
        "{label:<14} blobs={:<5} space={:<10} gap={:<10} slots={:<8} tomb={:<6} compacts={:<5} dirty={:<5} pending={:<5} hits={:<6} misses={:<6} spill={:<5} merges={:<5} avg_hops={:.2} max_hops={:<3} max_depth={}",
        stats.blob_count,
        stats.total_space_used,
        stats.total_gap_space,
        stats.total_slots,
        stats.total_tombstones,
        stats.total_compactions,
        stats.bm_dirty_count,
        stats.bm_pending_delete_count,
        stats.bm_cache_hits,
        stats.bm_cache_misses,
        stats.bm_spillovers,
        stats.bm_merges,
        stats.bm_avg_blob_hops(),
        stats.bm_max_blob_hops,
        stats.bm_max_cross_blob_depth,
    );
}

fn new_hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_NS, 3).unwrap()
}

fn record_elapsed<T>(hist: &mut Histogram<u64>, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    let nanos = start.elapsed().as_nanos().min(u128::from(HIST_MAX_NS)) as u64;
    let _ = hist.record(nanos.max(1));
    out
}

fn hist_us(hist: &Histogram<u64>, percentile: f64) -> f64 {
    hist.value_at_percentile(percentile) as f64 / 1_000.0
}

fn hist_max_us(hist: &Histogram<u64>) -> f64 {
    hist.max() as f64 / 1_000.0
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}
