//! Release staging: media blobs and code trees become a datapackage whose
//! hashes match the staged bytes.

use std::{fs, io::Cursor, path::Path};

use blake3::Hasher as Blake3;
use graft::remote::RemoteConfig;
use graft_workspace::{
    ArchiveConfig, Entry, Kind, Lane, MediaConfig, ReleaseErr, Workspace, WorkspaceConfig, release,
};
use sha2::{Digest, Sha256};

fn config(dir: &Path) -> WorkspaceConfig {
    let key_file = dir.join("archive.key");
    std::fs::write(&key_file, "22".repeat(32)).unwrap();
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
fn stages_media_and_code_with_matching_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());

    let video = b"pretend this is a video";
    let ingested = workspace
        .blobs()
        .unwrap()
        .ingest(Cursor::new(video), None, None, Some("video/mp4"))
        .unwrap();
    let mut entry = Entry::new("media/clip", Lane::Media, Kind::File);
    entry.blob_hash = Some(ingested.blob.hash.clone());
    entry.mime = ingested.blob.mime.clone();
    entry.size = Some(ingested.blob.size);
    workspace.catalog().entry_put(&entry).unwrap();

    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("src")).unwrap();
    fs::write(tree.join("README.md"), b"readme\n").unwrap();
    fs::write(tree.join("src").join("main.rs"), b"fn main() {}\n").unwrap();
    let snapshot = workspace.snapshot_code(&tree, "code/demo").unwrap();

    let out = dir.path().join("staging");
    let staged = release::stage(
        &workspace,
        &["media/clip".to_string(), "code/demo".to_string()],
        &out,
    )
    .unwrap();

    assert_eq!(staged.resources.len(), 3);
    assert!(out.join("datapackage.json").is_file());
    assert_eq!(fs::read(out.join("files/media/clip")).unwrap(), video);
    assert_eq!(
        fs::read(out.join("files/code/demo/README.md")).unwrap(),
        b"readme\n"
    );
    assert_eq!(
        fs::read(out.join("files/code/demo/src/main.rs")).unwrap(),
        b"fn main() {}\n"
    );

    for resource in &staged.resources {
        let bytes = fs::read(out.join(&resource.path)).unwrap();
        assert_eq!(bytes.len() as u64, resource.bytes);
        let hash = format!(
            "blake3:{}",
            Blake3::new().update(&bytes).finalize().to_hex()
        );
        assert_eq!(hash, resource.hash, "{}", resource.path);
        let sha = hex(&Sha256::digest(&bytes));
        assert_eq!(sha, resource.sha256, "{}", resource.path);
    }

    let package = fs::read(out.join("datapackage.json")).unwrap();
    assert_eq!(
        staged.manifest_hash,
        format!(
            "blake3:{}",
            Blake3::new().update(&package).finalize().to_hex()
        )
    );
    let package: serde_json::Value = serde_json::from_slice(&package).unwrap();
    assert_eq!(package["resources"].as_array().unwrap().len(), 3);
    assert_eq!(
        package["workspace"],
        workspace.catalog().workspace_id().unwrap()
    );

    // A tree manifest is pinned, so the staged code cannot vanish underneath
    // the release.
    assert!(
        workspace
            .catalog()
            .blob_get(&snapshot.tree)
            .unwrap()
            .unwrap()
            .pinned
    );

    let err = release::stage(&workspace, &["code/demo".to_string()], &out).unwrap_err();
    assert!(matches!(err, ReleaseErr::NotEmpty(_)), "{err:?}");
}

#[test]
fn refuses_entries_without_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = initialized(dir.path());
    let entry = Entry::new("notes/empty", Lane::Loro, Kind::Doc);
    workspace.catalog().entry_put(&entry).unwrap();

    let err = release::stage(
        &workspace,
        &["notes/empty".to_string()],
        &dir.path().join("staging"),
    )
    .unwrap_err();
    assert!(matches!(err, ReleaseErr::EmptyEntry(_)), "{err:?}");

    let err = release::stage(
        &workspace,
        &["notes/missing".to_string()],
        &dir.path().join("staging2"),
    )
    .unwrap_err();
    assert!(matches!(err, ReleaseErr::NoEntry(_)), "{err:?}");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
