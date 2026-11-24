//! Path and key handling that untrusted input reaches.

use std::{fs, io::Cursor, path::Path};

use graft::remote::RemoteConfig;
use graft_workspace::{
    BackupConfig, BlobErr, Entry, Kind, Lane, MediaConfig, Workspace, WorkspaceConfig, code,
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
        backup: Some(BackupConfig {
            store: MediaConfig::Fs {
                root: dir.join("backup").display().to_string(),
            },
            prefix: "nightly".to_string(),
            archive: None,
        }),
    }
}

#[test]
fn entry_paths_cannot_escape_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();
    for path in ["../escape", "/absolute", "a//b", "a/../b", "a\\b", "a\0b"] {
        let mut entry = Entry::new(path.to_string(), Lane::Media, Kind::File);
        entry.size = Some(0);
        let err = workspace.catalog().entry_put(&entry).unwrap_err();
        assert!(
            matches!(err, graft_workspace::CatalogErr::InvalidPath { .. }),
            "{path:?}: {err}"
        );
    }
}

#[test]
fn backup_and_restore_prefixes_reject_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();
    let err = graft_workspace::backup::run(&workspace, &config, Some("../escape")).unwrap_err();
    assert!(
        matches!(err, graft_workspace::BackupErr::UnsafePrefix(_)),
        "{err}"
    );

    let err = graft_workspace::backup::restore(&mut workspace, &config, "../escape").unwrap_err();
    assert!(
        matches!(err, graft_workspace::BackupErr::UnsafePrefix(_)),
        "{err}"
    );
}

#[test]
fn code_restore_rejects_a_manifest_that_escapes_destination() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();
    let manifest = serde_json::json!({
        "version": 1,
        "files": [{
            "path": "../escaped.txt",
            "hash": "blake3:0000000000000000000000000000000000000000000000000000000000000000",
            "size": 3,
            "mode": 420
        }]
    });
    let json = serde_json::to_vec(&manifest).unwrap();
    let blobs = workspace.blobs().unwrap();
    let tree = blobs
        .ingest(
            Cursor::new(&json),
            Some(json.len() as u64),
            None,
            Some("application/json"),
        )
        .unwrap();
    blobs.pin(&tree.blob.hash).unwrap();

    let dest = dir.path().join("restore");
    let err = code::restore(&blobs, &tree.blob.hash, &dest).unwrap_err();
    assert!(matches!(err, code::CodeErr::UnsafePath(_)), "{err}");
    assert!(!dir.path().join("escaped.txt").exists());
}

#[test]
fn commit_rejects_a_ticket_outside_the_staging_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();
    let err = workspace
        .blobs()
        .unwrap()
        .commit_presigned(
            "media/raw/other/file",
            None,
            "blake3:0000000000000000000000000000000000000000000000000000000000000000",
            None,
        )
        .unwrap_err();
    assert!(matches!(err, BlobErr::BadTicket(_)), "{err}");
}

#[test]
fn archive_backup_does_not_copy_plaintext() {
    // The archive lane requires a key; an empty backup target is enough to show
    // that cdc rows are skipped rather than decrypted into the backup store.
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    let key = (0..32)
        .map(|index| format!("{:02x}", index * 3 + 1))
        .collect::<String>();
    let key_path = dir.path().join("archive.key");
    fs::write(&key_path, key).unwrap();
    config.archive = Some(graft_workspace::ArchiveConfig { key_file: key_path });
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();
    let bytes = b"secret archive bytes";
    let ingested = workspace
        .blobs()
        .unwrap()
        .ingest_cdc(Cursor::new(bytes), None, None, None)
        .unwrap();
    let report = graft_workspace::backup::run(&workspace, &config, None).unwrap();
    assert_eq!(report.skipped_cdc, 1);
    assert!(
        !report
            .blobs
            .iter()
            .any(|blob| blob.locator == ingested.blob.locator),
        "archive blob must not appear in the plaintext backup list"
    );
}
