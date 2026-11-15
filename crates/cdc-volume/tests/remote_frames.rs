//! Measurement: what a cold read actually downloads from a Graft remote.
//!
//! Graft stores a remote commit as one compressed segment, split into zstd
//! frames of at most 64 pages (256 KiB uncompressed). `read_page` fetches the
//! whole frame containing the page, so the frame is the real cold-read unit.
//! This test writes encrypted (incompressible) pages, pushes, and reports the
//! frame sizes recorded in the remote commit.
//!
//! Run with `cargo test -p cdc-volume --test remote_frames -- --nocapture`.

use std::path::Path;

use cdc_volume::{CdcVolume, PAYLOAD_SIZE, PageWrite};
use graft::{
    core::{VolumeId, lsn::LSN},
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft},
};

const FRAME_PAGE_LIMIT: u32 = 64;

fn runtime(dir: &Path, remote_root: &Path) -> Runtime {
    std::fs::create_dir_all(remote_root).unwrap();
    setup_graft(GraftConfig {
        remote: RemoteConfig::Fs {
            root: remote_root.to_str().unwrap().to_string(),
        },
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

/// Deterministic but incompressible filler, standing in for AEAD ciphertext.
fn fill_incompressible(payload: &mut [u8; PAYLOAD_SIZE], seed: u64) {
    for (i, chunk) in payload.chunks_mut(32).enumerate() {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&seed.to_le_bytes());
        hasher.update(&(i as u64).to_le_bytes());
        let hash = hasher.finalize();
        chunk.copy_from_slice(&hash.as_bytes()[..chunk.len()]);
    }
}

struct FrameReport {
    pages: u32,
    bytes: u64,
}

fn frames_of(rt: &Runtime, log: &graft::core::LogId, lsn: LSN) -> Vec<FrameReport> {
    let commit = rt
        .get_commit(log, lsn)
        .expect("read remote commit")
        .unwrap_or_else(|| panic!("no remote commit at {lsn:?}"));
    let idx = commit.segment_idx().expect("remote commit has a segment");
    idx.iter_frames(|_| true)
        .map(|frame| FrameReport {
            pages: frame.pageset.cardinality().to_u32(),
            bytes: frame.size(),
        })
        .collect()
}

#[test]
fn measure_remote_frame_granularity() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(&dir.path().join("local"), &dir.path().join("remote"));
    let (vid, vol) = open_fresh(&rt);

    // One bulk commit: a batch-sized write of encrypted chunk frames.
    vol.update(|txn| -> cdc_volume::Result<()> {
        let start = txn.alloc(1000)?;
        let (io, _alloc) = txn.parts();
        for i in 0..1000u64 {
            let mut payload = [0u8; PAYLOAD_SIZE];
            fill_incompressible(&mut payload, i);
            io.write_payload(start.saturating_add(i as u32), &payload)?;
        }
        Ok(())
    })
    .unwrap();

    rt.volume_push(vid.clone()).unwrap();
    let remote_log = rt.volume_get(&vid).unwrap().remote;

    let bulk = frames_of(&rt, &remote_log, LSN::FIRST);
    let bulk_pages: u32 = bulk.iter().map(|f| f.pages).sum();
    let bulk_bytes: u64 = bulk.iter().map(|f| f.bytes).sum();
    let largest = bulk.iter().max_by_key(|f| f.bytes).unwrap();

    println!(
        "\nbulk commit: {bulk_pages} pages, {} frames, {bulk_bytes} bytes on the wire",
        bulk.len()
    );
    for (i, f) in bulk.iter().enumerate() {
        println!(
            "  frame {i}: {:>7} bytes over {:>2} pages",
            f.bytes, f.pages
        );
    }

    assert!(
        bulk.iter().all(|f| f.pages <= FRAME_PAGE_LIMIT),
        "a frame exceeded the 64-page limit"
    );
    assert!(
        largest.bytes <= u64::from(FRAME_PAGE_LIMIT) * 4096 + 4096,
        "frame larger than 256 KiB plus codec overhead: {}",
        largest.bytes
    );

    // One small commit: what an index-only transaction costs a cold reader.
    vol.update(|txn| -> cdc_volume::Result<()> {
        let start = txn.alloc(3)?;
        let (io, _alloc) = txn.parts();
        for i in 0..3u64 {
            let mut payload = [0u8; PAYLOAD_SIZE];
            fill_incompressible(&mut payload, 10_000 + i);
            io.write_payload(start.saturating_add(i as u32), &payload)?;
        }
        Ok(())
    })
    .unwrap();

    rt.volume_push(vid).unwrap();
    let small = frames_of(&rt, &remote_log, LSN::new(2));
    let small_bytes: u64 = small.iter().map(|f| f.bytes).sum();
    println!(
        "\nsmall commit: {} pages, {} frames, {small_bytes} bytes on the wire",
        small.iter().map(|f| f.pages).sum::<u32>(),
        small.len()
    );

    println!(
        "\ncold read of one page: {} bytes (the frame holding it)\n\
         cold 3-level index path: ~{} bytes (three separate frames)\n",
        largest.bytes,
        largest.bytes * 3
    );

    assert_eq!(small.len(), 1, "a 4-page commit fits in a single frame");
}
