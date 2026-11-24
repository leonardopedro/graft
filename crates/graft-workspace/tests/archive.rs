//! The encrypted archive lane: chunked ingest, dedup, and range reads through
//! the workspace `cdc` storage class.

use std::{fs, io::Cursor, path::Path};

use graft::remote::RemoteConfig;
use graft_workspace::{
    ArchiveConfig, BlobErr, BlobStorage, Entry, Kind, Lane, MediaConfig, Workspace, WorkspaceConfig,
};

fn key_file(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("archive.key");
    let key = (0..32)
        .map(|i| format!("{:02x}", i * 7 + 3))
        .collect::<String>();
    fs::write(&path, key).unwrap();
    path
}

fn config(dir: &Path) -> WorkspaceConfig {
    WorkspaceConfig {
        remote: RemoteConfig::Fs {
            root: dir.join("remote").display().to_string(),
        },
        data_dir: dir.join("data"),
        autosync: None,
        catalog_tag: "ws-catalog".to_string(),
        media: Some(MediaConfig::Fs {
            root: dir.join("media").display().to_string(),
        }),
        archive: Some(ArchiveConfig { key_file: key_file(dir) }),
        backup: None,
    }
}

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(47).wrapping_add(11))
        .collect()
}

#[test]
fn cdc_blobs_round_trip_ranges_and_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::open(&config(dir.path())).unwrap();
    workspace.init().unwrap();
    let bytes = payload(700_000);

    let first = workspace
        .blobs()
        .unwrap()
        .ingest_cdc(
            Cursor::new(&bytes),
            Some(bytes.len() as u64),
            None,
            Some("application/octet-stream"),
        )
        .unwrap();
    assert!(!first.deduplicated);
    assert_eq!(first.blob.storage, BlobStorage::Cdc);
    assert_eq!(first.blob.size, bytes.len() as i64);
    assert!(first.blob.locator.starts_with("media/cdc/"));

    let blobs = workspace.blobs().unwrap();
    assert_eq!(blobs.read_range(&first.blob.hash, 0, None).unwrap(), bytes);
    assert_eq!(
        blobs.read_range(&first.blob.hash, 1_000, Some(50)).unwrap(),
        &bytes[1_000..1_050]
    );
    assert_eq!(
        blobs
            .read_range(&first.blob.hash, (bytes.len() as u64) - 7, Some(100))
            .unwrap(),
        &bytes[bytes.len() - 7..]
    );
    assert!(
        blobs
            .read_range(&first.blob.hash, bytes.len() as u64, None)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        blobs.read_range(&first.blob.hash, bytes.len() as u64 + 1, None),
        Err(BlobErr::RangeOutOfBounds { .. })
    ));

    let second = workspace
        .blobs()
        .unwrap()
        .ingest_cdc(Cursor::new(&bytes), None, Some(&first.blob.hash), None)
        .unwrap();
    assert!(second.deduplicated);
    assert_eq!(second.blob.hash, first.blob.hash);
    assert_eq!(second.blob.locator, first.blob.locator);
    assert_eq!(workspace.catalog().blob_list().unwrap().len(), 1);

    // A known expected hash is enforced; a mismatch leaves no row and no file.
    let wrong = "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let err = workspace
        .blobs()
        .unwrap()
        .ingest_cdc(Cursor::new(b"different"), Some(9), Some(wrong), None)
        .unwrap_err();
    assert!(matches!(err, BlobErr::HashMismatch { .. }), "{err}");
    assert_eq!(workspace.catalog().blob_list().unwrap().len(), 1);
}

#[test]
fn gc_removes_unreferenced_cdc_and_rewrites_the_volume() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();

    let kept = payload(200_000);
    let ingested = workspace
        .blobs()
        .unwrap()
        .ingest_cdc(Cursor::new(&kept), None, None, None)
        .unwrap();
    let mut entry = Entry::new("media/keep.bin".to_string(), Lane::Media, Kind::File);
    entry.blob_hash = Some(ingested.blob.hash.clone());
    entry.size = Some(ingested.blob.size);
    workspace.catalog().entry_put(&entry).unwrap();

    let orphan = workspace
        .blobs()
        .unwrap()
        .ingest_cdc(Cursor::new(payload(150_000)), None, None, None)
        .unwrap();
    let before = workspace.archive().unwrap().list("media/cdc/").unwrap();
    let rows = workspace.catalog().blob_list().unwrap();
    assert!(before.len() == 2, "paths={before:?} rows={rows:?}");
    let old_volume = workspace.archive().unwrap().volume_id();

    let (removed, stats) = workspace.gc_archive(&config).unwrap();
    assert_eq!(removed, vec![orphan.blob.hash]);
    // The catalog sweep runs before the rewrite, so the rewrite already sees
    // only the kept file.
    assert_eq!(stats.before.files, 1);
    assert_eq!(stats.after.files, 1);
    assert_ne!(workspace.archive().unwrap().volume_id(), old_volume);
    assert_eq!(
        workspace.archive().unwrap().list("media/cdc/").unwrap(),
        vec![ingested.blob.locator.clone()]
    );
    assert_eq!(workspace.catalog().blob_list().unwrap().len(), 1);
    assert_eq!(
        workspace
            .blobs()
            .unwrap()
            .read_range(&ingested.blob.hash, 1_234, Some(64))
            .unwrap(),
        &kept[1_234..1_298]
    );
}

#[test]
fn archive_works_without_a_raw_media_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.media = None;
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();
    let bytes = payload(100_000);

    let ingested = workspace
        .blobs()
        .unwrap()
        .ingest_cdc(Cursor::new(&bytes), None, None, None)
        .unwrap();
    assert_eq!(
        workspace
            .blobs()
            .unwrap()
            .read_range(&ingested.blob.hash, 100, Some(10))
            .unwrap(),
        &bytes[100..110]
    );
    let err = workspace
        .blobs()
        .unwrap()
        .ingest(Cursor::new(&bytes), None, None, None)
        .unwrap_err();
    assert!(matches!(err, BlobErr::NoMediaStore), "{err}");
}

#[test]
fn rejects_a_malformed_archive_key() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.key");
    fs::write(&bad, "not a key").unwrap();
    let mut config = config(dir.path());
    config.archive = Some(ArchiveConfig { key_file: bad });

    let err = match Workspace::open(&config) {
        Ok(_) => panic!("malformed key must be rejected"),
        Err(err) => err,
    };
    let message = format!("{err:#}");
    assert!(message.contains("archive key"), "{message}");
}
