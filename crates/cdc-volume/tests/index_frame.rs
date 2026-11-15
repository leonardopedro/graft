//! The chunk index must not straddle a remote frame.
//!
//! A cold range read needs one data frame plus the frame holding the chunk
//! index. Graft packs a segment's pages into 64-page frames in page order, and
//! frames start at the segment's first page (always the superblock, page 1), so
//! the index only costs one frame when it begins at page `1 + 64k`. `put_file`
//! pads for that; this test is what fails if the padding is dropped or drifts.

use std::path::Path;

use cdc_volume::{Store, crypto::MASTER_KEY_LEN};
use graft::{
    core::VolumeId,
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft},
};

const KEY: [u8; MASTER_KEY_LEN] = [7u8; MASTER_KEY_LEN];

fn runtime(dir: &Path, remote: &Path) -> Runtime {
    setup_graft(GraftConfig {
        remote: RemoteConfig::Fs {
            root: remote.to_string_lossy().into_owned(),
        },
        data_dir: dir.to_path_buf(),
        autosync: None,
    })
    .expect("graft setup")
}

fn incompressible(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x = seed | 1;
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

#[test]
fn the_chunk_index_stays_inside_one_frame() {
    // Different seeds chunk differently, which moves where the data ends.
    for seed in [1u64, 2, 3] {
        let root = tempfile::tempdir().unwrap();
        let origin = runtime(&root.path().join("origin"), &root.path().join("remote"));
        let vid: VolumeId = origin.volume_open(None, None, None).unwrap().vid;
        let store = Store::create(origin.clone(), vid.clone(), &KEY).unwrap();
        let bytes = incompressible(4 << 20, seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        store.put_file(b"blob", bytes.as_slice()).unwrap();
        let log = origin.volume_get(&vid).unwrap().remote;
        origin.volume_push(vid.clone()).unwrap();

        let sb = store.volume().superblock().unwrap();
        let chunk_root = sb.chunk_root_idx().map(|p| p.to_u32()).unwrap();
        let path_root = sb.path_root_idx().map(|p| p.to_u32()).unwrap();

        let commit = origin
            .get_commit(&log, graft::core::lsn::LSN::FIRST)
            .unwrap()
            .unwrap();
        let idx = commit.segment_idx().unwrap();
        let last = idx.iter_frames(|_| true).last().unwrap();
        let first = last.pageset.first().map(|p| p.to_u32()).unwrap();
        let last_page = last.pageset.last().map(|p| p.to_u32()).unwrap();

        assert_eq!(
            first % 64,
            1,
            "seed {seed}: index frame starts at page {first}, not a frame boundary"
        );
        for root_page in [chunk_root, path_root] {
            assert!(
                (first..=last_page).contains(&root_page),
                "seed {seed}: root page {root_page} is outside the last frame \
                 ({first}..{last_page}), so the read pays a second index frame"
            );
        }
        assert!(
            last.size() < 64 * 4096,
            "seed {seed}: the last frame is a full one ({} B), so the index is \
             sharing it with data",
            last.size()
        );
    }
}
