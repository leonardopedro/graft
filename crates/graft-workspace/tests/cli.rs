//! End-to-end tests of the `workspace` binary.
//!
//! Each step is a separate process, so these also cover reopening the catalog
//! volume from the local Graft index and the filesystem remote.

use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use loro::{ExportMode, LoroDoc};

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

fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn fail(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        !out.status.success(),
        "workspace {args:?} unexpectedly succeeded: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn interrupted_cli_apply_resumes_staged_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let bytes = payload(24 * 1024 * 1024, 91);
    fs::create_dir_all(dir.join("v1")).unwrap();
    fs::write(dir.join("v1/payload.bin"), &bytes).unwrap();
    ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "publish",
            "demo",
            "1.0.0",
            "v1",
        ],
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_workspace"))
        .current_dir(dir)
        .args([
            "update",
            "--repo",
            "fs:./repo",
            "apply",
            "demo",
            "1.0.0",
            "install",
            "--state",
            "state.json",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn workspace apply");

    let staged = dir.join(".install.graft-update/tree/payload.bin");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_partial = false;
    while Instant::now() < deadline {
        if let Ok(meta) = fs::metadata(&staged)
            && meta.len() >= 512 * 1024
            && meta.len() < bytes.len() as u64
        {
            saw_partial = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        saw_partial,
        "apply finished before the staging file was partial"
    );
    assert!(
        child.kill().is_ok(),
        "apply exited before it could be killed"
    );
    let _ = child.wait();
    assert!(!dir.join("state.json").exists());

    let resumed = ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "apply",
            "demo",
            "1.0.0",
            "install",
            "--state",
            "state.json",
        ],
    );
    assert!(resumed.contains("resumed: "), "{resumed}");
    assert_eq!(fs::read(dir.join("install/payload.bin")).unwrap(), bytes);
}

#[test]
fn update_publish_and_prune_share_the_catalog_inventory() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    fs::write(
        dir.join("workspace.toml"),
        concat!(
            "data_dir = \"./data\"\n",
            "\n",
            "[remote]\n",
            "type = \"fs\"\n",
            "root = \"./remote\"\n",
        ),
    )
    .unwrap();
    ok(dir, &["init"]);
    fs::create_dir_all(dir.join("v1")).unwrap();
    fs::write(dir.join("v1/version.txt"), b"one\n").unwrap();
    fs::create_dir_all(dir.join("v2")).unwrap();
    fs::write(dir.join("v2/version.txt"), b"two\n").unwrap();

    ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "publish",
            "demo",
            "1.0.0",
            "v1",
        ],
    );
    let status = ok(dir, &["status"]);
    assert!(status.contains("releases: 1"), "{status}");
    let published = ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "publish",
            "demo",
            "2.0.0",
            "v2",
        ],
    );
    let status = ok(dir, &["status"]);
    assert!(status.contains("releases: 2"), "{status}");

    let pruned = ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "prune-refs",
            "demo",
            "--keep",
            "1",
            "--grace-secs",
            "0",
        ],
    );
    assert!(pruned.contains("removed 1 ref"), "{pruned}");
    let status = ok(dir, &["status"]);
    assert!(status.contains("releases: 1"), "{status}");

    let gc = ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "gc",
            "demo",
            "--grace-secs",
            "0",
        ],
    );
    assert!(gc.contains("removed 1 manifest(s)"), "{gc}");

    // The surviving catalog row pins release 2's manifest even if its ref is
    // removed outside the CLI.
    let manifest = published
        .lines()
        .next()
        .and_then(|line| line.rsplit_once(": "))
        .map(|(_, hash)| hash.to_string())
        .expect("publish prints the manifest hash");
    let manifest_hex = manifest.strip_prefix("blake3:").unwrap();
    let manifest_path = dir
        .join("repo/updates/demo/manifests")
        .join(format!("{manifest_hex}.json"));
    fs::remove_file(dir.join("repo/updates/demo/refs/2.0.0")).unwrap();
    // A differently spelled path to the same store must still match the pin.
    let gc = ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:repo",
            "gc",
            "demo",
            "--grace-secs",
            "0",
        ],
    );
    assert!(gc.contains("removed 0 manifest(s)"), "{gc}");
    assert!(manifest_path.exists());
}

#[test]
fn catalog_survives_between_processes() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let flags = ["--data", "data", "--remote", "fs:remote"];

    // Before init there is no workspace, but the volume exists and is readable.
    let status = ok(dir, &[&flags[..], &["status"]].concat());
    assert!(status.contains("workspace: (none"), "{status}");
    assert!(status.contains("schema: 1"), "{status}");

    let init = ok(dir, &[&flags[..], &["init"]].concat());
    assert!(init.contains("workspace: w-"), "{init}");
    assert!(init.contains("catalog tag: ws-catalog"), "{init}");

    // Running init again keeps the id.
    let again = ok(dir, &[&flags[..], &["init"]].concat());
    let id = init
        .lines()
        .find_map(|line| line.strip_prefix("workspace: "))
        .unwrap();
    assert!(again.contains(&format!("workspace: {id}")), "{again}");

    let put = ok(
        dir,
        &[
            &flags[..],
            &[
                "entry",
                "put",
                "notes/intro",
                "--lane",
                "loro",
                "--kind",
                "doc",
                "--loro-doc",
                "doc-1",
                "--mime",
                "text/markdown",
            ],
        ]
        .concat(),
    );
    assert!(put.contains("revision 1"), "{put}");

    // A fresh process must see the row.
    let get = ok(
        dir,
        &[&flags[..], &["entry", "get", "notes/intro"]].concat(),
    );
    assert!(get.contains("lane: loro"), "{get}");
    assert!(get.contains("kind: doc"), "{get}");
    assert!(get.contains("loro_doc: doc-1"), "{get}");
    assert!(get.contains("revision: 1"), "{get}");

    // Upserts bump the revision instead of adding a row.
    let put = ok(
        dir,
        &[
            &flags[..],
            &[
                "entry",
                "put",
                "notes/intro",
                "--lane",
                "loro",
                "--kind",
                "doc",
            ],
        ]
        .concat(),
    );
    assert!(put.contains("revision 2"), "{put}");
    let status = ok(dir, &[&flags[..], &["status"]].concat());
    assert!(status.contains("entries: 1"), "{status}");

    let ls = ok(dir, &[&flags[..], &["entry", "ls", "notes/"]].concat());
    assert_eq!(ls.lines().count(), 1, "{ls}");
    assert!(ls.starts_with("notes/intro\tloro\tdoc\t2"), "{ls}");

    let rm = ok(dir, &[&flags[..], &["entry", "rm", "notes/intro"]].concat());
    assert!(rm.contains("removed notes/intro"), "{rm}");
    assert_eq!(
        ok(dir, &[&flags[..], &["entry", "ls"]].concat()),
        "",
        "soft-deleted entries must not be listed"
    );
    assert_eq!(
        ok(dir, &[&flags[..], &["entry", "ls", "--all"]].concat())
            .lines()
            .count(),
        1
    );
    assert!(
        fail(dir, &[&flags[..], &["entry", "rm", "notes/intro"]].concat())
            .contains("no live entry")
    );

    // Paths may not escape the workspace, and a missing blob is refused.
    assert!(
        fail(
            dir,
            &[
                &flags[..],
                &[
                    "entry",
                    "put",
                    "../escape",
                    "--lane",
                    "loro",
                    "--kind",
                    "doc"
                ],
            ]
            .concat(),
        )
        .contains("invalid entry path")
    );
    assert!(
        fail(
            dir,
            &[
                &flags[..],
                &[
                    "entry",
                    "put",
                    "media/clip",
                    "--lane",
                    "media",
                    "--kind",
                    "file",
                    "--blob",
                    "blake3:abcd",
                ],
            ]
            .concat(),
        )
        .contains("FOREIGN KEY")
    );
}

#[test]
fn oplog_syncs_between_processes() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let flags = ["--data", "data", "--remote", "fs:remote"];
    let run_ws = |args: &[&str]| ok(dir, &[&flags[..], args].concat());

    let init = run_ws(&["init"]);
    assert!(init.contains("oplog tag: ws-catalog-oplog"), "{init}");

    run_ws(&[
        "entry",
        "put",
        "notes/live",
        "--lane",
        "loro",
        "--kind",
        "doc",
        "--loro-doc",
        "doc-1",
    ]);

    let one = dir.join("frame-1.bin");
    let two = dir.join("frame-2.bin");
    fs::write(&one, b"one").unwrap();
    fs::write(&two, b"second").unwrap();

    let out = run_ws(&[
        "doc",
        "append",
        "doc-1",
        "--peer",
        "alice",
        "--seq",
        "1",
        "--file",
        one.to_str().unwrap(),
    ]);
    assert!(out.contains("applied alice:1"), "{out}");

    // Out-of-order frames are refused with the expected sequence.
    let out = run_ws(&[
        "doc",
        "append",
        "doc-1",
        "--peer",
        "alice",
        "--seq",
        "3",
        "--file",
        two.to_str().unwrap(),
    ]);
    assert!(out.contains("gap alice: expected 2"), "{out}");

    let out = run_ws(&[
        "doc",
        "append",
        "doc-1",
        "--peer",
        "alice",
        "--seq",
        "2",
        "--file",
        two.to_str().unwrap(),
    ]);
    assert!(out.contains("applied alice:2"), "{out}");

    // Replays are duplicates, not new rows.
    let out = run_ws(&[
        "doc",
        "append",
        "doc-1",
        "--peer",
        "alice",
        "--seq",
        "1",
        "--file",
        one.to_str().unwrap(),
    ]);
    assert!(out.contains("duplicate alice:1"), "{out}");

    // A fresh process reads the persisted frames.
    let frames = run_ws(&["doc", "frames", "doc-1"]);
    assert_eq!(frames.lines().count(), 2, "{frames}");
    assert!(frames.contains("alice 1 3"), "{frames}");
    assert!(frames.contains("alice 2 6"), "{frames}");

    // Cursors only return the tail.
    let tail = run_ws(&["doc", "frames", "doc-1", "--cursor", "alice=1"]);
    assert_eq!(tail.lines().count(), 1, "{tail}");
    assert!(tail.starts_with("alice 2 6"), "{tail}");

    // Frames can be written back out byte for byte.
    let out_dir = dir.join("out");
    run_ws(&[
        "doc",
        "frames",
        "doc-1",
        "--out-dir",
        out_dir.to_str().unwrap(),
    ]);
    assert_eq!(fs::read(out_dir.join("alice-1.bin")).unwrap(), b"one");
    assert_eq!(fs::read(out_dir.join("alice-2.bin")).unwrap(), b"second");

    let out = run_ws(&[
        "doc",
        "ack",
        "doc-1",
        "--producer",
        "alice",
        "--consumer",
        "bob",
        "--seq",
        "1",
    ]);
    assert!(out.contains("acked alice through 1 for bob"), "{out}");

    let stats = run_ws(&["doc", "stats", "doc-1"]);
    assert!(stats.contains("frames: 2"), "{stats}");
    assert!(stats.contains("frontier: alice=2"), "{stats}");

    // A compacted frontier lands on the catalog entry.
    let frontier = dir.join("frontier.bin");
    fs::write(&frontier, b"frontier").unwrap();
    let out = run_ws(&[
        "doc",
        "snapshot",
        "doc-1",
        "--file",
        frontier.to_str().unwrap(),
    ]);
    assert!(out.contains("updated 1 entries"), "{out}");
    let entry = run_ws(&["entry", "get", "notes/live"]);
    assert!(entry.contains("loro_frontier: 66726f6e74696572"), "{entry}");

    let docs = run_ws(&["doc", "ls"]);
    assert!(docs.starts_with("doc-1\t1\t2\t9"), "{docs}");
    let status = run_ws(&["status"]);
    assert!(status.contains("docs: 1"), "{status}");
}

#[test]
fn blob_ingest_and_range_reads() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();

    // Without [media] the blob lane must fail with a clear message.
    ok(dir, &["--data", "data", "--remote", "fs:remote", "init"]);
    let unstored = dir.join("unstored.bin");
    fs::write(&unstored, b"x").unwrap();
    let err = fail(
        dir,
        &[
            "blob",
            "put",
            "media/x",
            "--file",
            unstored.to_str().unwrap(),
        ],
    );
    assert!(err.contains("no media store configured"), "{err}");

    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    fs::write(
        dir.join("workspace.toml"),
        r#"
data_dir = "./data"

[remote]
type = "fs"
root = "./remote"

[media]
type = "fs"
root = "./media"
"#,
    )
    .unwrap();
    ok(dir, &["init"]);

    let bytes: Vec<u8> = (0..5_000u32)
        .map(|i| (i.wrapping_mul(37) % 251) as u8)
        .collect();
    let source = dir.join("clip.bin");
    fs::write(&source, &bytes).unwrap();
    let source = source.to_str().unwrap();

    let put = ok(
        dir,
        &[
            "blob",
            "put",
            "media/clip",
            "--file",
            source,
            "--mime",
            "video/mp4",
        ],
    );
    assert!(put.contains("5000 bytes"), "{put}");
    assert!(put.contains("(uploaded)"), "{put}");
    let hash = put.split_whitespace().nth(2).unwrap().to_string();
    assert!(hash.starts_with("blake3:"), "{put}");

    let entry = ok(dir, &["entry", "get", "media/clip"]);
    assert!(entry.contains("lane: media"), "{entry}");
    assert!(entry.contains("kind: file"), "{entry}");
    assert!(entry.contains("size: 5000"), "{entry}");
    assert!(entry.contains(&format!("blob: {hash}")), "{entry}");

    // Re-uploading identical bytes is a catalog lookup, at the same path or a
    // new one.
    let put = ok(dir, &["blob", "put", "media/clip", "--file", source]);
    assert!(put.contains("(deduplicated)"), "{put}");
    let put = ok(dir, &["blob", "put", "media/copy", "--file", source]);
    assert!(put.contains("(deduplicated)"), "{put}");
    assert_eq!(ok(dir, &["blob", "ls"]).lines().count(), 1);

    // Whole-object and ranged reads.
    let out = dir.join("out.bin");
    ok(
        dir,
        &["blob", "get", "media/clip", "--out", out.to_str().unwrap()],
    );
    assert_eq!(fs::read(&out).unwrap(), bytes);

    let part = dir.join("part.bin");
    ok(
        dir,
        &[
            "blob",
            "get",
            "media/clip",
            "--out",
            part.to_str().unwrap(),
            "--offset",
            "100",
            "--limit",
            "64",
        ],
    );
    assert_eq!(fs::read(&part).unwrap(), &bytes[100..164]);

    let shown = ok(dir, &["blob", "show", &hash]);
    assert!(shown.contains("storage: raw"), "{shown}");
    assert!(shown.contains("size: 5000"), "{shown}");

    // Nothing was abandoned, so the sweep is a no-op.
    let sweep = ok(dir, &["blob", "sweep-staging"]);
    assert!(sweep.contains("swept 0 staging objects"), "{sweep}");
}

#[test]
fn clone_adopts_a_workspace_and_pull_fetches_new_commits() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    fs::write(
        dir.join("workspace.toml"),
        r#"
data_dir = "./data"
autosync = 1

[remote]
type = "fs"
root = "./remote"
"#,
    )
    .unwrap();
    let desktop = |args: &[&str]| ok(dir, &[&["--data", "desktop"][..], args].concat());

    let init = desktop(&["init"]);
    let workspace_id = init
        .lines()
        .find_map(|line| line.strip_prefix("workspace: "))
        .unwrap()
        .to_string();

    desktop(&[
        "entry",
        "put",
        "notes/intro",
        "--lane",
        "loro",
        "--kind",
        "doc",
        "--loro-doc",
        "doc-1",
    ]);
    let frame = dir.join("frame-1.bin");
    fs::write(&frame, b"draft one").unwrap();
    desktop(&[
        "doc",
        "append",
        "doc-1",
        "--peer",
        "alice",
        "--seq",
        "1",
        "--file",
        frame.to_str().unwrap(),
    ]);

    // The short-lived process cannot wait for an autosync tick.
    let sync = desktop(&["sync"]);
    assert!(sync.contains("pushed catalog"), "{sync}");
    assert!(sync.contains("pushed oplog"), "{sync}");

    let shared = desktop(&["share"]);
    let reference = shared
        .lines()
        .find_map(|line| line.strip_prefix("ref: "))
        .unwrap()
        .to_string();
    assert!(reference.starts_with("wref:1:"), "{shared}");

    let cloned = ok(dir, &["--data", "laptop", "clone", &reference]);
    assert!(
        cloned.contains(&format!("workspace: {workspace_id}")),
        "{cloned}"
    );
    assert!(cloned.contains("entries: 1"), "{cloned}");
    assert!(cloned.contains("docs: 1"), "{cloned}");

    // The clone is a real workspace, not a pointer: rows and frames are readable.
    let ls = ok(dir, &["--data", "laptop", "entry", "ls"]);
    assert_eq!(ls.lines().count(), 1, "{ls}");
    assert!(ls.starts_with("notes/intro\tloro\tdoc\t1"), "{ls}");
    let frames = ok(dir, &["--data", "laptop", "doc", "frames", "doc-1"]);
    assert_eq!(frames.trim(), "alice 1 9", "{frames}");

    // Later commits arrive with `pull`.
    desktop(&[
        "entry",
        "put",
        "notes/from-desktop",
        "--lane",
        "loro",
        "--kind",
        "doc",
        "--loro-doc",
        "doc-2",
    ]);
    desktop(&["sync"]);
    let pulled = ok(dir, &["--data", "laptop", "pull"]);
    assert!(pulled.contains("pulled catalog"), "{pulled}");
    let ls = ok(dir, &["--data", "laptop", "entry", "ls"]);
    assert!(ls.contains("notes/from-desktop"), "{ls}");

    // Adopting into a directory that already holds a workspace is refused, and
    // the memory remote cannot be pulled from at all.
    assert!(
        fail(dir, &["--data", "laptop", "clone", &reference]).contains("already holds a workspace")
    );
    assert!(
        fail(dir, &["--data", "laptop", "--remote", "memory", "pull"])
            .contains("pull needs a remote")
    );
    assert!(
        fail(dir, &["--data", "laptop", "clone", "not-a-ref"])
            .contains("workspace ref must start with")
    );

    // A reference whose catalog was never pushed must not clone as an empty
    // workspace, and the failed attempt must leave the directory reusable.
    ok(dir, &["--data", "other", "init"]);
    let other_ref = ok(dir, &["--data", "other", "share"])
        .lines()
        .find_map(|line| line.strip_prefix("ref: "))
        .unwrap()
        .to_string();
    for _ in 0..2 {
        let err = fail(dir, &["--data", "lost", "clone", &other_ref]);
        assert!(err.contains("remote has no commits"), "{err}");
    }
}

#[test]
fn reads_configuration_from_workspace_toml() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    fs::write(
        dir.join("workspace.toml"),
        r#"
data_dir = "./data"
catalog_tag = "my-catalog"

[remote]
type = "fs"
root = "./remote"
"#,
    )
    .unwrap();

    let init = ok(dir, &["init"]);
    assert!(init.contains("catalog tag: my-catalog"), "{init}");
    assert!(init.contains("remote: fs:./remote"), "{init}");

    ok(
        dir,
        &[
            "entry",
            "put",
            "specs/design",
            "--lane",
            "loro",
            "--kind",
            "doc",
        ],
    );
    let status = ok(dir, &["status"]);
    assert!(status.contains("entries: 1"), "{status}");
    assert!(status.contains("catalog tag: my-catalog"), "{status}");

    // Environment variables override the file; CLI flags override both.
    let out = Command::new(env!("CARGO_BIN_EXE_workspace"))
        .current_dir(dir)
        .env("WORKSPACE_CATALOG_TAG", "env-catalog")
        .arg("status")
        .output()
        .expect("spawn workspace");
    assert!(out.status.success());
    let status = String::from_utf8_lossy(&out.stdout);
    assert!(status.contains("catalog tag: env-catalog"), "{status}");

    // A different tag is a different catalog volume, so it starts empty.
    assert!(
        status.contains("workspace: (none"),
        "a distinct tag must not reuse the catalog: {status}"
    );
}

#[test]
fn doc_compact_refuses_to_drop_frames_a_consumer_has_not_acked() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let flags = ["--data", "data", "--remote", "fs:remote"];
    ok(dir, &[&flags[..], &["init"]].concat());

    let doc = LoroDoc::new();
    for seq in 1..=3 {
        let file = format!("frame-{seq}.bin");
        doc.get_text("text")
            .insert(0, &format!("frame {seq}"))
            .unwrap();
        fs::write(
            dir.join(&file),
            doc.export(ExportMode::all_updates()).unwrap(),
        )
        .unwrap();
        let out = ok(
            dir,
            &[
                &flags[..],
                &[
                    "doc",
                    "append",
                    "doc-notes",
                    "--peer",
                    "alice",
                    "--seq",
                    &seq.to_string(),
                    "--file",
                    &file,
                ],
            ]
            .concat(),
        );
        assert!(out.contains(&format!("applied alice:{seq}")), "{out}");
    }

    ok(
        dir,
        &[
            &flags[..],
            &[
                "doc",
                "ack",
                "doc-notes",
                "--producer",
                "alice",
                "--consumer",
                "bob",
                "--seq",
                "1",
            ],
        ]
        .concat(),
    );
    let blocked = ok(
        dir,
        &[
            &flags[..],
            &[
                "doc",
                "compact",
                "doc-notes",
                "--producer",
                "alice",
                "--through",
                "2",
            ],
        ]
        .concat(),
    );
    assert!(
        blocked.contains("blocked: bob has acked alice through 1, needs 2"),
        "{blocked}"
    );

    ok(
        dir,
        &[
            &flags[..],
            &[
                "doc",
                "ack",
                "doc-notes",
                "--producer",
                "alice",
                "--consumer",
                "bob",
                "--seq",
                "2",
            ],
        ]
        .concat(),
    );
    let compacted = ok(
        dir,
        &[
            &flags[..],
            &[
                "doc",
                "compact",
                "doc-notes",
                "--producer",
                "alice",
                "--through",
                "2",
            ],
        ]
        .concat(),
    );
    assert!(
        compacted.contains("compacted alice through 2: 2 frames deleted"),
        "{compacted}"
    );

    // A fresh client gets the snapshot rather than the one remaining frame:
    // the snapshot already covers sequence 3.
    let frames = ok(dir, &[&flags[..], &["doc", "frames", "doc-notes"]].concat());
    assert_eq!(frames.lines().count(), 1, "{frames}");
    assert!(frames.starts_with("snapshot "), "{frames}");
    let stats = ok(dir, &[&flags[..], &["doc", "stats", "doc-notes"]].concat());
    assert!(stats.contains("frames: 1"), "{stats}");
    let snapshot_file = dir.join("doc-notes.snapshot");
    let out = ok(
        dir,
        &[
            &flags[..],
            &[
                "doc",
                "frames",
                "doc-notes",
                "--snapshot-out",
                snapshot_file.to_str().unwrap(),
            ],
        ]
        .concat(),
    );
    assert!(out.contains("snapshot "), "{out}");
    LoroDoc::new()
        .import(&fs::read(&snapshot_file).unwrap())
        .unwrap();

    let err = fail(
        dir,
        &[
            &flags[..],
            &[
                "doc",
                "compact",
                "doc-notes",
                "--producer",
                "alice",
                "--through",
                "9",
            ],
        ]
        .concat(),
    );
    assert!(
        err.contains("cannot compact through 9: only 3 frames were applied"),
        "{err}"
    );
}

#[test]
fn cdc_archive_survives_sync_share_and_clone() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let key = (0..32)
        .map(|i| format!("{:02x}", i * 5 + 1))
        .collect::<String>();
    fs::write(dir.join("archive.key"), key).unwrap();
    fs::write(
        dir.join("workspace.toml"),
        "[media]\ntype = \"fs\"\nroot = \"media\"\n\n[archive]\nkey_file = \"archive.key\"\n",
    )
    .unwrap();

    let flags = ["--data", "data", "--remote", "fs:remote"];
    ok(dir, &[&flags[..], &["init"]].concat());
    let bytes = (0..250_000)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(9))
        .collect::<Vec<_>>();
    fs::write(dir.join("payload.bin"), &bytes).unwrap();
    let put = ok(
        dir,
        &[
            &flags[..],
            &[
                "blob",
                "put",
                "media/archive.bin",
                "--file",
                "payload.bin",
                "--storage",
                "cdc",
            ],
        ]
        .concat(),
    );
    assert!(put.contains(" cdc rev 1 (uploaded)"), "{put}");

    // Rewrite the archive volume before publishing it, so the clone below
    // exercises manifests whose chunk ids were re-keyed by GC.
    let gc = ok(dir, &[&flags[..], &["blob", "gc-cdc"]].concat());
    assert!(gc.contains("archive gc:"), "{gc}");
    assert!(gc.contains("old archive volume kept"), "{gc}");

    ok(dir, &[&flags[..], &["sync"]].concat());

    let shared = ok(dir, &[&flags[..], &["share"]].concat());
    let reference = shared
        .lines()
        .find_map(|line| line.strip_prefix("ref: "))
        .expect("share prints a ref")
        .to_string();
    assert_eq!(
        reference.split(':').count(),
        8,
        "archive ref should append two fields: {reference}"
    );

    let clone_flags = ["--data", "data2", "--remote", "fs:remote"];
    ok(dir, &[&clone_flags[..], &["clone", &reference]].concat());
    let get = ok(
        dir,
        &[
            &clone_flags[..],
            &["blob", "get", "media/archive.bin", "--out", "restored.bin"],
        ]
        .concat(),
    );
    assert!(
        get.contains("read media/archive.bin: 250000 bytes"),
        "{get}"
    );
    assert_eq!(fs::read(dir.join("restored.bin")).unwrap(), bytes);
}

#[test]
fn service_generate_writes_systemd_and_launchd_files() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    fs::write(
        dir.join("workspace.toml"),
        concat!(
            "[backup]\n",
            "prefix = \"backups\"\n",
            "[backup.store]\n",
            "type = \"fs\"\n",
            "root = \"./backup-root\"\n",
        ),
    )
    .unwrap();

    let out = ok(
        dir,
        &[
            "service",
            "generate",
            "--kind",
            "systemd",
            "--interval",
            "600",
            "--keep",
            "3",
            "--out",
            "units",
            "--exec",
            "/bin/workspace",
        ],
    );
    assert!(out.contains("workspace-backup.service"), "{out}");
    let service = fs::read_to_string(dir.join("units/workspace-backup.service")).unwrap();
    assert!(
        service.contains("ExecStart=/bin/workspace --config "),
        "{service}"
    );
    assert!(service.contains("backup --keep 3"), "{service}");
    let timer = fs::read_to_string(dir.join("units/workspace-backup.timer")).unwrap();
    assert!(timer.contains("OnUnitActiveSec=600s"), "{timer}");

    let out = ok(
        dir,
        &[
            "service",
            "generate",
            "--kind",
            "launchd",
            "--name",
            "nightly",
            "--out",
            "launchd",
            "--exec",
            "/bin/workspace",
        ],
    );
    assert!(out.contains("com.graft.nightly.plist"), "{out}");
    let plist = fs::read_to_string(dir.join("launchd/com.graft.nightly.plist")).unwrap();
    assert!(plist.contains("<key>StartInterval</key>"), "{plist}");
    assert!(plist.contains("<integer>3600</integer>"), "{plist}");
}

#[test]
fn backup_keep_retains_the_newest_run() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    fs::write(
        dir.join("workspace.toml"),
        concat!(
            "data_dir = \"./data\"\n",
            "\n",
            "[remote]\n",
            "type = \"fs\"\n",
            "root = \"./remote\"\n",
            "\n",
            "[backup]\n",
            "prefix = \"backups\"\n",
            "[backup.store]\n",
            "type = \"fs\"\n",
            "root = \"./backup-root\"\n",
        ),
    )
    .unwrap();

    ok(dir, &["init"]);
    let first = ok(dir, &["backup", "--keep", "1"]);
    let first_prefix = first
        .lines()
        .find_map(|line| line.strip_prefix("backup: "))
        .expect("first backup prefix")
        .to_string();
    assert!(
        first.contains("retention: kept 1 backup(s), pruned 0"),
        "{first}"
    );

    let second = ok(dir, &["backup", "--keep", "1"]);
    assert!(
        second.contains("retention: kept 1 backup(s), pruned 1"),
        "{second}"
    );
    assert!(
        second.contains(&format!("pruned {first_prefix}")),
        "{second}"
    );

    let runs: Vec<_> = fs::read_dir(dir.join("backup-root/backups"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .flat_map(|workspace| {
            fs::read_dir(workspace)
                .unwrap()
                .map(|entry| entry.unwrap().path())
        })
        .filter(|run| run.join("manifest.json").is_file())
        .collect();
    assert_eq!(runs.len(), 1, "only the newest backup remains: {runs:?}");

    let err = fail(dir, &["backup", "--every", "0"]);
    assert!(err.contains("--every must be at least 1 second"), "{err}");
}
