//! Code-lane checkpoints: tree manifests, dedup, pinned blobs, restore
//! verification, ignore rules and git bundles.

use std::{fs, io::Cursor, path::Path, process::Command};

use graft::remote::RemoteConfig;
use graft_workspace::{ArchiveConfig, BlobStorage, MediaConfig, Workspace, WorkspaceConfig, code};

fn config(dir: &Path) -> WorkspaceConfig {
    let key_file = dir.join("archive.key");
    std::fs::write(&key_file, "11".repeat(32)).unwrap();
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
        backup: None,
    }
}

fn initialized(dir: &Path) -> Workspace {
    let mut workspace = Workspace::open(&config(dir)).unwrap();
    workspace.init().unwrap();
    workspace
}

#[test]
fn snapshots_dedup_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let root = dir.path().join("tree");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), b"one\n").unwrap();
    fs::write(root.join("sub").join("b.bin"), [0u8, 1, 2, 3, 255]).unwrap();

    let first = workspace.snapshot_code(&root, "code/demo").unwrap();
    assert_eq!(first.files, 2);
    assert_eq!(first.bytes, 9);
    assert!(first.git_bundle.is_none());
    assert_eq!(
        workspace
            .catalog()
            .entry_get("code/demo")
            .unwrap()
            .unwrap()
            .code_tree
            .as_deref(),
        Some(first.tree.as_str())
    );
    assert_eq!(workspace.catalog().blob_list().unwrap().len(), 3);
    assert!(
        workspace
            .catalog()
            .blob_list()
            .unwrap()
            .iter()
            .all(|blob| blob.pinned),
        "checkpoint blobs are pinned so gc cannot collect them"
    );
    assert!(
        workspace
            .catalog()
            .blob_list()
            .unwrap()
            .iter()
            .all(|blob| blob.storage == BlobStorage::Cdc),
        "code contents live in the encrypted archive, not raw media"
    );

    fs::write(root.join("a.txt"), b"two\n").unwrap();
    let second = workspace.snapshot_code(&root, "code/demo").unwrap();
    assert_ne!(first.tree, second.tree);
    // Only the changed file and the new manifest were added.
    assert_eq!(workspace.catalog().blob_list().unwrap().len(), 5);

    let out = dir.path().join("restored");
    let stats = code::restore(&workspace.blobs().unwrap(), &first.tree, &out).unwrap();
    assert_eq!(stats.files, 2);
    assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"one\n");
    assert_eq!(
        fs::read(out.join("sub").join("b.bin")).unwrap(),
        [0u8, 1, 2, 3, 255]
    );

    let out = dir.path().join("restored2");
    code::restore(&workspace.blobs().unwrap(), &second.tree, &out).unwrap();
    assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"two\n");

    // Everything a checkpoint names stays through a garbage collection.
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
fn workspaceignore_and_manifest_listing() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let root = dir.path().join("tree");
    fs::create_dir_all(root.join("build")).unwrap();
    fs::write(root.join("keep.txt"), b"keep").unwrap();
    fs::write(root.join("build").join("out.o"), b"obj").unwrap();
    fs::write(root.join("skip.tmp"), b"tmp").unwrap();
    fs::write(root.join(".workspaceignore"), b"build/\n*.tmp\n").unwrap();

    let snapshot = workspace.snapshot_code(&root, "code/ignored").unwrap();
    assert_eq!(snapshot.files, 1);
    let files = code::manifest(&workspace.blobs().unwrap(), &snapshot.tree).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "keep.txt");
    assert!(
        files[0].mode & 0o400 != 0,
        "read bits survive the checkpoint"
    );
}

#[test]
fn restores_a_git_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    fs::write(root.join("README.md"), b"hello\n").unwrap();
    git(&root, &["add", "README.md"]);
    git(&root, &["commit", "-q", "-m", "first"]);

    let snapshot = workspace.snapshot_code(&root, "code/repo").unwrap();
    let bundle = snapshot
        .git_bundle
        .clone()
        .expect("a commit produces a bundle");

    // The bundle blob is a real git bundle, not just bytes.
    let bundle_path = dir.path().join("repo.bundle");
    let mut file = fs::File::create(&bundle_path).unwrap();
    code::read_blob(&workspace.blobs().unwrap(), &bundle, &mut file).unwrap();
    drop(file);
    let heads = Command::new("git")
        .args(["bundle", "list-heads"])
        .arg(&bundle_path)
        .output()
        .unwrap();
    assert!(
        heads.status.success(),
        "{}",
        String::from_utf8_lossy(&heads.stderr)
    );
    assert!(String::from_utf8_lossy(&heads.stdout).contains("refs/heads/"));

    let out = dir.path().join("checkout");
    let stats = code::restore(&workspace.blobs().unwrap(), &snapshot.tree, &out).unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(fs::read(out.join("README.md")).unwrap(), b"hello\n");
    assert!(
        !out.join(".git").exists(),
        "restore materialises files, not the repository"
    );
}

#[test]
fn refuses_manifest_paths_that_escape_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let blobs = workspace.blobs().unwrap();

    let malicious = br#"{
        "version": 1,
        "files": [{"path": "../escape.txt", "hash": "blake3:00", "size": 0, "mode": 420}]
    }"#;
    let ingested = blobs
        .ingest(Cursor::new(malicious), None, None, Some("application/json"))
        .unwrap();

    let err = code::restore(&blobs, &ingested.blob.hash, &dir.path().join("out")).unwrap_err();
    assert!(matches!(err, code::CodeErr::UnsafePath(_)), "{err:?}");
    assert!(!dir.path().join("escape.txt").exists());
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
