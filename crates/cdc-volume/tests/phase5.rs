//! Phase 5 acceptance: reachability GC, interrupted uploads, corruption.

use std::{
    io::{self, Cursor, Read},
    path::Path,
};

use cdc_volume::{
    BTree, CdcErr, ChunkCodec, Store, crypto::MASTER_KEY_LEN, gc, layout::PAYLOAD_SIZE,
};
use graft::{
    core::{PageIdx, VolumeId, page::Page},
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft},
    volume_reader::VolumeRead,
    volume_writer::VolumeWrite,
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

fn new_volume(rt: &Runtime) -> (VolumeId, Store) {
    let vid = rt.volume_open(None, None, None).expect("open volume").vid;
    let store = Store::create(rt.clone(), vid.clone(), &KEY).expect("create store");
    (vid, store)
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

#[test]
fn gc_reclaims_deleted_and_overwritten_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let store = new_store(&rt);
    let mut rng = Rng::new(11);

    let keep = rng.bytes(2 << 20);
    let doomed = rng.bytes(2 << 20);
    let first = rng.bytes(1 << 20);
    let second = rng.bytes(1 << 20);
    store.put_file(b"keep", Cursor::new(&keep)).unwrap();
    store.put_file(b"doomed", Cursor::new(&doomed)).unwrap();
    // Overwriting a file with unrelated content orphans its old chunks.
    store.put_file(b"rewritten", Cursor::new(&first)).unwrap();
    store.put_file(b"rewritten", Cursor::new(&second)).unwrap();
    store.delete_file(b"doomed").unwrap();

    let before = gc::stats(&store).unwrap();
    assert_eq!(before.files, 2, "{before:?}");
    assert!(before.dead_chunk_frames > 0, "expected garbage: {before:?}");
    assert_eq!(
        before.live_bytes,
        (keep.len() + second.len()) as u64,
        "live bytes must count live chunks only"
    );

    let (_, target) = new_volume(&rt);
    let moved = gc::rewrite(&store, &target).unwrap();
    assert_eq!(moved.before, before);
    assert_eq!(moved.copied_bytes, before.live_bytes);

    // The source is untouched and still serves its files.
    assert_eq!(store.get_file(b"keep").unwrap().unwrap(), keep);

    // The rewritten volume serves the same bytes for every survivor.
    assert_eq!(target.get_file(b"keep").unwrap().unwrap(), keep);
    assert_eq!(target.get_file(b"rewritten").unwrap().unwrap(), second);
    assert!(target.get_file(b"doomed").unwrap().is_none());
    assert_eq!(target.ls(b"").unwrap().len(), 2);

    let after = moved.after;
    eprintln!(
        "gc: {} pages ({} live) -> {} pages ({} live), {} dead frames dropped, copied {} chunks / {} bytes",
        before.pages,
        before.live_pages,
        after.pages,
        after.live_pages,
        before
            .dead_chunk_frames
            .saturating_sub(after.dead_chunk_frames),
        moved.copied_chunks,
        moved.copied_bytes
    );
    assert_eq!(
        after.dead_chunk_frames, 0,
        "chunk garbage survived GC: {after:?}"
    );
    assert!(after.pages < before.pages, "{after:?} vs {before:?}");
    assert_eq!(after.files, 2);
    assert_eq!(after.live_bytes, before.live_bytes);
    assert_eq!(after.chunks, before.chunks);

    // GC is what makes the superblock's accounting exact again.
    let sb = target.volume().superblock().unwrap();
    assert_eq!(sb.chunk_count, after.chunks);
    assert_eq!(sb.file_count, after.files);
    assert_eq!(sb.live_bytes, after.live_bytes);
    assert_eq!(u64::from(sb.next_page), after.pages + 1);
}

/// A reader that hands out `limit` bytes of incompressible data and then
/// fails, like a dropped connection part-way through an upload.
struct DroppedConnection {
    left: usize,
    rng: Rng,
}

impl Read for DroppedConnection {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::other("connection dropped"));
        }
        let take = out.len().min(self.left);
        for byte in out.iter_mut().take(take) {
            *byte = self.rng.next_u64() as u8;
        }
        self.left -= take;
        Ok(take)
    }
}

#[test]
fn interrupted_upload_leaves_only_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let store = new_store(&rt);

    // Enough to force at least one committed chunk batch before the failure.
    let mut source = DroppedConnection { left: 34 << 20, rng: Rng::new(41) };
    let err = store.put_file(b"partial", &mut source).unwrap_err();
    assert!(
        matches!(err, CdcErr::Io(_)),
        "expected the read failure to surface: {err:?}"
    );
    assert!(store.get_file(b"partial").unwrap().is_none());

    let before = gc::stats(&store).unwrap();
    assert_eq!(before.files, 0);
    assert!(
        before.dead_chunk_frames > 0,
        "an interrupted upload must leave unreachable frames: {before:?}"
    );

    let (_, target) = new_volume(&rt);
    let moved = gc::rewrite(&store, &target).unwrap();
    eprintln!(
        "interrupted upload: {} pages / {} dead frames -> {} pages",
        before.pages, before.dead_chunk_frames, moved.after.pages
    );
    assert_eq!(moved.after.files, 0);
    assert_eq!(moved.after.chunks, 0);
    assert_eq!(moved.after.dead_chunk_frames, 0);
    // A volume with nothing live is just its superblock.
    assert_eq!(moved.after.pages, 1);
    assert_eq!(moved.pages_reclaimed(), before.pages - 1);
    assert!(target.ls(b"").unwrap().is_empty());
}

#[test]
fn dedup_survives_the_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let store = new_store(&rt);
    let mut rng = Rng::new(23);

    let content = rng.bytes(4 << 20);
    store.put_file(b"a", Cursor::new(&content)).unwrap();

    let (_, target) = new_volume(&rt);
    gc::rewrite(&store, &target).unwrap();
    let after_gc = gc::stats(&target).unwrap();
    assert_eq!(target.get_file(b"a").unwrap().unwrap(), content);

    // Identical content must dedup against the copied chunks, which only works
    // if the rewrite re-keyed them consistently with the manifests.
    target.put_file(b"b", Cursor::new(&content)).unwrap();
    let after_second_put = gc::stats(&target).unwrap();
    assert_eq!(after_second_put.chunks, after_gc.chunks);
    assert_eq!(after_second_put.live_bytes, after_gc.live_bytes);
    assert_eq!(target.get_file(b"b").unwrap().unwrap(), content);
}

#[test]
fn a_corrupted_frame_fails_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (vid, store) = new_volume(&rt);
    let mut rng = Rng::new(31);

    let content = rng.bytes(64 << 10);
    store.put_file(b"f", Cursor::new(&content)).unwrap();

    // Flip a byte of the first chunk frame on disk, past the nonce.
    let sb = store.volume().superblock().unwrap();
    let reader = store.volume().reader().unwrap();
    let entries = BTree::<ChunkCodec>::new(sb.chunk_root_idx())
        .scan_all(&reader)
        .unwrap();
    let start = entries[0].1.start;
    let idx = PageIdx::try_new(start).unwrap();

    let frame = reader.inner().read_page(idx).unwrap();
    let mut bytes = frame.as_ref().to_vec();
    bytes[100] ^= 0x01;
    let tampered = Page::try_from(bytes.as_slice()).unwrap();

    let mut writer = rt.volume_writer(vid).unwrap();
    writer.write_page(idx, tampered).unwrap();
    writer.commit().unwrap();

    match store.get_file(b"f") {
        Err(CdcErr::Auth(page)) => assert_eq!(page, start),
        other => panic!("expected an authentication error, got {other:?}"),
    }

    // GC refuses to copy data that does not match its key.
    let (_, target) = new_volume(&rt);
    assert!(gc::rewrite(&store, &target).is_err());
    let _ = PAYLOAD_SIZE;
}
