//! Lane counters collected from the catalog and oplog.

use std::{fs, io::Cursor, path::Path};

use graft::remote::RemoteConfig;
use graft_workspace::{Entry, Kind, Lane, MediaConfig, Workspace, WorkspaceConfig, metrics};

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

#[test]
fn metrics_report_oplog_and_blob_counters() {
    let dir = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::open(&config(dir.path())).unwrap();
    workspace.init().unwrap();
    workspace
        .oplog()
        .append("doc-1", "alice", 1, b"first")
        .unwrap();
    workspace
        .oplog()
        .append("doc-1", "alice", 2, b"second")
        .unwrap();

    let kept = vec![7u8; 1_000];
    let ingested = workspace
        .blobs()
        .unwrap()
        .ingest(Cursor::new(&kept), Some(1_000), None, None)
        .unwrap();
    let mut entry = Entry::new("media/keep.bin".to_string(), Lane::Media, Kind::File);
    entry.blob_hash = Some(ingested.blob.hash);
    workspace.catalog().entry_put(&entry).unwrap();
    let orphan = vec![9u8; 500];
    workspace
        .blobs()
        .unwrap()
        .ingest(Cursor::new(&orphan), Some(500), None, None)
        .unwrap();
    assert_eq!(
        workspace
            .blobs()
            .unwrap()
            .sweep_unreferenced()
            .unwrap()
            .len(),
        1
    );

    let metrics = metrics::collect(&workspace).unwrap();
    assert_eq!(metrics.oplog_docs, 1);
    assert_eq!(metrics.oplog_frames, 2);
    assert_eq!(metrics.oplog_bytes, 11);
    assert_eq!(
        metrics.storage,
        vec![graft_workspace::StorageMetric {
            storage: "raw".to_string(),
            blobs: 1,
            bytes: 1_000,
        }]
    );
    assert_eq!(metrics.ingested_blobs, 2);
    assert_eq!(metrics.ingested_bytes, 1_500);
    assert_eq!(metrics.swept_blobs, 1);
    assert_eq!(metrics.swept_bytes, 500);
    assert_eq!(metrics.release_stages, 0);

    // The catalog export remains a valid SQLite database after the backup path
    // was exercised elsewhere; metrics are plain rows in catalog_meta.
    let exported = dir.path().join("export.db");
    workspace.catalog().export_to(&exported).unwrap();
    assert!(fs::metadata(exported).unwrap().len() > 0);
}
