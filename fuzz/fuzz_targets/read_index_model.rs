#![no_main]

use std::collections::{BTreeMap, BTreeSet};

use arbitrary::{Arbitrary, Result as ArbitraryResult, Unstructured};
use holt::{KeyRangeEntry, Tree, TreeConfig};
use libfuzzer_sys::fuzz_target;

const MAX_ACTIONS: usize = 48;
const DIRS: u8 = 8;
const PREFIXES: u8 = DIRS + 2;
const FILES_PER_DIR: u16 = 256;
const DELIMITERS: [u8; 6] = [b'/', b':', b'|', b'#', b'@', b'\\'];

#[derive(Debug)]
struct Input {
    actions: Vec<Action>,
}

#[derive(Debug)]
enum Action {
    Get { dir: u8, file: u16 },
    GetMissing { dir: u8, file: u16 },
    PrefixEmpty { dir: u8 },
    ScanDelimiter { dir: u8, delimiter: u8 },
    Delete { dir: u8, file: u16 },
    Put { dir: u8, file: u16, rev: u8 },
    CheckpointReopen,
    Reopen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpectedKeyEntry {
    Key(Vec<u8>),
    CommonPrefix(Vec<u8>),
}

impl<'a> Arbitrary<'a> for Input {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        let len = u.int_in_range(0..=MAX_ACTIONS)?;
        let mut actions = Vec::with_capacity(len);
        for _ in 0..len {
            actions.push(Action::arbitrary(u)?);
        }
        Ok(Self { actions })
    }
}

impl<'a> Arbitrary<'a> for Action {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        Ok(match u.int_in_range(0..=7u8)? {
            0 => Self::Get {
                dir: live_dir_id(u)?,
                file: file_id(u)?,
            },
            1 => Self::GetMissing {
                dir: prefix_id(u)?,
                file: file_id(u)?,
            },
            2 => Self::PrefixEmpty { dir: prefix_id(u)? },
            3 => Self::ScanDelimiter {
                dir: prefix_id(u)?,
                delimiter: delimiter_id(u)?,
            },
            4 => Self::Delete {
                dir: live_dir_id(u)?,
                file: file_id(u)?,
            },
            5 => Self::Put {
                dir: live_dir_id(u)?,
                file: u.int_in_range(0..=FILES_PER_DIR + 32)?,
                rev: u.int_in_range(0..=u8::MAX)?,
            },
            6 => Self::CheckpointReopen,
            _ => Self::Reopen,
        })
    }
}

fn live_dir_id(u: &mut Unstructured<'_>) -> ArbitraryResult<u8> {
    u.int_in_range(0..=DIRS - 1)
}

fn prefix_id(u: &mut Unstructured<'_>) -> ArbitraryResult<u8> {
    u.int_in_range(0..=PREFIXES - 1)
}

fn delimiter_id(u: &mut Unstructured<'_>) -> ArbitraryResult<u8> {
    u.int_in_range(0..=DELIMITERS.len() as u8 - 1)
}

fn file_id(u: &mut Unstructured<'_>) -> ArbitraryResult<u16> {
    u.int_in_range(0..=FILES_PER_DIR - 1)
}

fn delimiter(id: u8) -> u8 {
    DELIMITERS[id as usize % DELIMITERS.len()]
}

fn key(dir: u8, file: u16) -> Vec<u8> {
    format!(
        "meta/{:02}/tenant:{:02}|bucket#{:02}@shard\\part/{:04}",
        dir % DIRS,
        dir % 4,
        file % 16,
        file
    )
    .into_bytes()
}

fn missing_key(dir: u8, file: u16) -> Vec<u8> {
    if dir < DIRS {
        format!("meta/{:02}/missing:{:04}", dir, file + FILES_PER_DIR).into_bytes()
    } else {
        format!("absent/{:02}/missing:{:04}", dir - DIRS, file).into_bytes()
    }
}

fn prefix(dir: u8) -> Vec<u8> {
    if dir < DIRS {
        format!("meta/{:02}/", dir).into_bytes()
    } else {
        format!("absent/{:02}/", dir - DIRS).into_bytes()
    }
}

fn value(dir: u8, file: u16, rev: u8) -> Vec<u8> {
    let mut out = vec![rev; 256];
    out[0] = dir;
    out[1] = (file & 0xff) as u8;
    out[2] = (file >> 8) as u8;
    out
}

fn preload(tree: &Tree) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut model = BTreeMap::new();
    for dir in 0..DIRS {
        for file in 0..FILES_PER_DIR {
            let key = key(dir, file);
            let value = value(dir, file, 0);
            tree.put(&key, &value).unwrap();
            model.insert(key, value);
        }
    }
    model
}

fn expected_key_entries(
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    dir: u8,
    delimiter: u8,
) -> Vec<ExpectedKeyEntry> {
    let prefix = prefix(dir);
    let mut emitted_prefixes = BTreeSet::new();
    let mut expected = Vec::new();
    for key in model.keys().filter(|key| key.starts_with(&prefix)) {
        if let Some(pos) = key[prefix.len()..]
            .iter()
            .position(|byte| *byte == delimiter)
        {
            let common = key[..prefix.len() + pos + 1].to_vec();
            if emitted_prefixes.insert(common.clone()) {
                expected.push(ExpectedKeyEntry::CommonPrefix(common));
            }
        } else {
            expected.push(ExpectedKeyEntry::Key(key.clone()));
        }
    }
    expected
}

fn assert_prefix_empty(tree: &Tree, model: &BTreeMap<Vec<u8>, Vec<u8>>, dir: u8) {
    let prefix = prefix(dir);
    let expected = !model.keys().any(|key| key.starts_with(&prefix));
    assert_eq!(tree.is_prefix_empty(&prefix).unwrap(), expected);
}

fn assert_scan_delimiter(
    tree: &Tree,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    dir: u8,
    delimiter: u8,
) {
    let prefix = prefix(dir);
    let expected = expected_key_entries(model, dir, delimiter);
    let got: Vec<_> = tree
        .scan_keys(&prefix)
        .delimiter(delimiter)
        .into_iter()
        .map(|entry| match entry.unwrap() {
            KeyRangeEntry::Key { key, version } => {
                assert_eq!(tree.get_record(&key).unwrap().unwrap().version, version);
                ExpectedKeyEntry::Key(key)
            }
            KeyRangeEntry::CommonPrefix(prefix) => ExpectedKeyEntry::CommonPrefix(prefix),
            _ => panic!("KeyRangeEntry got a new variant"),
        })
        .collect();
    assert_eq!(got, expected);
}

fuzz_target!(|input: Input| {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = TreeConfig::new(dir.path());
    cfg.buffer_pool_size = 4;
    cfg.durability = holt::Durability::Wal { sync: true };
    cfg.checkpoint.enabled = false;

    let mut tree = Tree::open(cfg.clone()).unwrap();
    let mut model = preload(&tree);
    tree.checkpoint().unwrap();
    drop(tree);
    tree = Tree::open(cfg.clone()).unwrap();

    for action in input.actions {
        match action {
            Action::Get { dir, file } => {
                let key = key(dir, file);
                assert_eq!(tree.get(&key).unwrap(), model.get(&key).cloned());
            }
            Action::GetMissing { dir, file } => {
                assert!(tree.get(&missing_key(dir, file)).unwrap().is_none());
            }
            Action::PrefixEmpty { dir } => assert_prefix_empty(&tree, &model, dir),
            Action::ScanDelimiter { dir, delimiter } => {
                assert_scan_delimiter(&tree, &model, dir, self::delimiter(delimiter));
            }
            Action::Delete { dir, file } => {
                let key = key(dir, file);
                assert_eq!(tree.delete(&key).unwrap(), model.remove(&key).is_some());
            }
            Action::Put { dir, file, rev } => {
                let key = key(dir, file);
                let value = value(dir, file, rev);
                tree.put(&key, &value).unwrap();
                model.insert(key, value);
            }
            Action::CheckpointReopen => {
                tree.checkpoint().unwrap();
                drop(tree);
                tree = Tree::open(cfg.clone()).unwrap();
            }
            Action::Reopen => {
                drop(tree);
                tree = Tree::open(cfg.clone()).unwrap();
            }
        }
    }
});
