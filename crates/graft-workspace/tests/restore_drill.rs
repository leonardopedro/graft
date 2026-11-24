//! Backup/restore drill: advertise a workspace, restore it into a fresh data
//! directory and media root, and verify catalog rows, media bytes and a code
//! checkpoint still agree. This is the `RemoteConfig::Fs` form of the Phase 5
//! drill, so it runs in CI without a bucket.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_workspace"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn workspace")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "workspace {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn restores_catalog_media_and_code_into_a_fresh_directory() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();

    fs::write(dir.join("archive.key"), "33".repeat(32)).unwrap();
    write_config(dir, "workspace-a.toml", "data-a", "media-a");
    write_config(dir, "workspace-b.toml", "data-b", "media-b");
    ok(dir, &["--config", "workspace-a.toml", "init"]);

    let payload: Vec<u8> = (0..8_000u32)
        .map(|i| (i.wrapping_mul(37) % 251) as u8)
        .collect();
    fs::write(dir.join("payload.bin"), &payload).unwrap();
    ok(
        dir,
        &[
            "--config",
            "workspace-a.toml",
            "blob",
            "put",
            "media/note",
            "--file",
            "payload.bin",
        ],
    );

    let tree = dir.join("tree");
    fs::create_dir_all(tree.join("src")).unwrap();
    fs::write(tree.join("main.rs"), b"fn main() {}\n").unwrap();
    let snapshot = ok(
        dir,
        &[
            "--config",
            "workspace-a.toml",
            "code",
            "snapshot",
            "tree",
            "--path",
            "code/demo",
        ],
    );
    let tree_hash = snapshot
        .split_whitespace()
        .nth(3)
        .expect("tree hash in snapshot output")
        .trim_end_matches(',')
        .to_string();
    assert!(tree_hash.starts_with("blake3:"), "{snapshot}");

    ok(dir, &["--config", "workspace-a.toml", "sync"]);
    let shared = ok(dir, &["--config", "workspace-a.toml", "share"]);
    let reference = shared
        .lines()
        .find_map(|line| line.strip_prefix("ref: "))
        .expect("reference in share output")
        .to_string();

    // The media store is not the Graft remote; a real backup copies it too.
    copy_dir(&dir.join("media-a"), &dir.join("media-b"));

    let cloned = ok(dir, &["--config", "workspace-b.toml", "clone", &reference]);
    assert!(cloned.contains("entries: 2"), "{cloned}");

    let entries = ok(dir, &["--config", "workspace-b.toml", "entry", "ls"]);
    assert!(entries.contains("media/note"), "{entries}");
    assert!(entries.contains("code/demo"), "{entries}");

    ok(
        dir,
        &[
            "--config",
            "workspace-b.toml",
            "blob",
            "get",
            "media/note",
            "--out",
            "restored.bin",
        ],
    );
    assert_eq!(fs::read(dir.join("restored.bin")).unwrap(), payload);

    ok(
        dir,
        &[
            "--config",
            "workspace-b.toml",
            "code",
            "restore",
            &tree_hash,
            "--out",
            "restored-tree",
        ],
    );
    assert_eq!(
        fs::read(dir.join("restored-tree").join("main.rs")).unwrap(),
        b"fn main() {}\n"
    );
}

fn write_config(dir: &Path, name: &str, data: &str, media: &str) {
    fs::write(
        dir.join(name),
        format!(
            r#"
data_dir = "./{data}"

[remote]
type = "fs"
root = "./remote"

[media]
type = "fs"
root = "./{media}"

[archive]
key_file = "./archive.key"
"#
        ),
    )
    .unwrap();
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target: PathBuf = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
