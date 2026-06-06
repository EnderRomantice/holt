//! StateMachine-mode atomic batches take the mutation gate shared (so
//! they don't fence concurrent scans), yet stay logically atomic.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use holt::{Durability, RangeEntry, TreeConfig, DB};

fn sm_db() -> DB {
    let mut cfg = TreeConfig::memory();
    cfg.durability = Durability::StateMachine;
    DB::open(cfg).expect("open state-machine DB")
}

#[test]
fn sm_atomic_batch_is_all_or_none() {
    let db = sm_db();
    let t = db.create_tree("t").unwrap();
    t.put(b"a", b"0").unwrap();

    // A failing conditional guard rolls the whole batch back — even
    // though the gate is now shared, atomicity is unchanged.
    let committed = db
        .atomic(|b| {
            b.put("t", b"a", b"1");
            b.put_if_absent("t", b"a", b"x"); // `a` exists -> guard fails
            b.put("t", b"b", b"2");
        })
        .unwrap();
    assert!(!committed);
    assert_eq!(t.get(b"a").unwrap(), Some(b"0".to_vec()));
    assert_eq!(t.get(b"b").unwrap(), None);

    // A satisfiable batch commits every op.
    let committed = db
        .atomic(|b| {
            b.put("t", b"a", b"1");
            b.put("t", b"b", b"2");
        })
        .unwrap();
    assert!(committed);
    assert_eq!(t.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(t.get(b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn sm_scan_runs_concurrently_with_batches_without_tearing() {
    let db = Arc::new(sm_db());
    let writer_tree = db.create_tree("d").unwrap();
    const N: u32 = 64;
    for i in 0..N {
        writer_tree
            .put(format!("k{i:03}").as_bytes(), b"v0")
            .unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));

    // Writer: atomic batches that rewrite every key to a new generation.
    // Under the relaxed gate this runs concurrently with the scanners.
    let writer = {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut gen = 1u32;
            while !stop.load(Ordering::Relaxed) {
                let value = format!("v{gen}");
                db.atomic(|b| {
                    for i in 0..N {
                        b.put("d", format!("k{i:03}").as_bytes(), value.as_bytes());
                    }
                })
                .unwrap();
                gen = gen.wrapping_add(1);
            }
        })
    };

    // Readers scan concurrently. With a shared-gate batch a scan may see
    // an intermediate cross-key generation, but never a torn value — each
    // value must be a committed `v{n}`. No deadlock => the joins return.
    let mut readers = Vec::new();
    for _ in 0..3 {
        let tree = db.open_tree("d").unwrap();
        readers.push(thread::spawn(move || {
            for _ in 0..400 {
                for entry in tree.scan(b"k") {
                    if let RangeEntry::Key { value, .. } = entry.unwrap() {
                        assert_eq!(value[0], b'v', "torn value {value:?}");
                        std::str::from_utf8(&value[1..])
                            .unwrap()
                            .parse::<u32>()
                            .expect("value is a committed generation");
                    }
                }
            }
        }));
    }

    for reader in readers {
        reader.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}
