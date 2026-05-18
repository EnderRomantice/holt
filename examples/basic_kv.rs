//! `basic_kv` example.
//!
//! Shows the smallest possible "open a tree, do a lookup" cycle.
//! As more of the engine lands (Stage 2b's `put`, Stage 2c's
//! `delete`, etc.) this example will grow accordingly.

use artisan::TreeBuilder;

fn main() {
    println!("=== artisan basic_kv example ===\n");

    // In-memory backend works on every platform. For the
    // persistent (NVMe + io_uring + O_DIRECT) backend on Linux,
    // replace `.open_in_memory()` with `.open()`.
    let tree = TreeBuilder::new("./artisan-data")
        .buffer_pool_size(64)
        .wal_sync_on_commit(false)
        .open_in_memory()
        .expect("open_in_memory");

    println!("Tree opened: {tree:?}");

    // Until `put` lands (Stage 2b), every lookup returns None.
    match tree.get(b"img/01.jpg").expect("get") {
        Some(v) => println!("img/01.jpg -> {} bytes", v.len()),
        None => println!("img/01.jpg -> not found (expected — `put` lands Stage 2b)"),
    }
}
