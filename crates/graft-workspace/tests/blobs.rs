//! Ingest and range-read behaviour of the media lane, exercised through the
//! real Graft VFS and a filesystem media store.

use std::{fs, io::Cursor, path::Path, time::Duration};

use graft::remote::RemoteConfig;
use graft_workspace::{
    BlobErr, Kind, Lane, MediaConfig, Workspace, WorkspaceConfig, catalog::Entry,
};

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
        archive: None,
        backup: None,
    }
}

fn initialized(dir: &Path) -> Workspace {
    let mut workspace = Workspace::open(&config(dir)).unwrap();
    workspace.init().unwrap();
    workspace
}

/// `unwrap_err` needs `T: Debug`; `Blobs` borrows a connection and is not.
fn expect_err<T>(result: Result<T, BlobErr>) -> BlobErr {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(err) => err,
    }
}

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
        .collect()
}

#[test]
fn direct_upload_commit_verifies_and_dedups() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let bytes = payload(120_000);
    let hash = format!("blake3:{}", blake3::hash(&bytes).to_hex());
    let workspace_id = workspace.catalog().status().unwrap().workspace_id.unwrap();
    let ticket = format!("staging/{workspace_id}/ticket-1");
    let staging = dir.path().join("media").join(&ticket);
    fs::create_dir_all(staging.parent().unwrap()).unwrap();
    fs::write(&staging, &bytes).unwrap();

    let ingested = workspace
        .blobs()
        .unwrap()
        .commit_presigned(&ticket, Some(bytes.len() as u64), &hash, Some("text/plain"))
        .unwrap();
    assert!(!ingested.deduplicated);
    assert_eq!(ingested.blob.hash, hash);
    assert_eq!(ingested.blob.storage, graft_workspace::BlobStorage::Raw);
    assert!(!staging.exists(), "commit removes the staging object");
    assert_eq!(
        workspace
            .blobs()
            .unwrap()
            .read_range(&hash, 50, Some(10))
            .unwrap(),
        &bytes[50..60]
    );

    // A second direct upload of the same bytes is dropped at commit.
    fs::write(&staging, &bytes).unwrap();
    let again = workspace
        .blobs()
        .unwrap()
        .commit_presigned(&ticket, Some(bytes.len() as u64), &hash, None)
        .unwrap();
    assert!(again.deduplicated);
    assert!(!staging.exists());

    // A ticket outside this workspace's staging prefix cannot be committed.
    let err = expect_err(workspace.blobs().unwrap().commit_presigned(
        "media/raw/other/file",
        None,
        &hash,
        None,
    ));
    assert!(matches!(err, BlobErr::BadTicket(_)), "{err}");

    // The filesystem backend cannot presign; callers fall back to streaming.
    let unstored = format!("blake3:{}", blake3::hash(b"not stored yet").to_hex());
    let err = expect_err(
        workspace
            .blobs()
            .unwrap()
            .presign_ingest(&unstored, Duration::from_secs(60)),
    );
    assert!(matches!(err, BlobErr::PresignUnsupported), "{err}");
}

#[test]
fn ingests_streams_and_reads_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let bytes = payload(200_000);
    let mime = "application/octet-stream";

    let ingested = workspace
        .blobs()
        .unwrap()
        .ingest(
            Cursor::new(&bytes),
            Some(bytes.len() as u64),
            None,
            Some(mime),
        )
        .unwrap();

    assert!(!ingested.deduplicated);
    assert_eq!(ingested.blob.size, bytes.len() as i64);
    assert_eq!(ingested.blob.mime.as_deref(), Some(mime));
    assert!(ingested.blob.hash.starts_with("blake3:"));
    assert!(ingested.blob.sha256.is_some());
    let locator = ingested.blob.locator.clone();
    assert_eq!(
        workspace
            .media()
            .unwrap()
            .stat(&locator)
            .unwrap()
            .unwrap()
            .size,
        bytes.len() as u64
    );

    // Whole object and sub-ranges come back byte for byte.
    let blobs = workspace.blobs().unwrap();
    assert_eq!(
        blobs.read_range(&ingested.blob.hash, 0, None).unwrap(),
        bytes
    );
    assert_eq!(
        blobs
            .read_range(&ingested.blob.hash, 1_000, Some(50))
            .unwrap(),
        &bytes[1_000..1_050]
    );
    // A range may extend past the end; it is clamped, not an error.
    assert_eq!(
        blobs
            .read_range(&ingested.blob.hash, (bytes.len() as u64) - 10, Some(1_000))
            .unwrap(),
        &bytes[bytes.len() - 10..]
    );
    assert!(
        blobs
            .read_range(&ingested.blob.hash, bytes.len() as u64, None)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        blobs.read_range(&ingested.blob.hash, bytes.len() as u64 + 1, None),
        Err(BlobErr::RangeOutOfBounds { .. })
    ));
}

#[test]
fn identical_bytes_are_stored_once() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let bytes = payload(4_096);

    let first = workspace
        .blobs()
        .unwrap()
        .ingest(Cursor::new(&bytes), None, None, None)
        .unwrap();
    assert!(!first.deduplicated);

    // Known hash: short-circuits before writing anything.
    let second = workspace
        .blobs()
        .unwrap()
        .ingest(Cursor::new(&bytes), None, Some(&first.blob.hash), None)
        .unwrap();
    assert!(second.deduplicated);
    assert_eq!(second.blob.hash, first.blob.hash);

    // Unknown hash: dedup happens after streaming, and the staging object is
    // cleaned up either way.
    let third = workspace
        .blobs()
        .unwrap()
        .ingest(Cursor::new(&bytes), None, None, None)
        .unwrap();
    assert!(third.deduplicated);

    assert_eq!(workspace.catalog().blob_list().unwrap().len(), 1);
    assert!(
        workspace
            .media()
            .unwrap()
            .list("staging/")
            .unwrap()
            .is_empty(),
        "deduplicated uploads must not leave staging objects"
    );
}

#[test]
fn rejects_mismatches_without_recording_a_blob() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let bytes = payload(1_024);
    let blobs = workspace.blobs().unwrap();

    let err = blobs
        .ingest(Cursor::new(&bytes), Some(1_025), None, None)
        .unwrap_err();
    assert!(matches!(
        err,
        BlobErr::SizeMismatch { expected: 1_025, actual: 1_024 }
    ));

    let err = blobs
        .ingest(Cursor::new(&bytes), None, Some("blake3:0000"), None)
        .unwrap_err();
    assert!(matches!(err, BlobErr::HashMismatch { .. }));

    let err = blobs
        .ingest(Cursor::new(&bytes), None, Some("sha256:aa"), None)
        .unwrap_err();
    assert!(matches!(err, BlobErr::InvalidHash(_)));

    assert!(workspace.catalog().blob_list().unwrap().is_empty());
    assert!(
        workspace
            .media()
            .unwrap()
            .list("staging/")
            .unwrap()
            .is_empty(),
        "failed uploads must clean up staging"
    );
}

#[test]
fn sweeps_only_abandoned_staging_objects() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let workspace_id = workspace.catalog().workspace_id().unwrap().to_string();

    // An upload that died before commit: an object in staging with no catalog
    // row, written behind the store's back.
    let staging_dir = dir.path().join("media").join("staging").join(&workspace_id);
    fs::create_dir_all(&staging_dir).unwrap();
    let abandoned = staging_dir.join("interrupted");
    fs::write(&abandoned, b"partial").unwrap();

    // Canonical objects are set up by a normal ingest and must survive.
    let kept = workspace
        .blobs()
        .unwrap()
        .ingest(Cursor::new(b"keep me"), None, None, None)
        .unwrap();
    let kept_locator = kept.blob.locator;

    let removed = workspace
        .blobs()
        .unwrap()
        .sweep_staging(Duration::ZERO)
        .unwrap();
    assert_eq!(removed.len(), 1);
    assert!(removed[0].starts_with("staging/"));
    assert!(!abandoned.exists());
    assert!(
        workspace
            .media()
            .unwrap()
            .stat(&kept_locator)
            .unwrap()
            .is_some()
    );
    assert_eq!(workspace.catalog().blob_list().unwrap().len(), 1);
}

#[test]
fn gc_deletes_only_unreferenced_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let catalog = workspace.catalog();
    let blobs = workspace.blobs().unwrap();

    let kept = blobs
        .ingest(Cursor::new(b"referenced"), None, None, None)
        .unwrap();
    let mut entry = Entry::new("media/kept", Lane::Media, Kind::File);
    entry.blob_hash = Some(kept.blob.hash.clone());
    catalog.entry_put(&entry).unwrap();

    let dropped = blobs
        .ingest(Cursor::new(b"orphaned"), None, None, None)
        .unwrap();

    let removed = workspace.blobs().unwrap().sweep_unreferenced().unwrap();
    assert_eq!(removed, vec![dropped.blob.hash.clone()]);
    assert_eq!(catalog.blob_list().unwrap().len(), 1);
    assert!(
        workspace
            .media()
            .unwrap()
            .stat(&dropped.blob.locator)
            .unwrap()
            .is_none()
    );
    assert!(
        workspace
            .media()
            .unwrap()
            .stat(&kept.blob.locator)
            .unwrap()
            .is_some()
    );

    // A soft-deleted entry still references its blob, so a release can keep
    // pointing at it; a second sweep finds nothing.
    catalog.entry_remove("media/kept").unwrap();
    assert!(
        workspace
            .blobs()
            .unwrap()
            .sweep_unreferenced()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn entries_point_at_ingested_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let catalog = workspace.catalog();
    let blobs = workspace.blobs().unwrap();
    let ingested = blobs
        .ingest(Cursor::new(b"clip"), None, None, Some("video/mp4"))
        .unwrap();

    let mut entry = Entry::new("media/clip", Lane::Media, Kind::File);
    entry.blob_hash = Some(ingested.blob.hash.clone());
    entry.mime = ingested.blob.mime.clone();
    entry.size = Some(ingested.blob.size);
    let revision = catalog.entry_put(&entry).unwrap();
    assert_eq!(revision, 1);

    let fetched = catalog.entry_get("media/clip").unwrap().unwrap();
    assert_eq!(
        fetched.blob_hash.as_deref(),
        Some(ingested.blob.hash.as_str())
    );
    assert_eq!(fetched.size, Some(4));

    // Blob reads go through the entry as the CLI does.
    let data = blobs
        .read_range(fetched.blob_hash.as_deref().unwrap(), 1, Some(3))
        .unwrap();
    assert_eq!(data, b"lip");
}

#[test]
fn blob_lane_requires_media_and_init() {
    let no_media_dir = tempfile::tempdir().unwrap();
    let mut no_media = config(no_media_dir.path());
    no_media.media = None;
    let workspace = Workspace::open(&no_media).unwrap();
    assert!(matches!(
        expect_err(workspace.blobs()),
        BlobErr::NoMediaStore
    ));
    assert!(workspace.media().is_none());

    let uninitialized_dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(&config(uninitialized_dir.path())).unwrap();
    assert!(matches!(
        expect_err(workspace.blobs()),
        BlobErr::NotInitialized
    ));
}
