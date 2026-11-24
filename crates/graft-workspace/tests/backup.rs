//! Off-site backup: catalog export, git bundles, and the manifest that names
//! every object.

use std::{fs, io::Cursor, path::Path, process::Command};

use graft::remote::RemoteConfig;
use graft_workspace::{
    ArchiveBackupConfig, ArchiveConfig, BackupConfig, Entry, Kind, Lane, MediaConfig, Workspace,
    WorkspaceConfig, code, code::snapshot,
};
use loro::{ExportMode, LoroDoc};

fn config(dir: &Path) -> WorkspaceConfig {
    let key_file = dir.join("archive.key");
    fs::write(&key_file, "44".repeat(32)).unwrap();
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
        archive: Some(ArchiveConfig { key_file }),
        backup: Some(BackupConfig {
            store: MediaConfig::Fs {
                root: dir.join("backup-root").display().to_string(),
            },
            prefix: "nightly".to_string(),
            archive: None,
        }),
    }
}

fn delta(text: &str) -> Vec<u8> {
    let doc = LoroDoc::new();
    doc.get_text("text").insert(0, text).unwrap();
    doc.export(ExportMode::all_updates()).unwrap()
}

fn commit_repo(root: &Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    fs::create_dir_all(root).unwrap();
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    fs::write(root.join("README.md"), b"# backup test\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "initial"]);
}

#[test]
fn backup_exports_catalog_and_git_bundle_with_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();
    let workspace_id = workspace.catalog().workspace_id().unwrap().to_string();
    workspace
        .oplog()
        .append("doc-1", "alice", 1, &delta("hello"))
        .unwrap();
    // Identifiers may contain dots; a backup key must not become a traversal.
    workspace
        .oplog()
        .append("..", "bob", 1, &delta("dots"))
        .unwrap();

    let repo = dir.path().join("repo");
    commit_repo(&repo);
    let blobs = workspace.blobs().unwrap();
    let tree = snapshot(&blobs, &repo).unwrap();
    assert!(tree.git_bundle.is_some());
    let mut entry = Entry::new("code/app".to_string(), Lane::Code, Kind::Dir);
    entry.code_tree = Some(tree.tree.clone());
    workspace.catalog().entry_put(&entry).unwrap();

    let media_bytes = b"media bytes for restore".to_vec();
    let media = workspace
        .blobs()
        .unwrap()
        .ingest(Cursor::new(&media_bytes), None, None, None)
        .unwrap();
    let mut media_entry = Entry::new("media/photo.bin".to_string(), Lane::Media, Kind::File);
    media_entry.blob_hash = Some(media.blob.hash.clone());
    workspace.catalog().entry_put(&media_entry).unwrap();

    let report = graft_workspace::backup::run(&workspace, &config, None).unwrap();
    assert!(
        report
            .prefix
            .starts_with(&format!("nightly/{workspace_id}/"))
    );
    assert_eq!(report.code_bundles.len(), 1);
    assert_eq!(report.doc_logs.len(), 2);
    let doc = report
        .doc_logs
        .iter()
        .find(|doc| doc.doc_id == "doc-1")
        .unwrap();
    assert_eq!(doc.frames, 1);
    assert!(doc.complete);
    let dots = report
        .doc_logs
        .iter()
        .find(|doc| doc.doc_id == "..")
        .unwrap();
    assert!(
        dots.object.key.ends_with("/docs/_2e_2e.jsonl"),
        "{}",
        dots.object.key
    );
    assert!(!dots.object.key.contains("/../"));

    let root = dir
        .path()
        .join("backup-root")
        .join(report.prefix.replace('/', std::path::MAIN_SEPARATOR_STR));
    assert!(root.join("catalog.db").is_file());
    assert!(root.join("code").is_dir());
    assert!(root.join("docs").is_dir());
    let frame_log = fs::read_to_string(
        root.join(
            doc.object
                .key
                .trim_start_matches(&report.prefix)
                .trim_start_matches('/'),
        ),
    )
    .unwrap();
    assert!(frame_log.contains("\"peer\":\"alice\""), "{frame_log}");
    assert!(frame_log.contains("\"seq\":1"), "{frame_log}");
    let snapshot = doc.snapshot.as_ref().expect("snapshot is written");
    let bytes = fs::read(
        root.join(
            snapshot
                .key
                .trim_start_matches(&report.prefix)
                .trim_start_matches('/'),
        ),
    )
    .unwrap();
    LoroDoc::new().import(&bytes).unwrap();
    let manifest_bytes = fs::read(root.join("manifest.json")).unwrap();
    assert_eq!(
        report.manifest.hash,
        format!("blake3:{}", blake3::hash(&manifest_bytes).to_hex())
    );
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest["workspace"], workspace_id);
    assert_eq!(manifest["version"], 1);
    assert_eq!(
        manifest["catalog"]["hash"], report.catalog.hash,
        "manifest names the uploaded catalog"
    );
    assert_eq!(
        manifest["code_bundles"][0]["hash"],
        tree.git_bundle.unwrap()
    );
    let docs = manifest["doc_logs"].as_array().unwrap();
    assert_eq!(docs.len(), 2);
    assert!(
        docs.iter()
            .any(|doc| doc["doc_id"] == "doc-1" && doc["frames"] == 1)
    );

    // An override moves the base prefix but keeps workspace and timestamp.
    let custom = graft_workspace::backup::run(&workspace, &config, Some("other/place")).unwrap();
    assert!(
        custom
            .prefix
            .starts_with(&format!("other/place/{workspace_id}/"))
    );

    // Restore into a fresh data directory and media root.
    let mut restored_config = config;
    restored_config.data_dir = dir.path().join("data-restored");
    restored_config.media = Some(MediaConfig::Fs {
        root: dir.path().join("media-restored").display().to_string(),
    });
    let mut restored = Workspace::open(&restored_config).unwrap();
    let report =
        graft_workspace::backup::restore(&mut restored, &restored_config, &report.prefix).unwrap();
    assert_eq!(report.workspace, workspace_id);
    assert_eq!(report.entries, 2);
    assert_eq!(
        report.missing_blobs, report.skipped_cdc as i64,
        "code checkpoint bytes are cdc and stay out of the plaintext backup"
    );
    assert_eq!(report.docs, 2);
    assert_eq!(
        restored
            .blobs()
            .unwrap()
            .read_range(&media.blob.hash, 0, None)
            .unwrap(),
        media_bytes
    );
    // The git bundle is in the backup, but the tree manifest is encrypted in
    // the archive volume. A plaintext backup cannot reconstruct the checkpoint;
    // recover code from the bundle or copy the archive separately.
    let out = dir.path().join("code-restored");
    assert!(
        graft_workspace::code::restore(&restored.blobs().unwrap(), &tree.tree, &out).is_err(),
        "code restore needs the encrypted archive volume"
    );
    assert_eq!(
        restored.oplog().frames_since("doc-1", &[]).unwrap().len(),
        1
    );

    // A second restore must not overwrite the now-populated catalog.
    let err = graft_workspace::backup::restore(&mut restored, &restored_config, &report.prefix)
        .unwrap_err();
    assert!(
        matches!(err, graft_workspace::BackupErr::RestoreNotEmpty),
        "{err}"
    );
}

#[test]
fn compacted_documents_back_up_their_snapshot_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();
    workspace
        .oplog()
        .append("doc-1", "alice", 1, &delta("hello"))
        .unwrap();
    workspace
        .oplog()
        .append("doc-1", "alice", 2, &delta("world"))
        .unwrap();

    let report = workspace.compact_idle().unwrap();
    assert_eq!(report.snapshots, 1, "{report:?}");
    assert_eq!(report.frames_deleted, 2, "{report:?}");
    assert!(
        workspace
            .oplog()
            .frames_since("doc-1", &[])
            .unwrap()
            .is_empty()
    );
    // A frame after the snapshot is backed up as the tail and replayed after
    // the snapshot is imported.
    workspace
        .oplog()
        .append("doc-1", "alice", 3, &delta("!"))
        .unwrap();

    let backup = graft_workspace::backup::run(&workspace, &config, None).unwrap();
    let doc = &backup.doc_logs[0];
    assert_eq!(doc.frames, 1);
    assert!(doc.complete);
    assert!(doc.snapshot.is_some());
    assert_eq!(doc.snapshot_frontier, vec![("alice".to_string(), 2)]);

    let mut restored_config = config;
    restored_config.data_dir = dir.path().join("data-restored");
    let mut restored = Workspace::open(&restored_config).unwrap();
    graft_workspace::backup::restore(&mut restored, &restored_config, &backup.prefix).unwrap();
    let snapshot = restored.oplog().snapshot("doc-1").unwrap().unwrap();
    assert_eq!(snapshot.frontier, vec![("alice".to_string(), 2)]);
    let tail = restored.oplog().frames_since("doc-1", &[]).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, 3);
    let loro = LoroDoc::new();
    loro.import(&snapshot.data).unwrap();
    for frame in tail {
        loro.import(&frame.data).unwrap();
    }
    let text = loro.get_text("text").to_string();
    assert!(
        text.contains("hello") && text.contains("world") && text.contains('!'),
        "{text}"
    );
}

#[test]
fn encrypted_archive_backup_restores_code_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.backup.as_mut().unwrap().archive = Some(ArchiveBackupConfig {
        remote: RemoteConfig::Fs {
            root: dir
                .path()
                .join("archive-backup-remote")
                .display()
                .to_string(),
        },
        data_dir: dir.path().join("archive-backup-data"),
    });
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();

    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("src")).unwrap();
    fs::write(tree.join("README.md"), b"# archive backup\n").unwrap();
    fs::write(tree.join("src/main.rs"), b"fn main() {}\n").unwrap();
    let snapshot = workspace.snapshot_code(&tree, "code/demo").unwrap();

    let report = graft_workspace::backup::run(&workspace, &config, None).unwrap();
    assert_eq!(report.skipped_cdc, 0);
    let archive = report
        .archive
        .as_ref()
        .expect("encrypted archive is backed up");
    assert!(archive.blobs >= 3, "manifest plus two files: {archive:?}");
    assert!(
        dir.path()
            .join("backup-root")
            .join(report.prefix.rsplit_once('/').unwrap().0)
            .join("archive.json")
            .is_file(),
        "archive volume reference is written beside the runs"
    );

    let mut restored_config = config.clone();
    restored_config.data_dir = dir.path().join("restored-data");
    restored_config.media = Some(MediaConfig::Fs {
        root: dir.path().join("restored-media").display().to_string(),
    });
    restored_config.backup.as_mut().unwrap().archive = Some(ArchiveBackupConfig {
        remote: RemoteConfig::Fs {
            root: dir
                .path()
                .join("archive-backup-remote")
                .display()
                .to_string(),
        },
        data_dir: dir.path().join("archive-restore-data"),
    });
    let mut restored = Workspace::open(&restored_config).unwrap();
    let restored_report =
        graft_workspace::backup::restore(&mut restored, &restored_config, &report.prefix).unwrap();
    assert_eq!(restored_report.skipped_cdc, 0);
    assert_eq!(restored_report.missing_blobs, 0);

    let out = dir.path().join("restored-tree");
    let stats = code::restore(&restored.blobs().unwrap(), &snapshot.tree, &out).unwrap();
    assert_eq!(stats.files, 2);
    assert_eq!(
        fs::read(out.join("README.md")).unwrap(),
        b"# archive backup\n"
    );
    assert_eq!(
        fs::read(out.join("src/main.rs")).unwrap(),
        b"fn main() {}\n"
    );
}

#[test]
fn archive_backup_gc_drops_blobs_only_referenced_by_pruned_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.backup.as_mut().unwrap().archive = Some(ArchiveBackupConfig {
        remote: RemoteConfig::Fs {
            root: dir
                .path()
                .join("archive-backup-remote")
                .display()
                .to_string(),
        },
        data_dir: dir.path().join("archive-backup-data"),
    });
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();

    let old_bytes = b"old cdc bytes that only the first backup needs".to_vec();
    let old = workspace
        .blobs()
        .unwrap()
        .ingest_cdc(Cursor::new(&old_bytes), None, None, None)
        .unwrap();
    let first = graft_workspace::backup::run(&workspace, &config, None).unwrap();
    assert_eq!(first.archive.as_ref().unwrap().blobs, 1);

    // The first backup keeps the orphaned row; the live catalog no longer does.
    workspace.catalog().blob_delete(&old.blob.hash).unwrap();

    // Backup prefixes include a one-second timestamp.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let new_bytes = b"new cdc bytes retained by the second backup".to_vec();
    let new = workspace
        .blobs()
        .unwrap()
        .ingest_cdc(Cursor::new(&new_bytes), None, None, None)
        .unwrap();
    let mut new_entry = Entry::new("media/new.bin".to_string(), Lane::Media, Kind::File);
    new_entry.blob_hash = Some(new.blob.hash.clone());
    workspace.catalog().entry_put(&new_entry).unwrap();
    let second = graft_workspace::backup::run(&workspace, &config, None).unwrap();

    let retention = graft_workspace::backup::prune(&workspace, &config, None, 1).unwrap();
    assert_eq!(retention.kept, vec![second.prefix.clone()]);
    assert_eq!(retention.pruned, vec![first.prefix]);
    let gc = retention
        .archive_gc
        .expect("archive GC rewrites the volume");
    assert!(gc.files_removed >= 1, "{gc:?}");
    assert!(gc.stats.pages_reclaimed() > 0, "{gc:?}");

    // The rewritten volume still restores the retained backup's CDC bytes.
    let mut restored_config = config.clone();
    restored_config.data_dir = dir.path().join("restored-data");
    restored_config.backup.as_mut().unwrap().archive = Some(ArchiveBackupConfig {
        remote: RemoteConfig::Fs {
            root: dir
                .path()
                .join("archive-backup-remote")
                .display()
                .to_string(),
        },
        data_dir: dir.path().join("archive-restore-data"),
    });
    let mut restored = Workspace::open(&restored_config).unwrap();
    let report =
        graft_workspace::backup::restore(&mut restored, &restored_config, &second.prefix).unwrap();
    assert_eq!(report.skipped_cdc, 0);
    assert_eq!(
        restored
            .blobs()
            .unwrap()
            .read_range(&new.blob.hash, 0, None)
            .unwrap(),
        new_bytes
    );
}

#[test]
fn backup_without_a_target_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.backup = None;
    let mut workspace = Workspace::open(&config).unwrap();
    workspace.init().unwrap();
    let err = graft_workspace::backup::run(&workspace, &config, None).unwrap_err();
    assert!(matches!(err, graft_workspace::BackupErr::NoBackup), "{err}");
}
