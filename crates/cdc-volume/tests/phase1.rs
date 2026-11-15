//! Phase 1 acceptance tests: frame, crypto, superblock, allocator.

use std::path::Path;

use cdc_volume::{
    CdcErr, CdcVolume, FrameIo, PAYLOAD_SIZE, PageRead, PageWrite, Superblock,
    layout::{FRAME_SIZE, SUPERBLOCK_PAGE},
};
use graft::{
    core::{PageCount, PageIdx, VolumeId, page::Page},
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft},
    volume_reader::VolumeRead,
    volume_writer::{VolumeWrite, VolumeWriter},
};

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

#[test]
fn write_pages_commit_and_read_back_through_second_reader() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (_vid, vol) = open_fresh(&rt);

    vol.update(|txn| -> cdc_volume::Result<()> {
        let start = txn.alloc(3)?;
        assert_eq!(start.to_u32(), 2, "allocation starts after the superblock");

        let (io, _alloc) = txn.parts();
        for i in 0..3u32 {
            let mut payload = [0u8; PAYLOAD_SIZE];
            payload[..4].copy_from_slice(&i.to_le_bytes());
            payload[4] = 0xAA;
            io.write_payload(start.saturating_add(i), &payload)?;
        }
        Ok(())
    })
    .unwrap();

    let sb = vol.superblock().unwrap();
    assert_eq!(sb.generation, 1);
    assert_eq!(sb.next_page, 5);
    assert_eq!(sb.free_head, 0);

    // A reader created after the commit, i.e. from a different snapshot.
    let reader = vol.reader().unwrap();
    assert_eq!(reader.page_count(), PageCount::new(4));

    for i in 0..3u32 {
        let idx = PageIdx::try_new(2 + i).unwrap();
        let payload = reader.read_payload(idx).unwrap();
        assert_eq!(u32::from_le_bytes(payload[..4].try_into().unwrap()), i);
        assert_eq!(payload[4], 0xAA);
    }
}

#[test]
fn superblock_lives_at_page_one() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (_vid, vol) = open_fresh(&rt);

    assert_eq!(SUPERBLOCK_PAGE, PageIdx::FIRST);
    let sb = vol.superblock().unwrap();
    assert_eq!(sb.chunk_root_idx(), None);
    assert_eq!(sb.path_root_idx(), None);

    // Page 1 is a real page: the volume is not empty after init.
    let reader = rt.volume_reader(vol.id().clone()).unwrap();
    assert_eq!(reader.page_count(), PageCount::new(1));
}

#[test]
fn flipped_ciphertext_byte_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (vid, vol) = open_fresh(&rt);

    vol.update(|txn| -> cdc_volume::Result<()> {
        let start = txn.alloc(1)?;
        let (io, _alloc) = txn.parts();
        io.write_payload(start, &[7u8; PAYLOAD_SIZE])?;
        Ok(())
    })
    .unwrap();

    let target = PageIdx::try_new(2).unwrap();
    let reader = rt.volume_reader(vid.clone()).unwrap();
    let frame = reader.read_page(target).unwrap();

    let mut bytes = frame.as_ref().to_vec();
    bytes[100] ^= 0x01;
    let tampered = Page::try_from(bytes.as_slice()).unwrap();

    let mut writer = rt.volume_writer(vid).unwrap();
    writer.write_page(target, tampered).unwrap();
    writer.commit().unwrap();

    match vol.reader().unwrap().read_payload(target) {
        Err(CdcErr::Auth(idx)) => assert_eq!(idx, 2),
        other => panic!("expected authentication failure, got {other:?}"),
    }
}

#[test]
fn relocated_frame_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (vid, vol) = open_fresh(&rt);

    vol.update(|txn| -> cdc_volume::Result<()> {
        let start = txn.alloc(2)?;
        let (io, _alloc) = txn.parts();
        let mut a = [0u8; PAYLOAD_SIZE];
        a[0] = 1;
        let mut b = [0u8; PAYLOAD_SIZE];
        b[0] = 2;
        io.write_payload(start, &a)?;
        io.write_payload(start.saturating_add(1), &b)?;
        Ok(())
    })
    .unwrap();

    // Move the frame from page 2 to a fresh page 9 without re-encrypting it.
    let src = PageIdx::try_new(2).unwrap();
    let dst = PageIdx::try_new(9).unwrap();
    let frame = rt
        .volume_reader(vid.clone())
        .unwrap()
        .read_page(src)
        .unwrap();
    let mut writer = rt.volume_writer(vid).unwrap();
    writer.write_page(dst, frame).unwrap();
    writer.commit().unwrap();

    match vol.reader().unwrap().read_payload(dst) {
        Err(CdcErr::Auth(idx)) => assert_eq!(idx, 9),
        other => panic!("expected authentication failure, got {other:?}"),
    }
}

#[test]
fn a_fresh_runtime_opens_with_only_volume_id_and_master_key() {
    let dir = tempfile::tempdir().unwrap();
    let vid;

    {
        let rt = runtime(dir.path());
        let (id, vol) = open_fresh(&rt);
        vid = id;

        vol.update(|txn| -> cdc_volume::Result<()> {
            let start = txn.alloc(1)?;
            let (io, _alloc) = txn.parts();
            let mut payload = [0u8; PAYLOAD_SIZE];
            payload[..4].copy_from_slice(b"cold");
            io.write_payload(start, &payload)?;
            Ok(())
        })
        .unwrap();
    }

    // New runtime, same data directory, no state carried over except the id.
    let rt = runtime(dir.path());
    let vol = CdcVolume::open(rt, vid.clone(), &[42u8; 32]).expect("open with vid + key");
    let sb = vol.superblock().unwrap();
    assert_eq!(sb.generation, 1);
    assert_eq!(sb.next_page, 3);

    let payload = vol
        .reader()
        .unwrap()
        .read_payload(PageIdx::try_new(2).unwrap())
        .unwrap();
    assert_eq!(&payload[..4], b"cold");

    // Release the data directory, then prove a different key cannot open it.
    drop(vol);
    let rt2 = runtime(dir.path());
    assert!(CdcVolume::open(rt2, vid, &[43u8; 32]).is_err());
}

#[test]
fn freed_pages_are_reused_by_the_next_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (_vid, vol) = open_fresh(&rt);

    let run_start = vol
        .update(|txn| -> cdc_volume::Result<u32> {
            let start = txn.alloc(4)?;
            let (io, _alloc) = txn.parts();
            io.write_payload(start, &[0u8; PAYLOAD_SIZE])?;
            // Return the middle two pages to the free list.
            txn.free(start.saturating_add(1), 2)?;
            Ok(start.to_u32())
        })
        .unwrap();
    assert_eq!(run_start, 2);

    let sb = vol.superblock().unwrap();
    assert_eq!(sb.free_head, 3);
    assert_eq!(sb.next_page, 6);

    let reused = vol
        .update(|txn| -> cdc_volume::Result<u32> {
            let run = txn.alloc(2)?;
            let (io, _alloc) = txn.parts();
            io.write_payload(run, &[0u8; PAYLOAD_SIZE])?;
            Ok(run.to_u32())
        })
        .unwrap();
    assert_eq!(reused, 3, "the contiguous free run is reused first");
    assert_eq!(vol.superblock().unwrap().next_page, 6);
}

#[test]
fn allocator_never_hands_out_the_superblock() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (_vid, vol) = open_fresh(&rt);

    let first = vol
        .update(|txn| -> cdc_volume::Result<u32> { Ok(txn.alloc(1)?.to_u32()) })
        .unwrap();
    assert!(first >= 2);
    assert_eq!(Superblock::default().next_page, 2);
}

#[test]
fn frame_io_writes_and_reads_a_full_frame() {
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime(dir.path());
    let (vid, vol) = open_fresh(&rt);

    let mut io = FrameIo::new(
        rt.volume_writer(vid.clone()).unwrap(),
        vol.keys().page_cipher(),
    );

    let mut payload = [0u8; PAYLOAD_SIZE];
    payload[PAYLOAD_SIZE - 1] = 0x5A;
    let idx = PageIdx::try_new(3).unwrap();
    io.write_payload(idx, &payload).unwrap();
    assert_eq!(io.writes(), 1);

    {
        let writer: VolumeWriter = io.into_inner();
        writer.commit().unwrap();
    }

    let reader = FrameIo::new(rt.volume_reader(vid).unwrap(), vol.keys().page_cipher());
    assert_eq!(reader.read_payload(idx).unwrap(), payload);
    assert_eq!(reader.reads(), 1);
    assert_eq!(FRAME_SIZE, 4096);
}
