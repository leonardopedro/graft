//! Executes the commands from the workspace guides.
//!
//! One demo run backs `quickstart.md`, `clone.md` and `index.md`: the guide
//! transcript is this run, so ids, refs and hashes agree across the pages. Each
//! executed command and stable output is also checked to still appear on the
//! pages, so drift fails in either direction: a renamed CLI flag fails when the
//! command runs, and a documentation-only edit fails the presence check.
//! Asserted outputs are the stable strings the pages show; random ids and
//! timestamps are deliberately not pinned. `layout.md` has no transcript of its
//! own, so its test checks the names, tables and object paths it describes
//! against the schema files and a live workspace.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const QUICKSTART: &str = "quickstart.md";
const CLONE: &str = "clone.md";
const USAGE: &str = "index.md";
const LAYOUT: &str = "layout.md";
// The run the guides were captured from; every page must show these values.
const DEMO_ID: &str = "w-Q7PC828WuhwSN6jB";
const DEMO_REF: &str = "wref:1:5rMKZjMKvz-3N7frv6Fi5wbQ:74ghR5neqg-3PaCAezZvSp7c:5rMKZjMKvz-2qH6BJz1wpp3P:74ghR5neqg-2vaRfvmeHi9HM";
const PHOTO_HASH: &str = "blake3:852d942d0c9bc1c233cd09dfbc91b6ca0a564a14d48435be165ff3a97ea4319a";

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/src/content/docs/docs")
}

fn page(name: &str) -> String {
    let dir = if name == LAYOUT {
        docs_dir().join("concepts")
    } else {
        docs_dir().join("workspace")
    };
    let path = dir.join(name);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// Strip line continuations and collapse whitespace, so a command matches the
/// shell text whether the page wraps it across lines or not.
fn normalize(text: &str) -> String {
    text.replace("\\\n", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn assert_documented(page_name: &str, snippet: &str) {
    let text = normalize(&page(page_name));
    let snippet = normalize(snippet);
    assert!(
        text.contains(&snippet),
        "{page_name} no longer contains `{snippet}`"
    );
}

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

#[cfg(unix)]
#[test]
fn signed_update_walkthrough_runs_as_documented() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    fs::create_dir_all(dir.join("v1/bin")).unwrap();
    fs::write(dir.join("v1/bin/demo"), b"#!/bin/sh\necho demo 1.0.0\n").unwrap();
    fs::write(dir.join("v1/version.txt"), b"version 1\n").unwrap();
    let mut permissions = fs::metadata(dir.join("v1/bin/demo")).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(dir.join("v1/bin/demo"), permissions).unwrap();

    assert_documented(
        "updates.md",
        "workspace update keygen --private-key private.key --public-key public.key",
    );
    let keygen = ok(
        dir,
        &[
            "update",
            "keygen",
            "--private-key",
            "private.key",
            "--public-key",
            "public.key",
        ],
    );
    assert_eq!(keygen, "private key: private.key\npublic key: public.key\n");

    assert_documented(
        "updates.md",
        "workspace update --repo fs:./repo publish demo 1.0.0 ./v1 --signing-key private.key",
    );
    let published = ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "publish",
            "demo",
            "1.0.0",
            "./v1",
            "--signing-key",
            "private.key",
        ],
    );
    let manifest = "blake3:a0782adaf919d8729e20472299d541333b1c37bf2ad719f0b3b5f510ca6a9120";
    assert_eq!(
        published,
        format!(
            "published demo/1.0.0: {manifest}\nsigned: ed25519\nfiles: 2, chunks: 2, bytes: 36\nuploaded chunks: 2 (36 bytes), reused: 0\n"
        )
    );
    assert_documented("updates.md", manifest);

    assert_documented(
        "updates.md",
        "workspace update --repo fs:./repo apply demo 1.0.0 ./install --state state.json --public-key public.key",
    );
    let applied = ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "apply",
            "demo",
            "1.0.0",
            "./install",
            "--state",
            "state.json",
            "--public-key",
            "public.key",
        ],
    );
    assert_eq!(
        applied,
        "applied demo/1.0.0 to ./install\nfiles: 2 written, 0 unchanged, 0 deleted\nchunks: 2 downloaded (36 bytes), 0 reused (0 bytes)\nstate: state.json\n"
    );

    ok(
        dir,
        &[
            "update",
            "keygen",
            "--private-key",
            "other-private.key",
            "--public-key",
            "other-public.key",
        ],
    );
    let rejected = run(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "apply",
            "demo",
            "1.0.0",
            "./wrong-install",
            "--state",
            "wrong.json",
            "--public-key",
            "other-public.key",
        ],
    );
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert_eq!(
        stderr,
        "error: release demo/1.0.0 has no valid signature from the required key\n"
    );
    assert_documented(
        "updates.md",
        "error: release demo/1.0.0 has no valid signature from the required key",
    );

    assert_eq!(
        fs::read(dir.join("install/bin/demo")).unwrap(),
        b"#!/bin/sh\necho demo 1.0.0\n"
    );
    assert!(!dir.join("wrong-install").exists() || !dir.join("wrong-install/bin/demo").exists());
}

#[test]
fn compressed_update_walkthrough_runs_as_documented() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    fs::create_dir_all(dir.join("v4")).unwrap();
    let mut text = Vec::new();
    for _ in 0..50 {
        text.extend_from_slice(b"compressible release data\n");
    }
    fs::write(dir.join("v4/notes.txt"), &text).unwrap();

    assert_documented(
        "updates.md",
        "workspace update --repo fs:./repo publish demo 4.0.0 ./v4 --compress-level 9",
    );
    let published = ok(
        dir,
        &[
            "update",
            "--repo",
            "fs:./repo",
            "publish",
            "demo",
            "4.0.0",
            "./v4",
            "--compress-level",
            "9",
        ],
    );
    let manifest = "blake3:50700a36c9b818608659a8b1a0a59057934dc79405bff53af61245982be4d11b";
    let output = format!(
        "published demo/4.0.0: {manifest}\nfiles: 1, chunks: 1, bytes: 1300\nuploaded chunks: 1 (44 bytes), reused: 0\ncompressed chunks: 1 (zstd level 9)\n"
    );
    assert_eq!(published, output);
    for snippet in [
        manifest,
        "files: 1, chunks: 1, bytes: 1300",
        "uploaded chunks: 1 (44 bytes), reused: 0",
        "compressed chunks: 1 (zstd level 9)",
    ] {
        assert_documented("updates.md", snippet);
    }

    let chunks = dir.join("repo/updates/demo/chunks");
    let mut compressed = 0usize;
    for shard in fs::read_dir(&chunks).unwrap() {
        let shard = shard.unwrap().path();
        for object in fs::read_dir(shard).unwrap() {
            if object
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "zst")
            {
                compressed += 1;
            }
        }
    }
    assert_eq!(compressed, 1);
}

#[test]
fn guides_run_as_documented() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("field-notes");
    fs::create_dir_all(project.join("drafts")).unwrap();
    fs::write(
        project.join("workspace.toml"),
        concat!(
            "data_dir = \"./data\"\n",
            "autosync = 1\n",
            "\n",
            "[remote]\n",
            "type = \"fs\"\n",
            "root = \"./remote\"\n",
            "\n",
            "[media]\n",
            "type = \"fs\"\n",
            "root = \"./media-store\"\n",
        ),
    )
    .unwrap();
    // The quick start creates these with `mkdir` and `printf`.
    fs::write(
        project.join("drafts/notes.md"),
        "# Field notes\n\nFirst draft.\n",
    )
    .unwrap();
    fs::write(project.join("drafts/photo.jpg"), "photo-bytes-0123456789").unwrap();
    // The CLI usage page continues this project with two more frames.
    fs::write(project.join("drafts/frame-2.bin"), "frame-two").unwrap();
    fs::write(project.join("drafts/frame-3.bin"), "frame-thr").unwrap();
    let project = project.as_path();

    // Quick start, section 2.
    assert_documented(QUICKSTART, "workspace init");
    let init = ok(project, &["init"]);
    let workspace_id = init
        .lines()
        .find_map(|line| line.strip_prefix("workspace: "))
        .expect("init prints the workspace id")
        .to_string();

    // Section 3: entries point at documents; frames and the frontier are the
    // loom state the oplog replays.
    assert_documented(
        QUICKSTART,
        "workspace entry put notes/field --lane loro --kind doc \
         --loro-doc doc-notes --mime text/markdown",
    );
    let out = ok(
        project,
        &[
            "entry",
            "put",
            "notes/field",
            "--lane",
            "loro",
            "--kind",
            "doc",
            "--loro-doc",
            "doc-notes",
            "--mime",
            "text/markdown",
        ],
    );
    assert!(out.contains("put notes/field: revision 1"), "{out}");

    assert_documented(
        QUICKSTART,
        "workspace doc append doc-notes --peer alice --seq 1 --file drafts/notes.md",
    );
    let out = ok(
        project,
        &[
            "doc",
            "append",
            "doc-notes",
            "--peer",
            "alice",
            "--seq",
            "1",
            "--file",
            "drafts/notes.md",
        ],
    );
    assert!(out.contains("applied alice:1"), "{out}");

    assert_documented(
        QUICKSTART,
        "workspace doc frames doc-notes --out-dir frames",
    );
    let out = ok(
        project,
        &["doc", "frames", "doc-notes", "--out-dir", "frames"],
    );
    assert!(out.contains("alice 1 frames/alice-1.bin"), "{out}");
    assert_documented(QUICKSTART, "alice 1 frames/alice-1.bin");

    fs::write(project.join("frontier.bin"), "frontier-bytes").unwrap();
    assert_documented(
        QUICKSTART,
        "workspace doc snapshot doc-notes --file frontier.bin",
    );
    let out = ok(
        project,
        &["doc", "snapshot", "doc-notes", "--file", "frontier.bin"],
    );
    assert!(
        out.contains("snapshot doc-notes: updated 1 entries"),
        "{out}"
    );

    let out = ok(project, &["entry", "get", "notes/field"]);
    assert!(
        out.contains("loro_frontier: 66726f6e746965722d6279746573"),
        "{out}"
    );
    assert_documented(QUICKSTART, "loro_frontier: 66726f6e746965722d6279746573");

    // Section 4: media bytes are content addressed and deduplicated.
    assert_documented(
        QUICKSTART,
        "workspace blob put media/photo.jpg --file drafts/photo.jpg --mime image/jpeg",
    );
    let out = ok(
        project,
        &[
            "blob",
            "put",
            "media/photo.jpg",
            "--file",
            "drafts/photo.jpg",
            "--mime",
            "image/jpeg",
        ],
    );
    assert!(out.contains("rev 1 (uploaded)"), "{out}");
    let out = ok(
        project,
        &[
            "blob",
            "put",
            "media/photo.jpg",
            "--file",
            "drafts/photo.jpg",
            "--mime",
            "image/jpeg",
        ],
    );
    assert!(out.contains("rev 2 (deduplicated)"), "{out}");
    assert_documented(QUICKSTART, "rev 2 (deduplicated)");

    assert_documented(
        QUICKSTART,
        "workspace blob get media/photo.jpg --out thumb.bin --offset 6 --limit 5",
    );
    let out = ok(
        project,
        &[
            "blob",
            "get",
            "media/photo.jpg",
            "--out",
            "thumb.bin",
            "--offset",
            "6",
            "--limit",
            "5",
        ],
    );
    assert!(
        out.contains("read media/photo.jpg: 5 bytes at offset 6"),
        "{out}"
    );
    assert_eq!(fs::read(project.join("thumb.bin")).unwrap(), b"bytes");
    assert_documented(QUICKSTART, "read media/photo.jpg: 5 bytes at offset 6");

    // Section 5: listings and status.
    let out = ok(project, &["entry", "ls"]);
    assert_eq!(
        out,
        "media/photo.jpg\tmedia\tfile\t2\nnotes/field\tloro\tdoc\t1\n"
    );
    assert_documented(QUICKSTART, "media/photo.jpg\tmedia\tfile\t2");
    assert_documented(QUICKSTART, "notes/field\tloro\tdoc\t1");

    let out = ok(project, &["blob", "ls"]);
    assert!(
        out.contains(PHOTO_HASH) && out.contains("22\traw\t-"),
        "{out}"
    );
    let out = ok(project, &["doc", "ls"]);
    assert_eq!(out.trim(), "doc-notes\t1\t1\t28");
    assert_documented(QUICKSTART, "doc-notes\t1\t1\t28");

    let out = ok(project, &["status"]);
    assert!(out.contains("entries: 2"), "{out}");
    assert!(out.contains("docs: 1"), "{out}");
    assert!(out.contains("blobs: 1 (pinned 0)"), "{out}");
    assert_documented(QUICKSTART, "entries: 2");
    assert_documented(QUICKSTART, "docs: 1");

    assert_documented(QUICKSTART, &format!("workspace blob show {PHOTO_HASH}"));
    let out = ok(project, &["blob", "show", PHOTO_HASH]);
    assert!(
        out.contains("sha256: 4ac4b337c77b99e9898733d63edb767717fe22496d786ba032abce369da7f6d0"),
        "{out}"
    );
    assert!(out.contains("refcount: 0"), "{out}");
    let hex = PHOTO_HASH.strip_prefix("blake3:").unwrap();
    assert!(
        out.contains(&format!("media/raw/{workspace_id}/{hex}")),
        "{out}"
    );

    // Section 6: an explicit push is what makes the volumes visible remotely.
    assert_documented(QUICKSTART, "workspace sync");
    let out = ok(project, &["sync"]);
    assert!(out.contains("pushed catalog "), "{out}");
    assert!(out.contains("pushed oplog "), "{out}");

    // Clone guide, sections 1-3, continuing the same project.
    assert_documented(CLONE, "workspace sync");
    assert_documented(CLONE, "workspace share");
    let shared = ok(project, &["share"]);
    let reference = shared
        .lines()
        .find_map(|line| line.strip_prefix("ref: "))
        .expect("share prints the reference")
        .to_string();
    assert!(reference.starts_with("wref:1:"), "{reference}");
    assert_eq!(reference.split(':').count(), 6, "{reference}");

    assert_documented(CLONE, "workspace share | sed -n 's/^ref: //p'");
    assert_documented(CLONE, "workspace clone \"$REF\" --data data-laptop");
    let out = ok(project, &["clone", &reference, "--data", "data-laptop"]);
    assert!(out.contains(&format!("workspace: {workspace_id}")), "{out}");
    assert!(out.contains("entries: 2"), "{out}");
    assert!(out.contains("docs: 1"), "{out}");

    assert_documented(CLONE, "workspace status --data data-laptop");
    assert_documented(CLONE, "workspace entry ls --data data-laptop");
    assert_documented(CLONE, "workspace doc frames doc-notes --data data-laptop");
    let out = ok(project, &["status", "--data", "data-laptop"]);
    assert!(out.contains("entries: 2"), "{out}");
    let out = ok(project, &["entry", "ls", "--data", "data-laptop"]);
    assert_eq!(
        out,
        "media/photo.jpg\tmedia\tfile\t2\nnotes/field\tloro\tdoc\t1\n"
    );
    let out = ok(
        project,
        &["doc", "frames", "doc-notes", "--data", "data-laptop"],
    );
    assert_eq!(out.trim(), "alice 1 28");
    assert_documented(CLONE, "alice 1 28");

    assert_documented(
        CLONE,
        "workspace entry put notes/from-desktop --lane loro --kind doc \
         --loro-doc doc-2 --mime text/markdown",
    );
    ok(
        project,
        &[
            "entry",
            "put",
            "notes/from-desktop",
            "--lane",
            "loro",
            "--kind",
            "doc",
            "--loro-doc",
            "doc-2",
            "--mime",
            "text/markdown",
        ],
    );
    assert_documented(CLONE, "pushed catalog");
    ok(project, &["sync"]);
    assert_documented(CLONE, "workspace pull --data data-laptop");
    let out = ok(project, &["pull", "--data", "data-laptop"]);
    assert!(out.contains("pulled catalog "), "{out}");
    assert!(out.contains("pulled oplog "), "{out}");

    let out = ok(project, &["entry", "ls", "--data", "data-laptop"]);
    assert!(
        out.contains("notes/from-desktop\tloro\tdoc\t1"),
        "pull did not bring the new entry across: {out}"
    );
    assert_documented(CLONE, "notes/from-desktop\tloro\tdoc\t1");

    // The CLI usage page continues this same project with a soft delete, the
    // gap/duplicate protocol, acks, stats and a sweep.
    let usage_commands = [
        "workspace init",
        "workspace status",
        "workspace entry put notes/field --lane loro --kind doc \
         --loro-doc doc-notes --mime text/markdown",
        "workspace entry get notes/field",
        "workspace entry ls",
        "workspace entry ls --all",
        "workspace entry rm notes/from-desktop",
        "workspace entry ls notes/",
        "workspace doc append doc-notes --peer alice --seq 1 --file drafts/notes.md",
        "workspace doc append doc-notes --peer alice --seq 3 --file drafts/frame-3.bin",
        "workspace doc append doc-notes --peer alice --seq 2 --file drafts/frame-2.bin",
        "workspace doc frames doc-notes",
        "workspace doc frames doc-notes --cursor alice=1",
        "workspace doc frames doc-notes --cursor alice=1 --out-dir frames-extra",
        "workspace doc ack doc-notes --producer alice --consumer bob --seq 3",
        "workspace doc stats doc-notes",
        "workspace doc ls",
        "workspace doc snapshot doc-notes --file frontier.bin",
        "workspace blob put media/photo.jpg --file drafts/photo.jpg --mime image/jpeg",
        "workspace blob get media/photo.jpg --out thumb.bin --offset 6 --limit 5",
        "workspace blob ls",
        "workspace blob sweep-staging --older-than 86400",
        "workspace sync",
        "workspace share",
        "workspace clone \"$REF\" --data data-laptop",
        "workspace pull --data data-laptop",
    ];
    for command in usage_commands {
        assert_documented(USAGE, command);
    }
    assert_documented(USAGE, &format!("workspace blob show {PHOTO_HASH}"));

    // The page's entry-soft-delete transcript.
    assert_documented(USAGE, "removed notes/from-desktop");
    assert_eq!(
        ok(project, &["entry", "rm", "notes/from-desktop"]),
        "removed notes/from-desktop\n"
    );
    assert_eq!(
        ok(project, &["entry", "ls", "notes/"]),
        "notes/field\tloro\tdoc\t1\n"
    );
    assert_eq!(
        ok(project, &["entry", "ls", "--all"]),
        concat!(
            "media/photo.jpg\tmedia\tfile\t2\n",
            "notes/field\tloro\tdoc\t1\n",
            "notes/from-desktop\tloro\tdoc\t2\n",
        )
    );
    assert_documented(USAGE, "notes/from-desktop\tloro\tdoc\t2");

    // Frame 3 is a gap, so it is rejected; frame 2 fills it; frame 1 is a duplicate.
    assert_eq!(
        ok(
            project,
            &[
                "doc",
                "append",
                "doc-notes",
                "--peer",
                "alice",
                "--seq",
                "3",
                "--file",
                "drafts/frame-3.bin",
            ],
        ),
        "gap alice: expected 2\n"
    );
    assert_eq!(
        ok(
            project,
            &[
                "doc",
                "append",
                "doc-notes",
                "--peer",
                "alice",
                "--seq",
                "2",
                "--file",
                "drafts/frame-2.bin",
            ],
        ),
        "applied alice:2\n"
    );
    assert_eq!(
        ok(
            project,
            &[
                "doc",
                "append",
                "doc-notes",
                "--peer",
                "alice",
                "--seq",
                "1",
                "--file",
                "drafts/notes.md",
            ],
        ),
        "duplicate alice:1 (already applied)\n"
    );

    assert_eq!(
        ok(project, &["doc", "frames", "doc-notes"]),
        "alice 1 28\nalice 2 9\n"
    );
    assert_eq!(
        ok(
            project,
            &["doc", "frames", "doc-notes", "--cursor", "alice=1"],
        ),
        "alice 2 9\n"
    );
    assert_eq!(
        ok(
            project,
            &[
                "doc",
                "frames",
                "doc-notes",
                "--cursor",
                "alice=1",
                "--out-dir",
                "frames-extra",
            ],
        ),
        "alice 2 frames-extra/alice-2.bin\n"
    );
    assert_eq!(
        fs::read(project.join("frames-extra/alice-2.bin")).unwrap(),
        b"frame-two"
    );
    assert_documented(USAGE, "alice 2 frames-extra/alice-2.bin");

    assert_eq!(
        ok(
            project,
            &[
                "doc",
                "ack",
                "doc-notes",
                "--producer",
                "alice",
                "--consumer",
                "bob",
                "--seq",
                "3",
            ],
        ),
        "acked alice through 2 for bob\n"
    );
    assert_documented(USAGE, "acked alice through 2 for bob");
    let stats = ok(project, &["doc", "stats", "doc-notes"]);
    assert_eq!(
        stats,
        "doc: doc-notes\npeers: 1\nframes: 2\nbytes: 37\nfrontier: alice=2\n"
    );
    assert_documented(USAGE, "bytes: 37");
    assert_documented(USAGE, "frontier: alice=2");
    assert_eq!(ok(project, &["doc", "ls"]), "doc-notes\t1\t2\t37\n");
    assert_documented(USAGE, "doc-notes\t1\t2\t37");

    assert_eq!(
        ok(project, &["blob", "sweep-staging", "--older-than", "86400"]),
        "swept 0 staging objects\n"
    );
    assert_documented(USAGE, "swept 0 staging objects");

    // The page's full blob record comes from this run's media object.
    assert_documented(
        USAGE,
        "sha256: 4ac4b337c77b99e9898733d63edb767717fe22496d786ba032abce369da7f6d0",
    );
    assert_documented(USAGE, &format!("media/raw/{DEMO_ID}/"));

    // All three pages show the same captured run. The reference appears on the
    // pages that use it; the id appears everywhere.
    for name in [QUICKSTART, CLONE, USAGE] {
        assert_documented(name, DEMO_ID);
    }
    for name in [CLONE, USAGE] {
        assert_documented(name, DEMO_REF);
    }

    let out = ok(project, &["status"]);
    assert!(out.contains("entries: 2"), "{out}");
    assert_documented(USAGE, "entries: 2");
}
#[test]
fn layout_page_matches_the_storage_stack() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    fs::write(
        project.join("workspace.toml"),
        concat!(
            "data_dir = \"./data\"\n",
            "autosync = 1\n",
            "\n",
            "[remote]\n",
            "type = \"fs\"\n",
            "root = \"./remote\"\n",
            "\n",
            "[media]\n",
            "type = \"fs\"\n",
            "root = \"./media\"\n",
        ),
    )
    .unwrap();
    fs::write(project.join("pic.bin"), "layout-bytes").unwrap();

    let init = ok(project, &["init"]);
    let workspace_id = init
        .lines()
        .find_map(|line| line.strip_prefix("workspace: "))
        .expect("init prints the workspace id")
        .to_string();
    assert_documented(LAYOUT, "ws-catalog");
    assert_documented(LAYOUT, "ws-catalog-oplog");
    assert!(init.contains("catalog tag: ws-catalog"), "{init}");
    assert!(init.contains("oplog tag: ws-catalog-oplog"), "{init}");

    // Every table the page lists exists in the workspace schema.
    let sql_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("sql");
    let catalog = fs::read_to_string(sql_dir.join("catalog.sql")).unwrap();
    let oplog = fs::read_to_string(sql_dir.join("oplog.sql")).unwrap();
    for table in [
        "catalog_meta",
        "entries",
        "blobs",
        "releases",
        "update_releases",
    ] {
        assert!(
            catalog.contains(&format!("CREATE TABLE {table}")),
            "catalog.sql no longer creates {table}"
        );
        assert_documented(LAYOUT, table);
    }
    for table in ["oplog_meta", "docs", "producers", "frames", "acks"] {
        assert!(
            oplog.contains(&format!("CREATE TABLE {table}")),
            "oplog.sql no longer creates {table}"
        );
        assert_documented(LAYOUT, table);
    }

    // A live ingest lands under the media paths the page draws.
    ok(
        project,
        &[
            "blob",
            "put",
            "media/pic.bin",
            "--file",
            "pic.bin",
            "--mime",
            "application/octet-stream",
        ],
    );
    assert!(
        project.join("media/staging").join(&workspace_id).is_dir(),
        "staging directory is missing"
    );
    let raw = project.join("media/media/raw").join(&workspace_id);
    let objects: Vec<String> = fs::read_dir(&raw)
        .unwrap_or_else(|err| panic!("read {}: {err}", raw.display()))
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(objects.len(), 1, "{objects:?}");
    assert_eq!(objects[0].len(), 64, "{objects:?}");
    assert_documented(LAYOUT, "media/raw/<workspace>/<blake3 hex>");
    assert_documented(LAYOUT, "staging/<workspace>/<ticket>");

    // Graft's remote keeps logs and segments, as described.
    ok(project, &["sync"]);
    assert!(project.join("remote/logs").is_dir(), "remote logs missing");
    assert!(
        project.join("remote/segments").is_dir(),
        "remote segments missing"
    );
    assert_documented(LAYOUT, "logs/<logid>/commits/<CBE64 LSN>");
    assert_documented(LAYOUT, "segments/<segment id>");

    // Commands the layout page names are the ones the CLI page documents.
    assert_documented(LAYOUT, "workspace clone");
    for command in [
        "workspace share",
        "workspace clone",
        "workspace sync",
        "workspace pull",
        "workspace blob sweep-staging",
    ] {
        assert_documented(USAGE, command);
    }
}
