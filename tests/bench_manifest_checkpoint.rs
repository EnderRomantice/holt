//! Manifest/checkpoint pressure probe for persistent holt trees.
//!
//! This is a holt-only benchmark for the metadata persistence path:
//! repeated path-shaped inserts create new blobs, delete-heavy churn
//! plus `compact()` creates deferred manifest deletes, and explicit
//! `checkpoint()` calls force the manifest to become durable.
//!
//! Run explicitly:
//!
//! ```bash
//! cargo test --release --test bench_manifest_checkpoint -- --ignored --nocapture
//! ```
//!
//! Short smoke:
//!
//! ```bash
//! HOLT_MANIFEST_BENCH_ROUNDS=3 \
//! HOLT_MANIFEST_BENCH_KEYS_PER_ROUND=1000 \
//! cargo test --release --test bench_manifest_checkpoint -- --ignored --nocapture
//! ```

use std::env;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use holt::{TreeBuilder, TreeStats};
use tempfile::tempdir;

const DEFAULT_ROUNDS: usize = 8;
const DEFAULT_KEYS_PER_ROUND: usize = 4_000;
const DEFAULT_VALUE_LEN: usize = 192;
const DEFAULT_DELETE_STRIDE: usize = 4;
const DEFAULT_BUFFER_POOL: usize = 128;
const HIST_MAX_NS: u64 = 60_000_000_000;

#[test]
#[ignore = "manifest/checkpoint pressure bench; use `cargo test --release --test bench_manifest_checkpoint -- --ignored --nocapture`"]
fn manifest_log_checkpoint_pressure() {
    let rounds = env_usize("HOLT_MANIFEST_BENCH_ROUNDS", DEFAULT_ROUNDS);
    let keys_per_round = env_usize("HOLT_MANIFEST_BENCH_KEYS_PER_ROUND", DEFAULT_KEYS_PER_ROUND);
    let value_len = env_usize("HOLT_MANIFEST_BENCH_VALUE_LEN", DEFAULT_VALUE_LEN);
    let delete_stride = env_usize("HOLT_MANIFEST_BENCH_DELETE_STRIDE", DEFAULT_DELETE_STRIDE);
    let buffer_pool = env_usize("HOLT_MANIFEST_BENCH_BUFFER_POOL", DEFAULT_BUFFER_POOL);
    let dir = tempdir().unwrap();
    let value = vec![0xD7; value_len];

    let tree = TreeBuilder::new(dir.path())
        .buffer_pool_size(buffer_pool)
        .open()
        .unwrap();

    let mut put_checkpoint_hist = new_hist();
    let mut delete_checkpoint_hist = new_hist();
    let mut compact_hist = new_hist();
    let mut put_total = Duration::ZERO;
    let mut delete_total = Duration::ZERO;
    let mut total_deletes = 0usize;

    println!(
        "\n=== Manifest/checkpoint pressure (rounds={rounds}, keys_per_round={keys_per_round}, value_len={value_len}, delete_stride={delete_stride}, buffer_pool={buffer_pool}) ===\n"
    );
    println!(
        "{:<5} {:>8} {:>8} {:>10} {:>10} {:>10} {:>7} {:>7} {:>10} {:>10} {:>10} {:>10} {:>8} {:>8}",
        "round",
        "puts",
        "dels",
        "ckpt_put",
        "compact",
        "ckpt_del",
        "blobs",
        "pending",
        "m.bin",
        "m.log",
        "wal",
        "data",
        "avg_hop",
        "max_hop",
    );
    println!("{}", "-".repeat(140));

    for round in 0..rounds {
        let put_start = Instant::now();
        for i in 0..keys_per_round {
            tree.put(&bench_key(round, i), &value).unwrap();
        }
        put_total += put_start.elapsed();

        let ckpt_put = record_elapsed(&mut put_checkpoint_hist, || tree.checkpoint().unwrap());

        let delete_start = Instant::now();
        let mut round_deletes = 0usize;
        for i in 0..keys_per_round {
            if should_delete(i, delete_stride) {
                assert!(tree.delete(&bench_key(round, i)).unwrap());
                round_deletes += 1;
            }
        }
        delete_total += delete_start.elapsed();
        total_deletes += round_deletes;

        let compact_elapsed = record_elapsed(&mut compact_hist, || tree.compact().unwrap());
        let ckpt_delete =
            record_elapsed(&mut delete_checkpoint_hist, || tree.checkpoint().unwrap());
        let stats = tree.stats().unwrap();
        print_round(&RoundReport {
            round,
            puts: keys_per_round,
            deletes: round_deletes,
            ckpt_put,
            compact: compact_elapsed,
            ckpt_delete,
            stats: &stats,
            dir: dir.path(),
        });
    }

    drop(tree);

    let reopen_start = Instant::now();
    let tree = TreeBuilder::new(dir.path())
        .buffer_pool_size(buffer_pool)
        .open()
        .unwrap();
    let reopen = reopen_start.elapsed();

    verify_survivors(&tree, rounds, keys_per_round, delete_stride, &value);
    let stats = tree.stats().unwrap();

    println!("\nsummary:");
    println!("  put_total       : {put_total:.2?}");
    println!("  delete_total    : {delete_total:.2?} ({total_deletes} deletes)");
    print_hist("  checkpoint after put   ", &put_checkpoint_hist);
    print_hist("  checkpoint after delete", &delete_checkpoint_hist);
    print_hist("  compact                ", &compact_hist);
    println!("  reopen          : {reopen:.2?}");
    println!(
        "  final files     : manifest.bin={} manifest.log={} wal={} data={}",
        pretty_bytes(file_size(dir.path(), "manifest.bin")),
        pretty_bytes(file_size(dir.path(), "manifest.log")),
        pretty_bytes(file_size(dir.path(), "journal.wal")),
        pretty_bytes(file_size(dir.path(), "blobs.dat")),
    );
    println!(
        "  final shape     : blobs={} pending={} spill={} merges={} avg_hops={:.2} max_hops={}",
        stats.blob_count,
        stats.bm_pending_delete_count,
        stats.bm_spillovers,
        stats.bm_merges,
        stats.bm_avg_blob_hops(),
        stats.bm_max_blob_hops,
    );

    assert_eq!(stats.bm_pending_delete_count, 0);
    assert!(reopen < Duration::from_secs(5));
}

fn bench_key(round: usize, i: usize) -> Vec<u8> {
    format!(
        "tenant-{}/bucket-hot/dir-{}/sub-{}/file-r{round:03}-{i:08}.bin",
        round % 8,
        i % 32,
        (i / 32) % 16,
    )
    .into_bytes()
}

fn should_delete(i: usize, delete_stride: usize) -> bool {
    delete_stride > 1 && i % delete_stride != 0
}

fn verify_survivors(
    tree: &holt::Tree,
    rounds: usize,
    keys_per_round: usize,
    delete_stride: usize,
    value: &[u8],
) {
    for round in 0..rounds {
        for i in [
            0,
            keys_per_round / 3,
            keys_per_round / 2,
            keys_per_round - 1,
        ] {
            let key = bench_key(round, i);
            let got = tree.get(&key).unwrap();
            if should_delete(i, delete_stride) {
                assert_eq!(got, None, "round={round} i={i} should be deleted");
            } else {
                assert_eq!(
                    got.as_deref(),
                    Some(value),
                    "round={round} i={i} should survive",
                );
            }
        }
    }
}

struct RoundReport<'a> {
    round: usize,
    puts: usize,
    deletes: usize,
    ckpt_put: Duration,
    compact: Duration,
    ckpt_delete: Duration,
    stats: &'a TreeStats,
    dir: &'a Path,
}

fn print_round(report: &RoundReport<'_>) {
    println!(
        "{:<5} {:>8} {:>8} {:>10.2?} {:>10.2?} {:>10.2?} {:>7} {:>7} {:>10} {:>10} {:>10} {:>10} {:>8.2} {:>8}",
        report.round,
        report.puts,
        report.deletes,
        report.ckpt_put,
        report.compact,
        report.ckpt_delete,
        report.stats.blob_count,
        report.stats.bm_pending_delete_count,
        pretty_bytes(file_size(report.dir, "manifest.bin")),
        pretty_bytes(file_size(report.dir, "manifest.log")),
        pretty_bytes(file_size(report.dir, "journal.wal")),
        pretty_bytes(file_size(report.dir, "blobs.dat")),
        report.stats.bm_avg_blob_hops(),
        report.stats.bm_max_blob_hops,
    );
}

fn file_size(dir: &Path, name: &str) -> u64 {
    fs::metadata(dir.join(name)).map(|m| m.len()).unwrap_or(0)
}

fn pretty_bytes(bytes: u64) -> String {
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        format!("{kib:.1}K")
    } else {
        format!("{:.2}M", kib / 1024.0)
    }
}

fn new_hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, HIST_MAX_NS, 3).unwrap()
}

fn record_elapsed<T>(hist: &mut Histogram<u64>, f: impl FnOnce() -> T) -> Duration {
    let start = Instant::now();
    let _ = f();
    let elapsed = start.elapsed();
    let nanos = elapsed.as_nanos().min(u128::from(HIST_MAX_NS)) as u64;
    let _ = hist.record(nanos.max(1));
    elapsed
}

fn print_hist(label: &str, hist: &Histogram<u64>) {
    println!(
        "{label}: p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms",
        hist_ms(hist, 50.0),
        hist_ms(hist, 95.0),
        hist_ms(hist, 99.0),
        hist.max() as f64 / 1_000_000.0,
    );
}

fn hist_ms(hist: &Histogram<u64>, percentile: f64) -> f64 {
    hist.value_at_percentile(percentile) as f64 / 1_000_000.0
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}
