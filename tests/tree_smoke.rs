//! End-to-end smoke tests driving the public `Tree` API.
//!
//! Exercises only the public surface so signature breakage shows
//! up here first.

use std::sync::Arc;

use artisan::{Backend, MemoryBackend, Tree, TreeBuilder};

#[test]
fn open_in_memory_then_get_on_empty_tree_returns_none() {
    let tree = Tree::open_in_memory().unwrap();
    assert!(tree.get(b"anything").unwrap().is_none());
    assert!(tree.get(b"").unwrap().is_none());
}

#[test]
fn builder_in_memory_path_works() {
    let tree = TreeBuilder::new("(in-memory)")
        .buffer_pool_size(32)
        .open_in_memory()
        .unwrap();
    assert!(tree.get(b"x").unwrap().is_none());
}

#[test]
fn open_with_explicit_backend_round_trips_root_blob() {
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());

    // First open: should bootstrap the root blob.
    let _t = TreeBuilder::new("ignored")
        .open_with_backend(backend.clone())
        .unwrap();
    let blobs_after_first = backend.list_blobs().unwrap().len();
    assert!(blobs_after_first >= 1, "root blob should be present");

    // Second open against the same backend: should not duplicate.
    let _t2 = TreeBuilder::new("ignored")
        .open_with_backend(backend.clone())
        .unwrap();
    assert_eq!(
        backend.list_blobs().unwrap().len(),
        blobs_after_first,
        "re-open must not allocate a fresh root"
    );
}

#[test]
fn checkpoint_is_idempotent_on_memory_backend() {
    let tree = Tree::open_in_memory().unwrap();
    tree.checkpoint().unwrap();
    tree.checkpoint().unwrap();
    assert!(tree.get(b"k").unwrap().is_none());
}

#[test]
fn put_then_get_round_trip() {
    let tree = Tree::open_in_memory().unwrap();
    assert!(tree.put(b"hello", b"world").unwrap().is_none());
    assert_eq!(tree.get(b"hello").unwrap().as_deref(), Some(&b"world"[..]));
    assert!(tree.get(b"missing").unwrap().is_none());
}

#[test]
fn put_returns_previous_value_on_update() {
    let tree = Tree::open_in_memory().unwrap();
    assert!(tree.put(b"k", b"v1").unwrap().is_none());
    assert_eq!(tree.put(b"k", b"v2").unwrap().as_deref(), Some(&b"v1"[..]));
    assert_eq!(tree.get(b"k").unwrap().as_deref(), Some(&b"v2"[..]));
}

#[test]
fn many_keys_all_readable_via_public_api() {
    let tree = Tree::open_in_memory().unwrap();
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..100u32)
        .map(|i| (format!("img/{i:04}.jpg").into_bytes(), format!("blob#{i}").into_bytes()))
        .collect();
    for (k, v) in &pairs {
        tree.put(k, v).unwrap();
    }
    for (k, v) in &pairs {
        assert_eq!(tree.get(k).unwrap().as_deref(), Some(&v[..]));
    }
}

#[test]
fn concurrent_writers_serialised_by_internal_lock() {
    use std::sync::Arc;
    use std::thread;

    let tree = Arc::new(Tree::open_in_memory().unwrap());
    let handles: Vec<_> = (0..8u8)
        .map(|t| {
            let tree = tree.clone();
            thread::spawn(move || {
                for i in 0..25u32 {
                    let k = format!("t{t}/k{i:03}").into_bytes();
                    let v = format!("v{t}-{i}").into_bytes();
                    tree.put(&k, &v).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // All 8 * 25 = 200 keys readable.
    for t in 0..8u8 {
        for i in 0..25u32 {
            let k = format!("t{t}/k{i:03}").into_bytes();
            let v = format!("v{t}-{i}").into_bytes();
            assert_eq!(tree.get(&k).unwrap().as_deref(), Some(&v[..]));
        }
    }
}

#[test]
fn strict_prefix_key_pair_surfaces_not_yet_implemented() {
    let tree = Tree::open_in_memory().unwrap();
    tree.put(b"abc", b"v1").unwrap();
    let r = tree.put(b"abcdef", b"v2");
    assert!(matches!(r, Err(artisan::Error::NotYetImplemented(_))));
}

// ----------------------------------------------------------------
// Tagged-value (inline / external) API
// ----------------------------------------------------------------

#[test]
fn put_inline_then_get_value_round_trip() {
    use artisan::Value;
    let tree = Tree::open_in_memory().unwrap();
    tree.put_inline(b"meta/foo", b"hello world").unwrap();
    match tree.get_value(b"meta/foo").unwrap() {
        Some(Value::Inline(b)) => assert_eq!(b, b"hello world"),
        other => panic!("expected Inline, got {other:?}"),
    }
}

#[test]
fn put_ref_then_get_value_round_trip() {
    use artisan::Value;
    let tree = Tree::open_in_memory().unwrap();
    tree.put_ref(b"img/big.png", "s3://photos/big.png").unwrap();
    match tree.get_value(b"img/big.png").unwrap() {
        Some(Value::External(url)) => assert_eq!(url, "s3://photos/big.png"),
        other => panic!("expected External, got {other:?}"),
    }
}

#[test]
fn put_value_replaces_and_returns_previous_with_tag() {
    use artisan::Value;
    let tree = Tree::open_in_memory().unwrap();
    let prev1 = tree.put_inline(b"k", b"v1").unwrap();
    assert!(prev1.is_none());

    // Replace inline value with an external reference.
    let prev2 = tree.put_ref(b"k", "https://example.org/v2").unwrap();
    assert_eq!(prev2, Some(Value::Inline(b"v1".to_vec())));

    // And read it back as External.
    assert_eq!(
        tree.get_value(b"k").unwrap(),
        Some(Value::External("https://example.org/v2".to_owned()))
    );
}

#[test]
fn mixed_inline_and_external_workload() {
    let tree = Tree::open_in_memory().unwrap();

    // 30 small entries inline + 30 big-file refs.
    for i in 0..30u32 {
        let k = format!("inode/small/{i:04}").into_bytes();
        tree.put_inline(&k, format!("size=42,mtime=now,owner=user{i}").as_bytes()).unwrap();
    }
    for i in 0..30u32 {
        let k = format!("inode/big/{i:04}").into_bytes();
        tree.put_ref(&k, format!("s3://bucket/blob/{i:04}.bin")).unwrap();
    }

    // Read back, assert variant matches.
    for i in 0..30u32 {
        let k = format!("inode/small/{i:04}").into_bytes();
        let v = tree.get_value(&k).unwrap().unwrap();
        assert!(v.is_inline(), "key {k:?} should be Inline");
    }
    for i in 0..30u32 {
        let k = format!("inode/big/{i:04}").into_bytes();
        let v = tree.get_value(&k).unwrap().unwrap();
        let url = v.as_external().expect("should be External");
        assert_eq!(url, format!("s3://bucket/blob/{i:04}.bin"));
    }
}

#[test]
fn get_value_on_missing_key_returns_none() {
    let tree = Tree::open_in_memory().unwrap();
    assert!(tree.get_value(b"missing").unwrap().is_none());
}

#[test]
fn raw_put_then_get_value_errors_on_unknown_tag() {
    // If a caller writes raw bytes via put(), get_value should
    // surface the decode error rather than silently misinterpret.
    let tree = Tree::open_in_memory().unwrap();
    tree.put(b"k", &[0x99, 0xAB, 0xCD]).unwrap();
    let r = tree.get_value(b"k");
    assert!(matches!(r, Err(artisan::Error::InvalidValueEncoding { .. })));
}
