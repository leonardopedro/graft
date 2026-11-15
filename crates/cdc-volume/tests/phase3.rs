//! Phase 3 acceptance: chunking, dedup, manifests, ranges, listings.

use std::{io::Cursor, path::Path};

use cdc_volume::{
    BTree, CdcVolume, ChunkCodec, PageRead, Store, btree::ChunkKey, crypto::MASTER_KEY_LEN,
};
use graft::{
    core::VolumeId,
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft},
};

const KEY: [u8; MASTER_KEY_LEN] = [7u8; MASTER_KEY_LEN];

fn runtime(dir: &Path) -> Runtime {
    setup_graft(GraftConfig {
        remote: RemoteConfig::Memory,
        data_dir: dir.to_path_buf(),
        autosync: None,
    })
    .expect("graft setup")
}

fn new_store(rt: &Runtime) -> Store {
    let vid = rt.volume_open(None, None, None).expect("open volume").vid;
    Store::create(rt.clone(), vid, &KEY).expect("create store")
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}

fn chunk_entries(volume: &CdcVolume) -> usize {
    let reader = volume.reader().unwrap();
    let sb = reader.read_superblock().unwrap();
    BTree::<ChunkCodec>::new(sb.chunk_root_idx())
        .scan_all(&reader)
        .unwrap()
        .len()
}

fn live_pages(volume: &CdcVolume) -> u32 {
    volume.reader().unwrap().page_count().to_u32()
}

#[test]
fn roundtrip_including_empty_and_tiny() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let store = new_store(&rt);

    let cases: Vec<(&[u8], Vec<u8>)> = vec![
        (b"empty", Vec::new()),
        (b"one", vec![0xAB]),
        (b"tiny", b"hello, chunked world".to_vec()),
        (b"dir/nested", Rng::new(1).bytes(200 * 1024)),
        (b"page-aligned", Rng::new(2).bytes(4056)),
        (b"page-plus-one", Rng::new(3).bytes(4057)),
    ];

    for (path, data) in &cases {
        let stat = store.put_file(path, Cursor::new(data.clone())).unwrap();
        assert_eq!(stat.size, data.len() as u64);
        assert_eq!(stat.path, path.to_vec());
        assert_eq!(
            store.get_file(path).unwrap().as_deref(),
            Some(data.as_slice()),
            "{} roundtrip",
            String::from_utf8_lossy(path)
        );
        assert_eq!(
            store.stat(path).unwrap().unwrap().chunk_count,
            stat.chunk_count
        );
    }

    // Replacing an existing path serves the new bytes.
    let replacement = Rng::new(4).bytes(70 * 1024);
    store
        .put_file(b"tiny", Cursor::new(replacement.clone()))
        .unwrap();
    assert_eq!(store.get_file(b"tiny").unwrap().unwrap(), replacement);

    assert!(store.get_file(b"missing").unwrap().is_none());
    assert!(store.stat(b"missing").unwrap().is_none());
}

#[test]
fn range_reads_only_overlapping_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let store = new_store(&rt);

    let data = Rng::new(11).bytes(4 << 20);
    let stat = store.put_file(b"big", Cursor::new(data.clone())).unwrap();
    let total_frames = stat.size.div_ceil(4056);

    let snapshot = store.snapshot().unwrap();
    snapshot.reset_counters();
    let offset = 2 << 20;
    let len = 10 << 10;
    let got = snapshot.read_range(b"big", offset, len).unwrap();
    let reads = snapshot.reads();

    assert_eq!(got, data[offset as usize..(offset + len) as usize]);
    println!(
        "range read: {len} bytes from a {} byte file in {reads} frame reads ({total_frames} data frames total)",
        stat.size
    );
    assert!(
        reads < 40,
        "range read touched {reads} frames, which is not selective"
    );

    // Edges.
    assert_eq!(snapshot.read_range(b"big", 0, 0).unwrap(), Vec::<u8>::new());
    let tail = snapshot.read_range(b"big", stat.size - 10, 100).unwrap();
    assert_eq!(tail, data[data.len() - 10..]);
    assert_eq!(
        snapshot.read_range(b"big", stat.size, 10).unwrap(),
        Vec::<u8>::new()
    );
    assert!(snapshot.read_range(b"big", stat.size + 1, 1).is_err());
    assert!(snapshot.read_range(b"nope", 0, 1).is_err());
}

#[test]
fn shared_content_is_stored_once() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let store = new_store(&rt);

    let mut rng = Rng::new(21);
    let base = rng.bytes(16 << 20);

    let stat_a = store.put_file(b"a.img", Cursor::new(base.clone())).unwrap();
    let pages_a = live_pages(store.volume());
    let chunks_a = chunk_entries(store.volume());

    // Same image with a small insertion at 5 MiB and a rewritten tail.
    let mut edited = Vec::with_capacity(base.len() + 4096);
    edited.extend_from_slice(&base[..5 << 20]);
    edited.extend_from_slice(&rng.bytes(4096));
    edited.extend_from_slice(&base[5 << 20..]);
    let tail_from = edited.len() - (512 << 10);
    edited[tail_from..].copy_from_slice(&rng.bytes(512 << 10));

    let stat_b = store
        .put_file(b"b.img", Cursor::new(edited.clone()))
        .unwrap();
    let pages_b = live_pages(store.volume());
    let chunks_b = chunk_entries(store.volume());

    assert_eq!(store.get_file(b"b.img").unwrap().unwrap(), edited);
    assert_eq!(store.get_file(b"a.img").unwrap().unwrap(), base);

    let new_chunks = chunks_b - chunks_a;
    let growth = pages_b - pages_a;
    println!(
        "dedup: a has {} chunks / {pages_a} pages, b has {} chunks; {new_chunks} new chunks and {growth} new pages",
        stat_a.chunk_count, stat_b.chunk_count
    );

    // A 90 % shared 16 MiB image must not cost another full copy.
    assert!(
        new_chunks < stat_a.chunk_count as usize / 5,
        "{new_chunks} new chunks for a mostly shared file"
    );
    assert!(
        growth < pages_a / 3,
        "{growth} new pages for a mostly shared file"
    );
}

#[test]
fn large_manifest_spills_to_recipe_pages() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let store = new_store(&rt);

    // ~96 chunks at the 32 KiB target, past the 48 inline refs.
    let data = Rng::new(31).bytes(3 << 20);
    let stat = store
        .put_file(b"recipe", Cursor::new(data.clone()))
        .unwrap();
    assert!(
        stat.chunk_count as usize > cdc_volume::store::INLINE_REFS,
        "test needs an overflowing manifest, got {} chunks",
        stat.chunk_count
    );

    assert_eq!(store.get_file(b"recipe").unwrap().unwrap(), data);
    let snapshot = store.snapshot().unwrap();
    snapshot.reset_counters();
    let offset = (3 << 20) - 4096;
    let slice = snapshot.read_range(b"recipe", offset, 4096).unwrap();
    assert_eq!(slice, data[offset as usize..]);
    println!(
        "recipe overflow: {} chunks, 4 KiB tail read in {} frame reads",
        stat.chunk_count,
        snapshot.reads()
    );
    assert!(
        snapshot.reads() <= 12,
        "tail read of an overflowing manifest touched {} frames",
        snapshot.reads()
    );
}

#[test]
fn listings_are_ordered_and_prefix_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let store = new_store(&rt);

    for path in [&b"dir/a"[..], b"dir/b", b"dir/c", b"other/x", b"dir2/y"] {
        store.put_file(path, Cursor::new(path.to_vec())).unwrap();
    }

    let names: Vec<Vec<u8>> = store
        .ls(b"dir/")
        .unwrap()
        .into_iter()
        .map(|stat| stat.path)
        .collect();
    assert_eq!(
        names,
        vec![b"dir/a".to_vec(), b"dir/b".to_vec(), b"dir/c".to_vec()]
    );

    assert_eq!(store.ls(b"").unwrap().len(), 5);
    assert_eq!(store.ls(b"dir").unwrap().len(), 4); // dir/ and dir2/
    assert!(store.ls(b"nothing").unwrap().is_empty());

    assert!(store.delete_file(b"dir/b").unwrap());
    assert!(!store.delete_file(b"dir/b").unwrap());
    assert!(store.get_file(b"dir/b").unwrap().is_none());
    assert_eq!(store.ls(b"dir/").unwrap().len(), 2);

    // Deleted chunks stay in the index until GC, but listing is unaffected.
    assert!(chunk_entries(store.volume()) >= 5);
}

#[test]
fn dedup_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let vid: VolumeId;
    let data = Rng::new(41).bytes(1 << 20);
    {
        let store = new_store(&rt);
        vid = store.volume().id().clone();
        store.put_file(b"f", Cursor::new(data.clone())).unwrap();
        store.put_file(b"g", Cursor::new(data.clone())).unwrap();
    }

    let reopened = Store::open(rt, vid, &KEY).unwrap();
    assert_eq!(reopened.get_file(b"f").unwrap().unwrap(), data);
    assert_eq!(reopened.get_file(b"g").unwrap().unwrap(), data);

    // The identical second file reused the same chunk entries.
    let reader = reopened.volume().reader().unwrap();
    let sb = reader.read_superblock().unwrap();
    let tree = BTree::<ChunkCodec>::new(sb.chunk_root_idx());
    let mut count: Vec<ChunkKey> = tree
        .scan_all(&reader)
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    count.sort();
    count.dedup();
    assert_eq!(count.len(), chunk_entries(reopened.volume()));
}
