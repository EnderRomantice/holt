//! `Tree::put_many_if_absent` — same-prefix batch create.

use holt::{Durability, PutOutcome, Tree, TreeConfig};
use tempfile::tempdir;

#[test]
fn put_many_if_absent_creates_all_fresh() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    let owned: Vec<(Vec<u8>, Vec<u8>)> = (0..200u32)
        .map(|i| {
            (
                format!("dir/f{i:04}").into_bytes(),
                format!("v{i}").into_bytes(),
            )
        })
        .collect();
    let entries: Vec<(&[u8], &[u8])> = owned
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();

    let out = tree.put_many_if_absent(&entries).unwrap();
    assert_eq!(out.len(), 200);
    assert!(out.iter().all(|o| *o == PutOutcome::Created));
    for (k, v) in &owned {
        assert_eq!(tree.get(k).unwrap().as_deref(), Some(v.as_slice()));
    }
}

#[test]
fn put_many_if_absent_reports_existing() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    tree.put(b"dir/a", b"old").unwrap();
    tree.put(b"dir/c", b"old").unwrap();

    let out = tree
        .put_many_if_absent(&[
            (b"dir/a".as_slice(), b"new".as_slice()), // exists
            (b"dir/b".as_slice(), b"new".as_slice()), // fresh
            (b"dir/c".as_slice(), b"new".as_slice()), // exists
            (b"dir/d".as_slice(), b"new".as_slice()), // fresh
        ])
        .unwrap();
    assert_eq!(
        out,
        vec![
            PutOutcome::AlreadyExists,
            PutOutcome::Created,
            PutOutcome::AlreadyExists,
            PutOutcome::Created,
        ],
    );
    // Existing keys keep their value; only the fresh keys were written.
    assert_eq!(tree.get(b"dir/a").unwrap().as_deref(), Some(&b"old"[..]));
    assert_eq!(tree.get(b"dir/b").unwrap().as_deref(), Some(&b"new"[..]));
    assert_eq!(tree.get(b"dir/c").unwrap().as_deref(), Some(&b"old"[..]));
    assert_eq!(tree.get(b"dir/d").unwrap().as_deref(), Some(&b"new"[..]));
}

#[test]
fn put_many_if_absent_dedups_within_batch() {
    let tree = Tree::open(TreeConfig::memory()).unwrap();
    let out = tree
        .put_many_if_absent(&[
            (b"k".as_slice(), b"first".as_slice()),
            (b"k".as_slice(), b"second".as_slice()), // duplicate
        ])
        .unwrap();
    assert_eq!(out, vec![PutOutcome::Created, PutOutcome::AlreadyExists]);
    assert_eq!(tree.get(b"k").unwrap().as_deref(), Some(&b"first"[..]));
}

#[test]
fn put_many_if_absent_is_crash_atomic() {
    let dir = tempdir().unwrap();
    let mut cfg = TreeConfig::new(dir.path());
    cfg.durability = Durability::Wal { sync: true };
    cfg.checkpoint.enabled = false;

    const N: u32 = 2000;
    let value = vec![0xAB_u8; 100];
    {
        let tree = Tree::open(cfg.clone()).unwrap();
        let owned: Vec<(Vec<u8>, Vec<u8>)> = (0..N)
            .map(|i| (format!("k{i:08}").into_bytes(), value.clone()))
            .collect();
        let entries: Vec<(&[u8], &[u8])> = owned
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        tree.put_many_if_absent(&entries).unwrap();
        // Drop without a checkpoint — the single batch WAL record is the
        // recovery commit point.
    }

    let tree = Tree::open(cfg).unwrap();
    for i in 0..N {
        assert_eq!(
            tree.get(format!("k{i:08}").as_bytes()).unwrap().as_deref(),
            Some(&value[..]),
            "key {i} after WAL replay",
        );
    }
}
