//! Persistent cold-I/O comparator for Holt, RocksDB, and SQLite.
//!
//! This is deliberately separate from the Criterion `*_persist_*`
//! groups in `benches/main.rs`. Those measure a hot service state:
//! WAL enabled, in-process caches warm, no per-op fsync. This probe
//! measures the other question: what happens when the process is
//! reopened and random metadata operations must fault storage pages
//! back in.
//!
//! Fairness boundary:
//! - every engine preloads the same dataset,
//! - every engine forces its checkpoint/flush boundary,
//! - every engine is dropped and reopened before timing,
//! - on Linux, every file below the temp directory gets
//!   `posix_fadvise(POSIX_FADV_DONTNEED)` before reopen.
//!
//! Run explicitly:
//!
//! ```bash
//! cargo bench --features io-uring --bench cold_io
//! ```
//!
//! Short smoke:
//!
//! ```bash
//! HOLT_COLD_IO_KEYS=20000 HOLT_COLD_IO_OPS=2000 \
//! cargo bench --features io-uring --bench cold_io
//! ```

use std::env;
use std::fs::{self, File};
use std::hint::black_box;
use std::io::Write;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use holt::TreeBuilder;
use rocksdb::{Options, WriteOptions, DB};
use rusqlite::{params, Connection};
use tempfile::TempDir;

const DEFAULT_KEYS: usize = 2_000_000;
const DEFAULT_OPS: usize = 20_000;
const KV_KEY_LEN: usize = 32;
const KV_VAL_LEN: usize = 64;
const OBJSTORE_BUCKETS: usize = 32;
const FS_DIRS: usize = 16;
const SEED: u64 = 0xD1B5_54A3_9E37_79B9;

#[derive(Clone, Copy)]
enum Workload {
    Kv,
    Objstore,
    Fs,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Objstore => "objstore",
            Self::Fs => "fs",
        }
    }
}

struct EngineReport {
    cold_get: Duration,
    cold_put: Duration,
}

fn main() {
    persistent_cold_io_compare();
}

fn persistent_cold_io_compare() {
    let keys = env_usize("HOLT_COLD_IO_KEYS", DEFAULT_KEYS);
    let ops = env_usize("HOLT_COLD_IO_OPS", DEFAULT_OPS);
    let indices = index_sequence(keys, ops, SEED);

    println!(
        "\n=== Persistent cold-I/O compare (keys={keys}, ops={ops}, cache_drop={}) ===\n",
        cache_drop_mode(),
    );
    println!(
        "{:<9} {:<9} {:>12} {:>12} {:>12} {:>10} {:>10}",
        "workload", "op", "holt_ns", "rocks_ns", "sqlite_ns", "vs_rocks", "vs_sqlite"
    );
    println!("{}", "-".repeat(83));

    for workload in [Workload::Kv, Workload::Objstore, Workload::Fs] {
        progress(format!(
            "preparing {} dataset ({keys} keys)",
            workload.name()
        ));
        let pairs = gen_dataset(workload, keys);

        progress(format!("{} / holt", workload.name()));
        let holt = bench_holt(&pairs, &indices);
        progress(format!("{} / rocksdb", workload.name()));
        let rocks = bench_rocksdb(&pairs, &indices);
        progress(format!("{} / sqlite", workload.name()));
        let sqlite = bench_sqlite(&pairs, &indices);

        print_row(
            workload.name(),
            "cold_get",
            holt.cold_get,
            rocks.cold_get,
            sqlite.cold_get,
            ops,
        );
        print_row(
            workload.name(),
            "cold_put",
            holt.cold_put,
            rocks.cold_put,
            sqlite.cold_put,
            ops,
        );
    }
}

fn bench_holt(pairs: &[(Vec<u8>, Vec<u8>)], indices: &[usize]) -> EngineReport {
    let dir = TempDir::new().unwrap();
    {
        let tree = TreeBuilder::new(dir.path()).open().unwrap();
        for (k, v) in pairs {
            tree.put(k, v).unwrap();
        }
        tree.checkpoint().unwrap();
    }

    drop_file_cache(dir.path());
    let tree = TreeBuilder::new(dir.path()).open().unwrap();
    let cold_get = elapsed(|| {
        for &idx in indices {
            let (k, v) = &pairs[idx];
            assert_eq!(
                tree.get(black_box(k)).unwrap().as_deref(),
                Some(v.as_slice())
            );
        }
    });
    drop(tree);

    drop_file_cache(dir.path());
    let tree = TreeBuilder::new(dir.path()).open().unwrap();
    let cold_put = elapsed(|| {
        for &idx in indices {
            let (k, v) = &pairs[idx];
            tree.put(black_box(k), black_box(v)).unwrap();
        }
    });
    black_box(tree.stats().unwrap());

    EngineReport { cold_get, cold_put }
}

fn bench_rocksdb(pairs: &[(Vec<u8>, Vec<u8>)], indices: &[usize]) -> EngineReport {
    let dir = TempDir::new().unwrap();
    {
        let db = open_rocksdb(dir.path());
        let wo = rocksdb_write_opts_persistent();
        for (k, v) in pairs {
            db.put_opt(k, v, &wo).unwrap();
        }
        db.flush_wal(true).unwrap();
        db.flush().unwrap();
    }

    drop_file_cache(dir.path());
    let db = open_rocksdb(dir.path());
    let cold_get = elapsed(|| {
        for &idx in indices {
            let (k, v) = &pairs[idx];
            assert_eq!(db.get(black_box(k)).unwrap().as_deref(), Some(v.as_slice()));
        }
    });
    drop(db);

    drop_file_cache(dir.path());
    let db = open_rocksdb(dir.path());
    let wo = rocksdb_write_opts_persistent();
    let cold_put = elapsed(|| {
        for &idx in indices {
            let (k, v) = &pairs[idx];
            db.put_opt(black_box(k), black_box(v), &wo).unwrap();
        }
    });

    EngineReport { cold_get, cold_put }
}

fn bench_sqlite(pairs: &[(Vec<u8>, Vec<u8>)], indices: &[usize]) -> EngineReport {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bench.db");
    {
        let mut conn = open_sqlite(&path);
        preload_sqlite(&mut conn, pairs);
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }

    drop_file_cache(dir.path());
    let conn = open_sqlite(&path);
    let cold_get = elapsed(|| {
        let mut stmt = conn.prepare_cached("SELECT v FROM kv WHERE k = ?").unwrap();
        for &idx in indices {
            let (k, v) = &pairs[idx];
            let got: Vec<u8> = stmt
                .query_row(params![black_box(k).as_slice()], |row| row.get(0))
                .unwrap();
            assert_eq!(got.as_slice(), v.as_slice());
        }
    });
    drop(conn);

    drop_file_cache(dir.path());
    let conn = open_sqlite(&path);
    let cold_put = elapsed(|| {
        let mut stmt = conn
            .prepare_cached("INSERT OR REPLACE INTO kv (k, v) VALUES (?, ?)")
            .unwrap();
        for &idx in indices {
            let (k, v) = &pairs[idx];
            stmt.execute(params![black_box(k).as_slice(), black_box(v).as_slice()])
                .unwrap();
        }
    });

    EngineReport { cold_get, cold_put }
}

fn open_rocksdb(path: &Path) -> DB {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_write_buffer_size(64 * 1024 * 1024);
    opts.set_max_write_buffer_number(2);
    opts.set_compression_type(rocksdb::DBCompressionType::None);
    DB::open(&opts, path).unwrap()
}

fn rocksdb_write_opts_persistent() -> WriteOptions {
    let mut wo = WriteOptions::default();
    wo.disable_wal(false);
    wo.set_sync(false);
    wo
}

fn open_sqlite(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;\n\
         PRAGMA synchronous = NORMAL;\n\
         PRAGMA cache_size = -65536;\n\
         CREATE TABLE IF NOT EXISTS kv (k BLOB PRIMARY KEY, v BLOB) WITHOUT ROWID;",
    )
    .unwrap();
    conn
}

fn preload_sqlite(conn: &mut Connection, pairs: &[(Vec<u8>, Vec<u8>)]) {
    let tx = conn.transaction().unwrap();
    {
        let mut stmt = tx
            .prepare("INSERT OR REPLACE INTO kv (k, v) VALUES (?, ?)")
            .unwrap();
        for (k, v) in pairs {
            stmt.execute(params![k.as_slice(), v.as_slice()]).unwrap();
        }
    }
    tx.commit().unwrap();
}

fn drop_file_cache(root: &Path) {
    let Ok(files) = collect_files(root) else {
        return;
    };
    for path in files {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let _ = file.sync_all();
        #[cfg(target_os = "linux")]
        unsafe {
            let _ = libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
}

fn collect_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(root, &mut out)?;
    Ok(out)
}

fn collect_files_inner(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let meta = fs::metadata(path)?;
    if meta.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_files_inner(&entry?.path(), out)?;
        }
    }
    Ok(())
}

fn cache_drop_mode() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "sync_all+posix_fadvise(DONTNEED)"
    }
    #[cfg(not(target_os = "linux"))]
    {
        "sync_all only"
    }
}

fn progress(msg: impl AsRef<str>) {
    println!("# {}", msg.as_ref());
    let _ = std::io::stdout().flush();
}

fn gen_dataset(workload: Workload, n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    match workload {
        Workload::Kv => gen_kv_dataset(n),
        Workload::Objstore => gen_objstore_dataset(n),
        Workload::Fs => gen_fs_dataset(n),
    }
}

fn gen_kv_dataset(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut x = SEED;
    let mut pairs = Vec::with_capacity(n);
    for _ in 0..n {
        let mut k = vec![0u8; KV_KEY_LEN];
        let mut v = vec![0u8; KV_VAL_LEN];
        fill_bytes(&mut x, &mut k);
        fill_bytes(&mut x, &mut v);
        pairs.push((k, v));
    }
    pairs
}

fn gen_objstore_dataset(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let files_per_bucket = n.div_ceil(OBJSTORE_BUCKETS);
    let mut pairs = Vec::with_capacity(n);
    for b in 0..OBJSTORE_BUCKETS {
        for f in 0..files_per_bucket {
            if pairs.len() == n {
                break;
            }
            let key = format!("bucket-{b:02}/path/sub/file-{f:06}.bin").into_bytes();
            let value = format!(
                "{{\"size\":{:08},\"etag\":\"{:08x}\",\"class\":\"STD\"}}",
                f * 1000 + b * 100,
                (b.wrapping_mul(1000).wrapping_add(f)) as u32,
            )
            .into_bytes();
            pairs.push((key, value));
        }
    }
    pairs
}

fn gen_fs_dataset(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let files_per_dir = n.div_ceil(FS_DIRS);
    let mut pairs = Vec::with_capacity(n);
    for d in 0..FS_DIRS {
        for f in 0..files_per_dir {
            if pairs.len() == n {
                break;
            }
            let key = format!("/usr/local/share/category-{d}/file-{f:06}").into_bytes();
            let mut value = Vec::with_capacity(32);
            value.extend_from_slice(&((f as u64) * 1024).to_le_bytes());
            value.extend_from_slice(&(1_700_000_000u64 + f as u64).to_le_bytes());
            value.extend_from_slice(&0o644u32.to_le_bytes());
            value.extend_from_slice(&1000u32.to_le_bytes());
            value.extend_from_slice(&1000u32.to_le_bytes());
            value.extend_from_slice(&1u32.to_le_bytes());
            pairs.push((key, value));
        }
    }
    pairs
}

fn fill_bytes(state: &mut u64, out: &mut [u8]) {
    for chunk in out.chunks_mut(8) {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let bytes = state.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

fn index_sequence(keys: usize, ops: usize, seed: u64) -> Vec<usize> {
    let mut x = seed;
    let mut out = Vec::with_capacity(ops);
    for _ in 0..ops {
        x = x
            .wrapping_mul(2_862_933_555_777_941_757)
            .wrapping_add(3_037_000_493);
        out.push(((x >> 32) as usize) % keys);
    }
    out
}

fn elapsed(f: impl FnOnce()) -> Duration {
    let start = Instant::now();
    f();
    start.elapsed()
}

fn print_row(
    workload: &str,
    op: &str,
    holt: Duration,
    rocks: Duration,
    sqlite: Duration,
    ops: usize,
) {
    let h = ns_per_op(holt, ops);
    let r = ns_per_op(rocks, ops);
    let s = ns_per_op(sqlite, ops);
    println!(
        "{workload:<9} {op:<9} {h:>12.0} {r:>12.0} {s:>12.0} {rocks_vs:>9.2}x {sqlite_vs:>9.2}x",
        rocks_vs = r / h,
        sqlite_vs = s / h,
    );
}

fn ns_per_op(elapsed: Duration, ops: usize) -> f64 {
    elapsed.as_nanos() as f64 / ops as f64
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
