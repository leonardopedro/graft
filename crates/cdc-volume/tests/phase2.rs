//! Phase 2 acceptance tests: copy-on-write B-tree, replay loop, read counting.

use std::path::Path;

use cdc_volume::{BTree, CdcVolume, ChunkCodec, ChunkKey, ChunkMeta, PathCodec};
use graft::{
    core::{PageIdx, VolumeId, page::Page},
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft},
    volume_writer::VolumeWrite,
};

const TOTAL: usize = 100_000;
const BATCH: usize = 10_000;

fn runtime(dir: &Path) -> Runtime {
    setup_graft(GraftConfig {
        remote: RemoteConfig::Memory,
        data_dir: dir.to_path_buf(),
        autosync: None,
    })
    .expect("graft setup")
}

fn open_fresh(rt: &Runtime) -> (VolumeId, CdcVolume) {
    let vid = rt.volume_open(None, None, None).expect("open volume").vid;
    let vol = CdcVolume::init(rt.clone(), vid.clone(), &[42u8; 32]).expect("init cdc volume");
    (vid, vol)
}

fn key(i: usize) -> ChunkKey {
    ChunkKey(*blake3::hash(&(i as u64).to_le_bytes()).as_bytes())
}

fn meta(i: usize) -> ChunkMeta {
    ChunkMeta {
        start: 1_000 + i as u32,
        frames: 1,
        raw_len: 32,
    }
}

#[test]
fn hundred_thousand_keys_over_many_commits() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (_vid, vol) = open_fresh(&rt);

    for batch in 0..TOTAL.div_ceil(BATCH) {
        let start = batch * BATCH;
        let end = (start + BATCH).min(TOTAL);
        vol.update(|txn| -> cdc_volume::Result<()> {
            let mut tree = BTree::<ChunkCodec>::new(txn.chunk_root());
            {
                let (io, alloc) = txn.parts();
                for i in start..end {
                    tree.insert(io, alloc, key(i), meta(i))?;
                }
                // Free the pages this transaction superseded.
                tree.commit(io, alloc)?;
            }
            txn.set_chunk_root(tree.root());
            Ok(())
        })
        .unwrap();
    }

    let sb = vol.superblock().unwrap();
    assert_eq!(sb.generation, (TOTAL.div_ceil(BATCH)) as u64);

    // Copy-on-write plus the free list keep the watermark bounded; without
    // page reuse this would be one page per touched node per commit (~61k).
    assert!(
        sb.next_page < 10_000,
        "watermark grew to {} pages",
        sb.next_page
    );

    let reader = vol.reader().unwrap();
    let tree = BTree::<ChunkCodec>::new(sb.chunk_root_idx());
    assert!(
        tree.height(&reader).unwrap() <= 3,
        "tree is deeper than expected"
    );

    // Point lookups: one frame read per level, at most five.
    reader.reset_counters();
    let samples = 400;
    for i in (0..TOTAL).step_by(TOTAL / samples) {
        assert_eq!(tree.get(&reader, &key(i)).unwrap(), Some(meta(i)));
    }
    let reads_per_lookup = reader.reads() as f64 / samples as f64;
    println!(
        "100k keys: height {}, {reads_per_lookup} frame reads per point lookup, {} pages used",
        tree.height(&reader).unwrap(),
        sb.next_page
    );
    assert!(
        reads_per_lookup <= 5.0,
        "{reads_per_lookup} frame reads per point lookup"
    );

    // The whole tree is readable in order, with every key present exactly once.
    let all = tree.scan_all(&reader).unwrap();
    assert_eq!(all.len(), TOTAL);
    assert!(all.windows(2).all(|pair| pair[0].0 < pair[1].0));

    // A missing key is a miss, not an error.
    assert_eq!(tree.get(&reader, &ChunkKey([0xFF; 32])).unwrap(), None);
}

#[test]
fn path_tree_supports_ordered_range_scans_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (_vid, vol) = open_fresh(&rt);

    let entries: Vec<(Vec<u8>, Vec<u8>)> = [
        ("a/1", "one"),
        ("a/2", "two"),
        ("a/2/nested", "deep"),
        ("b/1", "bee"),
        ("c", "cee"),
    ]
    .into_iter()
    .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
    .collect();

    vol.update(|txn| -> cdc_volume::Result<()> {
        let mut tree = BTree::<PathCodec>::new(txn.path_root());
        {
            let (io, alloc) = txn.parts();
            for (k, v) in &entries {
                tree.insert(io, alloc, k.clone(), v.clone())?;
            }
            tree.commit(io, alloc)?;
        }
        txn.set_path_root(tree.root());
        Ok(())
    })
    .unwrap();

    let reader = vol.reader().unwrap();
    let root = vol.superblock().unwrap().path_root_idx();
    let tree = BTree::<PathCodec>::new(root);

    // Prefix listing is a range scan with byte-wise bounds.
    let mut listed = Vec::new();
    let lo = b"a/".to_vec();
    let hi = b"a0".to_vec();
    tree.scan(&reader, Some(&lo), Some(&hi), &mut |k, _| {
        listed.push(String::from_utf8(k.clone()).unwrap())
    })
    .unwrap();
    assert_eq!(listed, vec!["a/1", "a/2", "a/2/nested"]);

    // Delete one entry and confirm the tree still lists the rest in order.
    vol.update(|txn| -> cdc_volume::Result<()> {
        let mut tree = BTree::<PathCodec>::new(txn.path_root());
        {
            let (io, alloc) = txn.parts();
            assert!(tree.remove(io, alloc, &b"a/2".to_vec())?);
            assert!(!tree.remove(io, alloc, &b"nope".to_vec())?);
            tree.commit(io, alloc)?;
        }
        txn.set_path_root(tree.root());
        Ok(())
    })
    .unwrap();

    let reader = vol.reader().unwrap();
    let tree = BTree::<PathCodec>::new(vol.superblock().unwrap().path_root_idx());
    let all = tree.scan_all(&reader).unwrap();
    let keys: Vec<String> = all
        .iter()
        .map(|(k, _)| String::from_utf8(k.clone()).unwrap())
        .collect();
    assert_eq!(keys, vec!["a/1", "a/2/nested", "b/1", "c"]);
    assert_eq!(tree.get(&reader, &b"a/2".to_vec()).unwrap(), None);
}

#[test]
fn a_reader_pinned_before_a_commit_still_sees_the_old_tree() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (_vid, vol) = open_fresh(&rt);

    vol.update(|txn| -> cdc_volume::Result<()> {
        let mut tree = BTree::<ChunkCodec>::new(txn.chunk_root());
        {
            let (io, alloc) = txn.parts();
            tree.insert(io, alloc, key(1), meta(1))?;
            tree.commit(io, alloc)?;
        }
        txn.set_chunk_root(tree.root());
        Ok(())
    })
    .unwrap();

    let old_root = vol.superblock().unwrap().chunk_root_idx();
    let old_reader = vol.reader().unwrap();

    vol.update(|txn| -> cdc_volume::Result<()> {
        let mut tree = BTree::<ChunkCodec>::new(txn.chunk_root());
        {
            let (io, alloc) = txn.parts();
            tree.insert(io, alloc, key(2), meta(2))?;
            tree.commit(io, alloc)?;
        }
        txn.set_chunk_root(tree.root());
        Ok(())
    })
    .unwrap();

    let old_tree = BTree::<ChunkCodec>::new(old_root);
    assert_eq!(old_tree.get(&old_reader, &key(1)).unwrap(), Some(meta(1)));
    assert_eq!(
        old_tree.get(&old_reader, &key(2)).unwrap(),
        None,
        "the pinned reader must not observe the newer commit"
    );

    let new_reader = vol.reader().unwrap();
    let new_tree = BTree::<ChunkCodec>::new(vol.superblock().unwrap().chunk_root_idx());
    assert_eq!(new_tree.get(&new_reader, &key(2)).unwrap(), Some(meta(2)));
}

#[test]
fn a_stale_writer_conflicts_and_the_mutation_replays() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (vid, vol) = open_fresh(&rt);

    // Two writers from the same snapshot: the second commit must be refused.
    let mut first = rt.volume_writer(vid.clone()).unwrap();
    let second = rt.volume_writer(vid.clone()).unwrap();
    first
        .write_page(PageIdx::try_new(900).unwrap(), Page::EMPTY)
        .unwrap();
    first.commit().unwrap();
    let err = second.commit().unwrap_err();
    assert!(
        matches!(
            err,
            graft::GraftErr::Logical(graft::LogicalErr::VolumeConcurrentWrite(_))
        ),
        "expected VolumeConcurrentWrite, got {err:?}"
    );

    // The replay loop: the first attempt loses to an interfering writer, the
    // second attempt runs against a fresh snapshot and succeeds.
    let mut interfered = false;
    vol.update(|txn| -> cdc_volume::Result<()> {
        if !interfered {
            interfered = true;
            let mut other = rt.volume_writer(vid.clone())?;
            other.write_page(PageIdx::try_new(901).unwrap(), Page::EMPTY)?;
            other.commit()?;
        }

        let mut tree = BTree::<ChunkCodec>::new(txn.chunk_root());
        {
            let (io, alloc) = txn.parts();
            tree.insert(io, alloc, key(7), meta(7))?;
            tree.commit(io, alloc)?;
        }
        txn.set_chunk_root(tree.root());
        Ok(())
    })
    .unwrap();

    assert!(interfered);
    assert_eq!(vol.superblock().unwrap().generation, 1);

    let reader = vol.reader().unwrap();
    let tree = BTree::<ChunkCodec>::new(vol.superblock().unwrap().chunk_root_idx());
    assert_eq!(tree.get(&reader, &key(7)).unwrap(), Some(meta(7)));
}

#[test]
fn deleting_half_the_keys_keeps_the_tree_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (_vid, vol) = open_fresh(&rt);

    const N: usize = 5_000;

    vol.update(|txn| -> cdc_volume::Result<()> {
        let mut tree = BTree::<ChunkCodec>::new(txn.chunk_root());
        {
            let (io, alloc) = txn.parts();
            for i in 0..N {
                tree.insert(io, alloc, key(i), meta(i))?;
            }
            tree.commit(io, alloc)?;
        }
        txn.set_chunk_root(tree.root());
        Ok(())
    })
    .unwrap();

    vol.update(|txn| -> cdc_volume::Result<()> {
        let mut tree = BTree::<ChunkCodec>::new(txn.chunk_root());
        {
            let (io, alloc) = txn.parts();
            for i in (0..N).step_by(2) {
                assert!(tree.remove(io, alloc, &key(i))?);
            }
            tree.commit(io, alloc)?;
        }
        txn.set_chunk_root(tree.root());
        Ok(())
    })
    .unwrap();

    let reader = vol.reader().unwrap();
    let tree = BTree::<ChunkCodec>::new(vol.superblock().unwrap().chunk_root_idx());

    for i in 0..N {
        let got = tree.get(&reader, &key(i)).unwrap();
        if i % 2 == 0 {
            assert_eq!(got, None, "key {i} should be deleted");
        } else {
            assert_eq!(got, Some(meta(i)), "key {i} should survive");
        }
    }
    assert_eq!(tree.scan_all(&reader).unwrap().len(), N / 2);
}
