//! Phase 4 acceptance: a second machine attaches to a volume another machine
//! pushed and reads a range with an empty local cache.
//!
//! The remote here is an `Fs` directory rather than S3/MinIO, so the byte cost is
//! not measured on the wire; what is measured is how many page frames the lazy
//! path actually fetches, which §10 turns into bytes using the frame sizes
//! measured in `remote_frames.rs`.

use std::path::{Path, PathBuf};

use cdc_volume::{Store, crypto::MASTER_KEY_LEN};
use graft::{
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft},
};

const KEY: [u8; MASTER_KEY_LEN] = [7u8; MASTER_KEY_LEN];

fn runtime(dir: &Path, remote: RemoteConfig) -> Runtime {
    setup_graft(GraftConfig {
        remote,
        data_dir: dir.to_path_buf(),
        autosync: None,
    })
    .expect("graft setup")
}

fn fs_remote(root: &Path) -> RemoteConfig {
    RemoteConfig::Fs {
        root: root.to_string_lossy().into_owned(),
    }
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.truncate(len);
        out
    }
}

#[test]
fn a_cold_edge_reads_only_the_frames_it_needs() {
    let root = tempfile::tempdir().unwrap();
    let remote_dir: PathBuf = root.path().join("remote");
    let origin_dir = root.path().join("origin");
    let edge_dir = root.path().join("edge");

    // Origin: store 4 MiB, push every commit, then go away.
    let origin = runtime(&origin_dir, fs_remote(&remote_dir));
    let vid = origin.volume_open(None, None, None).unwrap().vid;
    let store = Store::create(origin.clone(), vid.clone(), &KEY).unwrap();
    let bytes = Rng::new(11).bytes(4 << 20);
    store.put_file(b"blob", bytes.as_slice()).unwrap();
    let log = origin.volume_get(&vid).unwrap().remote;
    origin.volume_push(vid.clone()).unwrap();
    let file = store.stat(b"blob").unwrap().unwrap();
    drop(store);
    drop(origin);

    // Edge: fresh data directory, so nothing but the remote is available.
    let edge = runtime(&edge_dir, fs_remote(&remote_dir));
    edge.volume_open(Some(vid.clone()), None, Some(log))
        .unwrap();
    edge.volume_pull(vid.clone()).unwrap();
    let store = Store::open(edge.clone(), vid.clone(), &KEY).unwrap();

    // 10 KiB from the middle: the superblock is cached by `open`, so this is the
    // index path plus the chunk frames the range touches.
    let snap = store.snapshot().unwrap();
    snap.reset_counters();
    let offset = 2 << 20;
    let out = snap.read_range(b"blob", offset, 10 << 10).unwrap();
    assert_eq!(out, &bytes[offset as usize..offset as usize + (10 << 10)]);
    let cold_reads = snap.reads();

    eprintln!(
        "edge: 10 KiB of {} bytes in {} chunks cost {cold_reads} frame reads",
        file.size, file.chunk_count
    );
    assert!(
        cold_reads < 40,
        "cold range read fetched {cold_reads} frames"
    );

    // `reads()` counts page reads, not network fetches: on a cold edge every page
    // is a remote segment, on a warm one the same pages are local. What the
    // count proves is that a range touches a bounded number of chunks, not the
    // 113 chunks the whole file has.
    assert!(
        cold_reads < u64::from(file.chunk_count),
        "range read touched {cold_reads} frames of {} chunks",
        file.chunk_count
    );

    // Pulling again with nothing new is a no-op, not an error.
    edge.volume_pull(vid).unwrap();
}

#[test]
fn a_wrong_key_cannot_read_a_pulled_volume() {
    let root = tempfile::tempdir().unwrap();
    let remote_dir = root.path().join("remote");
    let origin = runtime(&root.path().join("origin"), fs_remote(&remote_dir));
    let vid = origin.volume_open(None, None, None).unwrap().vid;
    let store = Store::create(origin.clone(), vid.clone(), &KEY).unwrap();
    store.put_file(b"blob", [3u8; 4096].as_slice()).unwrap();
    let log = origin.volume_get(&vid).unwrap().remote;
    origin.volume_push(vid.clone()).unwrap();
    drop(store);
    drop(origin);

    let edge = runtime(&root.path().join("edge"), fs_remote(&remote_dir));
    edge.volume_open(Some(vid.clone()), None, Some(log))
        .unwrap();
    edge.volume_pull(vid.clone()).unwrap();

    // The superblock decrypts with the wrong key only if the volume is empty, so
    // opening a real volume must fail with an authentication error.
    let err = Store::open(edge, vid, &[9u8; MASTER_KEY_LEN]).unwrap_err();
    assert!(
        matches!(err, cdc_volume::CdcErr::Auth(_)),
        "wrong key gave {err:?}"
    );
}
