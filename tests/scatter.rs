//! `DB::scatter` — independent single-key conditional writes across
//! families, no atomic barrier, StateMachine-only.

use holt::{Durability, Error, Scatter, TreeConfig, DB};

fn sm_db() -> DB {
    let mut cfg = TreeConfig::memory();
    cfg.durability = Durability::StateMachine;
    DB::open(cfg).expect("open state-machine DB")
}

#[test]
fn scatter_requires_state_machine() {
    // Default durability is Wal — the non-atomic fan-out has no replay
    // to heal a torn multi-family write there, so scatter must refuse.
    let db = DB::open(TreeConfig::memory()).unwrap();
    db.create_tree("dentries").unwrap();
    let err = db
        .scatter(|s| s.put_if_absent("dentries", b"a", b"1"))
        .unwrap_err();
    assert!(matches!(err, Error::ScatterRequiresStateMachine));
    // ...and nothing was written.
    assert_eq!(db.open_tree("dentries").unwrap().get(b"a").unwrap(), None);
}

#[test]
fn scatter_empty_is_ok() {
    let db = sm_db();
    assert_eq!(db.scatter(|_| {}).unwrap(), Vec::<bool>::new());
}

#[test]
fn scatter_creates_across_families() {
    let db = sm_db();
    db.create_tree("dentries").unwrap();
    db.create_tree("inodes").unwrap();
    let applied = db
        .scatter(|s| {
            s.put_if_absent("dentries", b"dir/f", b"ino=7");
            s.put_if_absent("inodes", b"7", b"meta");
        })
        .unwrap();
    assert_eq!(applied, vec![true, true]);
    assert_eq!(
        db.open_tree("dentries").unwrap().get(b"dir/f").unwrap(),
        Some(b"ino=7".to_vec()),
    );
    assert_eq!(
        db.open_tree("inodes").unwrap().get(b"7").unwrap(),
        Some(b"meta".to_vec()),
    );
}

#[test]
fn scatter_independent_creates_across_families() {
    let db = sm_db();
    db.create_tree("dentries").unwrap();
    db.create_tree("inodes").unwrap();
    let applied = db
        .scatter_independent(|s| {
            s.put_if_absent("dentries", b"dir/f", b"ino=7");
            s.put_if_absent("inodes", b"7", b"meta");
        })
        .unwrap();
    assert_eq!(applied, vec![true, true]);
    assert_eq!(
        db.open_tree("dentries").unwrap().get(b"dir/f").unwrap(),
        Some(b"ino=7".to_vec()),
    );
    assert_eq!(
        db.open_tree("inodes").unwrap().get(b"7").unwrap(),
        Some(b"meta".to_vec()),
    );
}

#[test]
fn scatter_independent_rejects_duplicate_keys() {
    let db = sm_db();
    db.create_tree("dentries").unwrap();
    let err = db
        .scatter_independent(|s| {
            s.put_if_absent("dentries", b"dir/f", b"ino=7");
            s.put_if_absent("dentries", b"dir/f", b"ino=8");
        })
        .unwrap_err();
    assert!(matches!(
        err,
        Error::ScatterDuplicateKey {
            tree,
            key_len: 5
        } if tree == "dentries"
    ));
    assert_eq!(
        db.open_tree("dentries").unwrap().get(b"dir/f").unwrap(),
        None
    );
}

#[test]
fn scatter_reports_conflict_per_op() {
    let db = sm_db();
    let dentries = db.create_tree("dentries").unwrap();
    db.create_tree("inodes").unwrap();
    dentries.put(b"dir/f", b"old").unwrap(); // name already taken

    let applied = db
        .scatter(|s| {
            s.put_if_absent("dentries", b"dir/f", b"new"); // conflict -> false
            s.put_if_absent("inodes", b"9", b"meta"); // fresh   -> true
        })
        .unwrap();
    assert_eq!(applied, vec![false, true]);
    // The taken name keeps its value; the inode was still created.
    assert_eq!(dentries.get(b"dir/f").unwrap(), Some(b"old".to_vec()));
    assert_eq!(
        db.open_tree("inodes").unwrap().get(b"9").unwrap(),
        Some(b"meta".to_vec()),
    );
}

#[test]
fn scatter_is_idempotent_on_replay() {
    // Re-running the same scatter (as a log replay would) reports every
    // create as already-applied (`false`) without changing state — the
    // F4 idempotency contract.
    let db = sm_db();
    db.create_tree("dentries").unwrap();
    db.create_tree("inodes").unwrap();
    let build = |s: &mut Scatter| {
        s.put_if_absent("dentries", b"dir/f", b"ino=7");
        s.put_if_absent("inodes", b"7", b"meta");
    };
    assert_eq!(db.scatter(build).unwrap(), vec![true, true]);
    assert_eq!(db.scatter(build).unwrap(), vec![false, false]); // replay
    assert_eq!(
        db.open_tree("inodes").unwrap().get(b"7").unwrap(),
        Some(b"meta".to_vec()),
    );
}

#[test]
fn scatter_mixes_op_kinds_in_order() {
    let db = sm_db();
    let t = db.create_tree("t").unwrap();
    t.put(b"keep", b"v0").unwrap();
    t.put(b"cas", b"v0").unwrap();
    t.put(b"del", b"v0").unwrap();
    let cas_ver = t.get_version(b"cas").unwrap().unwrap();

    // Ops apply in buffer order, each releasing its locks before the
    // next — so op N+1 sees op N's effect (the put_if_absent below sees
    // the preceding put).
    let applied = db
        .scatter(|s| {
            s.put("t", b"keep", b"v1"); // always            -> true
            s.put_if_absent("t", b"keep", b"v2"); // now present     -> false
            s.compare_and_put("t", b"cas", cas_ver, b"v1"); // match  -> true
            s.delete("t", b"del"); // present                -> true
            s.delete("t", b"absent"); // absent              -> false
        })
        .unwrap();
    assert_eq!(applied, vec![true, false, true, true, false]);
    assert_eq!(t.get(b"keep").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(t.get(b"cas").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(t.get(b"del").unwrap(), None);
}
