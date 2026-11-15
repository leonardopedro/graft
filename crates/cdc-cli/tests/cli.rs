//! End-to-end tests of the `cdc` binary: each step is a separate process, so
//! this also covers reopening from the on-disk state file.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

/// Bytes no chunker can deduplicate, so an upload really has to move them.
fn incompressible(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// True once anything (including a half-written temp file) exists here.
fn has_entry(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cdc"))
        .arg("--data")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn cdc")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "cdc {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[test]
fn end_to_end_over_a_memory_remote() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    ok(dir, &["init", "--remote", "memory"]);

    let source = dir.join("source.bin");
    let bytes = data(300 * 1024, 7);
    fs::write(&source, &bytes).unwrap();

    let put = ok(dir, &["put", "photos/cat", source.to_str().unwrap()]);
    assert!(put.contains("307200 bytes"), "unexpected put output: {put}");

    let listing = ok(dir, &["ls", "photos/"]);
    assert!(listing.contains("photos/cat"), "listing: {listing}");
    assert_eq!(listing.lines().count(), 1);

    let out = dir.join("out.bin");
    ok(dir, &["get", "photos/cat", "-o", out.to_str().unwrap()]);
    assert_eq!(fs::read(&out).unwrap(), bytes);

    // A range comes back exactly.
    let range = run(
        dir,
        &[
            "get",
            "photos/cat",
            "--offset",
            "100000",
            "--length",
            "4096",
        ],
    );
    assert!(range.status.success());
    assert_eq!(range.stdout, bytes[100000..104096]);

    let stat = ok(dir, &["stat", "photos/cat"]);
    assert!(stat.contains("size: 307200"), "stat: {stat}");

    ok(dir, &["sync"]);
    assert!(ok(dir, &["rm", "photos/cat"]).contains("removed"));
    assert!(ok(dir, &["ls", ""]).trim().is_empty());

    let missing = run(dir, &["get", "photos/cat"]);
    assert!(!missing.status.success());
}

#[test]
fn state_and_key_survive_between_invocations() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    ok(dir, &["init"]);
    assert!(dir.join("volume.key").exists());
    assert!(dir.join("cdc-volume").exists());

    let source = dir.join("f");
    fs::write(&source, data(5000, 3)).unwrap();
    ok(dir, &["put", "f", source.to_str().unwrap()]);

    // A fresh process with only --data can read it back.
    let out = run(dir, &["get", "f"]);
    assert!(out.status.success());
    assert_eq!(out.stdout, data(5000, 3));

    // Wrong key fails instead of returning garbage.
    let other_key = dir.join("other.key");
    fs::write(&other_key, "00".repeat(32)).unwrap();
    let out = run(dir, &["--key", other_key.to_str().unwrap(), "get", "f"]);
    assert!(!out.status.success());
}

#[test]
fn init_refuses_to_overwrite_and_open_needs_init() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();

    let not_yet = run(dir, &["ls", ""]);
    assert!(!not_yet.status.success());
    assert!(
        String::from_utf8_lossy(&not_yet.stderr).contains("cdc init"),
        "stderr: {}",
        String::from_utf8_lossy(&not_yet.stderr)
    );

    ok(dir, &["init"]);
    let again = run(dir, &["init"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already holds a volume"));
}

#[test]
fn gc_swaps_the_state_file_and_keeps_live_files() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    ok(dir, &["init", "--remote", "memory"]);

    let kept = data(200 * 1024, 5);
    let dropped = data(200 * 1024, 9);
    let kept_src = dir.join("kept");
    let dropped_src = dir.join("dropped");
    fs::write(&kept_src, &kept).unwrap();
    fs::write(&dropped_src, &dropped).unwrap();
    ok(dir, &["put", "kept", kept_src.to_str().unwrap()]);
    ok(dir, &["put", "dropped", dropped_src.to_str().unwrap()]);
    ok(dir, &["rm", "dropped"]);

    let state_before = fs::read_to_string(dir.join("cdc-volume")).unwrap();
    let output = ok(dir, &["gc"]);
    assert!(output.contains("reclaimed"), "gc output: {output}");

    // The state file now points at the rewritten volume.
    let state_after = fs::read_to_string(dir.join("cdc-volume")).unwrap();
    assert_ne!(state_before, state_after);
    assert!(!dir.join("cdc-volume.tmp").exists());

    assert_eq!(ok(dir, &["ls", ""]).trim(), "204800 kept");
    let served = run(dir, &["get", "kept"]);
    assert_eq!(served.stdout, kept);

    let out = dir.join("out.bin");
    ok(dir, &["get", "kept", "-o", out.to_str().unwrap()]);
    assert_eq!(fs::read(&out).unwrap(), kept);

    // A second GC finds nothing dead to reclaim.
    let again = ok(dir, &["gc"]);
    assert!(again.contains("(0 reclaimed"), "second gc output: {again}");
}

#[test]
fn a_second_machine_attaches_and_reads_lazily() {
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("remote");
    let origin = root.path().join("origin");
    let edge = root.path().join("edge");
    let remote = format!("fs:{}", shared.display());

    ok(&origin, &["init", "--remote", &remote]);
    let bytes = data(400 * 1024, 17);
    let source = root.path().join("source.bin");
    fs::write(&source, &bytes).unwrap();
    ok(&origin, &["put", "a/blob", source.to_str().unwrap()]);
    ok(&origin, &["sync"]);

    let share = ok(&origin, &["share"]);
    let field = |name: &str| {
        share
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name} in share output: {share}"))
            .trim()
            .to_string()
    };
    let (vid, log) = (field("vid: "), field("log: "));
    let key = origin.join("volume.key");

    // A wrong key must be rejected, not silently opened as an empty volume.
    let wrong_key = root.path().join("wrong.key");
    fs::write(&wrong_key, format!("{}\n", "11".repeat(32))).unwrap();
    let rejected = run(
        &edge,
        &[
            "attach",
            "--vid",
            &vid,
            "--log",
            &log,
            "--remote",
            &remote,
            "--key",
            wrong_key.to_str().unwrap(),
        ],
    );
    assert!(!rejected.status.success(), "attach accepted the wrong key");
    assert!(
        !edge.join("cdc-volume").exists(),
        "failed attach wrote state"
    );

    ok(
        &edge,
        &[
            "attach",
            "--vid",
            &vid,
            "--log",
            &log,
            "--remote",
            &remote,
            "--key",
            key.to_str().unwrap(),
        ],
    );

    // Everything but the key file came over the remote.
    assert_eq!(ok(&edge, &["ls", ""]).trim(), "409600 a/blob");
    assert_eq!(run(&edge, &["get", "a/blob"]).stdout, bytes);

    // Writes from the origin become visible after a pull.
    let second = data(64 * 1024, 23);
    let second_src = root.path().join("second.bin");
    fs::write(&second_src, &second).unwrap();
    ok(&origin, &["put", "b/second", second_src.to_str().unwrap()]);
    ok(&origin, &["sync"]);

    assert_eq!(ok(&edge, &["ls", ""]).lines().count(), 1);
    ok(&edge, &["pull"]);
    assert_eq!(ok(&edge, &["ls", ""]).lines().count(), 2);
    assert_eq!(run(&edge, &["get", "b/second"]).stdout, second);

    // The edge wrote its own volume log; the origin's state is untouched.
    let origin_state = fs::read_to_string(origin.join("cdc-volume")).unwrap();
    assert!(origin_state.contains(&vid));
}

#[test]
fn a_failed_push_leaves_the_local_volume_intact() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    ok(dir, &["init", "--remote", "memory"]);

    let source = dir.join("f");
    let bytes = data(128 * 1024, 13);
    fs::write(&source, &bytes).unwrap();
    ok(dir, &["put", "f", source.to_str().unwrap()]);

    // A remote whose root cannot exist: its parent is a regular file.
    let blocker = dir.join("blocker");
    fs::write(&blocker, b"not a directory").unwrap();
    let broken = format!("fs:{}/sub", blocker.display());
    let failed = run(dir, &["--remote", &broken, "sync"]);
    assert!(
        !failed.status.success(),
        "sync to a broken remote succeeded"
    );

    // Nothing local was lost, and the real remote still works afterwards.
    assert_eq!(run(dir, &["get", "f"]).stdout, bytes);
    assert!(ok(dir, &["sync"]).contains("pushed"));
    assert_eq!(run(dir, &["get", "f"]).stdout, bytes);
}

#[test]
fn sync_pushes_to_an_fs_remote() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let remote = dir.join("remote");
    ok(
        dir,
        &["init", "--remote", &format!("fs:{}", remote.display())],
    );

    let source = dir.join("f");
    fs::write(&source, data(64 * 1024, 9)).unwrap();
    ok(dir, &["put", "f", source.to_str().unwrap()]);
    let objects = |root: &Path| -> usize {
        fs::read_dir(root).map_or(0, |entries| entries.flatten().count())
    };
    assert_eq!(objects(&remote), 0, "put must not push to the remote");

    assert!(ok(dir, &["sync"]).contains("pushed"));
    assert!(objects(&remote) > 0, "sync wrote no objects to the remote");
}

#[test]
fn a_push_killed_mid_upload_leaves_the_remote_without_a_partial_file() {
    let root = tempfile::tempdir().unwrap();
    let remote_dir = root.path().join("remote");
    let origin = root.path().join("origin");
    let remote = format!("fs:{}", remote_dir.display());

    ok(&origin, &["init", "--remote", &remote]);
    let small = data(8192, 3);
    let small_src = root.path().join("small.bin");
    fs::write(&small_src, &small).unwrap();
    ok(&origin, &["put", "small", small_src.to_str().unwrap()]);
    ok(&origin, &["sync"]);

    // Push a file big enough that the segment upload is still running when we
    // kill the process: the commit object can only be written after it.
    let big = incompressible(8 << 20);
    let big_src = root.path().join("big.bin");
    fs::write(&big_src, &big).unwrap();
    ok(&origin, &["put", "big", big_src.to_str().unwrap()]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_cdc"))
        .arg("--data")
        .arg(&origin)
        .arg("sync")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn cdc sync");
    let segments = remote_dir.join("segments");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !has_entry(&segments) {
        assert!(
            child.try_wait().unwrap().is_none(),
            "sync finished before the upload could be interrupted"
        );
        assert!(Instant::now() < deadline, "sync never started uploading");
        thread::sleep(Duration::from_millis(2));
    }
    child.kill().expect("kill cdc sync");
    let status = child.wait().unwrap();
    assert!(
        status.code().is_none(),
        "expected the push to die from a signal, got {status:?}"
    );

    // The local volume is untouched by the interrupted push.
    let local_copy = root.path().join("local.bin");
    ok(&origin, &["get", "big", "-o", local_copy.to_str().unwrap()]);
    assert_eq!(fs::read(&local_copy).unwrap(), big);

    // A second machine sees the completed push and nothing of the interrupted one.
    let share = ok(&origin, &["share"]);
    let field = |name: &str| {
        share
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name} in share output: {share}"))
            .trim()
            .to_string()
    };
    let (vid, log) = (field("vid: "), field("log: "));
    let key = origin.join("volume.key");
    let edge = root.path().join("edge");
    ok(
        &edge,
        &[
            "attach",
            "--vid",
            &vid,
            "--log",
            &log,
            "--remote",
            &remote,
            "--key",
            key.to_str().unwrap(),
        ],
    );
    assert_eq!(
        ok(&edge, &["ls", ""]).trim(),
        format!("{} small", small.len()),
        "the killed push left something visible on the remote"
    );

    // Retrying the push completes, and the edge then reads the whole file.
    assert!(ok(&origin, &["sync"]).contains("pushed"));
    ok(&edge, &["pull"]);
    let listing = ok(&edge, &["ls", ""]);
    assert_eq!(listing.lines().count(), 2, "listing: {listing}");

    let pulled = root.path().join("pulled.bin");
    ok(&edge, &["get", "big", "-o", pulled.to_str().unwrap()]);
    assert_eq!(fs::read(&pulled).unwrap(), big);
}

#[test]
fn attach_rejects_missing_inputs_and_bad_ids() {
    let root = tempfile::tempdir().unwrap();
    let remote_dir = root.path().join("remote");
    let origin = root.path().join("origin");
    let remote = format!("fs:{}", remote_dir.display());

    ok(&origin, &["init", "--remote", &remote]);
    let source = root.path().join("f");
    fs::write(&source, data(4096, 11)).unwrap();
    ok(&origin, &["put", "f", source.to_str().unwrap()]);
    ok(&origin, &["sync"]);

    let share = ok(&origin, &["share"]);
    let field = |name: &str| {
        share
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name} in share output: {share}"))
            .trim()
            .to_string()
    };
    let (vid, log) = (field("vid: "), field("log: "));
    let key = origin.join("volume.key");

    // A volume that was created but never pushed: valid ids, empty remote log.
    let unpushed = root.path().join("unpushed");
    ok(&unpushed, &["init", "--remote", &remote]);
    let unpushed_share = ok(&unpushed, &["share"]);
    let unpushed_vid = unpushed_share
        .lines()
        .find_map(|line| line.strip_prefix("vid: "))
        .unwrap()
        .trim()
        .to_string();
    let unpushed_log = unpushed_share
        .lines()
        .find_map(|line| line.strip_prefix("log: "))
        .unwrap()
        .trim()
        .to_string();

    let missing_key = root.path().join("missing.key");
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec![
                "attach",
                "--vid",
                &vid,
                "--log",
                &log,
                "--key",
                key.to_str().unwrap(),
            ],
            "attach needs --remote",
        ),
        (
            vec!["attach", "--vid", &vid, "--log", &log, "--remote", &remote],
            "attach needs --key",
        ),
        (
            vec![
                "attach",
                "--vid",
                &vid,
                "--log",
                &log,
                "--remote",
                &remote,
                "--key",
                missing_key.to_str().unwrap(),
            ],
            "does not exist",
        ),
        (
            vec![
                "attach",
                "--vid",
                "not-a-volume",
                "--log",
                &log,
                "--remote",
                &remote,
                "--key",
                key.to_str().unwrap(),
            ],
            "bad volume id",
        ),
        (
            vec![
                "attach",
                "--vid",
                &vid,
                "--log",
                "not-a-log",
                "--remote",
                &remote,
                "--key",
                key.to_str().unwrap(),
            ],
            "bad log id",
        ),
        (
            vec![
                "attach",
                "--vid",
                &unpushed_vid,
                "--log",
                &unpushed_log,
                "--remote",
                &remote,
                "--key",
                key.to_str().unwrap(),
            ],
            "outside the volume",
        ),
    ];

    for (args, expected) in cases {
        let edge = root
            .path()
            .join(format!("edge-{}", expected.replace(' ', "-")));
        let mut with_data = vec!["--data", edge.to_str().unwrap()];
        with_data.extend(args);
        let out = Command::new(env!("CARGO_BIN_EXE_cdc"))
            .args(&with_data)
            .output()
            .expect("spawn cdc attach");
        assert!(
            !out.status.success(),
            "attach accepted {expected:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(expected),
            "expected {expected:?} in stderr, got: {stderr}"
        );
        assert!(
            !edge.join("cdc-volume").exists(),
            "a failed attach left state behind for {expected:?}"
        );
    }

    // A real attach works once, and a second one will not clobber it.
    let edge = root.path().join("edge");
    ok(
        &edge,
        &[
            "attach",
            "--vid",
            &vid,
            "--log",
            &log,
            "--remote",
            &remote,
            "--key",
            key.to_str().unwrap(),
        ],
    );
    let before = fs::read_to_string(edge.join("cdc-volume")).unwrap();
    let again = run(
        &edge,
        &[
            "attach",
            "--vid",
            &vid,
            "--log",
            &log,
            "--remote",
            &remote,
            "--key",
            key.to_str().unwrap(),
        ],
    );
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already holds a volume"));
    assert_eq!(fs::read_to_string(edge.join("cdc-volume")).unwrap(), before);
}

#[test]
fn gc_on_an_attached_edge_keeps_live_files_and_leaves_the_origin_alone() {
    let root = tempfile::tempdir().unwrap();
    let remote_dir = root.path().join("remote");
    let origin = root.path().join("origin");
    let edge = root.path().join("edge");
    let remote = format!("fs:{}", remote_dir.display());

    ok(&origin, &["init", "--remote", &remote]);
    let kept = data(40 * 1024, 5);
    let dropped = data(30 * 1024, 7);
    let kept_src = root.path().join("kept.bin");
    let dropped_src = root.path().join("dropped.bin");
    fs::write(&kept_src, &kept).unwrap();
    fs::write(&dropped_src, &dropped).unwrap();
    ok(&origin, &["put", "kept", kept_src.to_str().unwrap()]);
    ok(&origin, &["put", "dropped", dropped_src.to_str().unwrap()]);
    ok(&origin, &["sync"]);

    let share = ok(&origin, &["share"]);
    let field = |name: &str| {
        share
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name} in share output: {share}"))
            .trim()
            .to_string()
    };
    let (vid, log) = (field("vid: "), field("log: "));
    let key = origin.join("volume.key");
    ok(
        &edge,
        &[
            "attach",
            "--vid",
            &vid,
            "--log",
            &log,
            "--remote",
            &remote,
            "--key",
            key.to_str().unwrap(),
        ],
    );
    assert_eq!(ok(&edge, &["ls", ""]).lines().count(), 2);

    // Drop one file on the edge and reclaim it there. GC rewrites into a fresh
    // volume, so the edge ends up on a new volume id and needs no remote writes.
    ok(&edge, &["rm", "dropped"]);
    let before = fs::read_to_string(edge.join("cdc-volume")).unwrap();
    let output = ok(&edge, &["gc"]);
    assert!(output.contains("reclaimed"), "gc output: {output}");
    assert_ne!(fs::read_to_string(edge.join("cdc-volume")).unwrap(), before);

    assert_eq!(
        ok(&edge, &["ls", ""]).trim(),
        format!("{} kept", kept.len())
    );
    assert_eq!(run(&edge, &["get", "kept"]).stdout, kept);
    assert!(
        !run(&edge, &["get", "dropped"]).status.success(),
        "gc revived a removed file"
    );

    // The origin was never involved: both its files are still there.
    assert_eq!(ok(&origin, &["ls", ""]).lines().count(), 2);
    assert_eq!(run(&origin, &["get", "dropped"]).stdout, dropped);

    // The rewritten volume can still be shared, and a third machine reads it.
    let rewritten = ok(&edge, &["share"]);
    let new_vid = rewritten
        .lines()
        .find_map(|line| line.strip_prefix("vid: "))
        .unwrap()
        .trim()
        .to_string();
    assert_ne!(new_vid, vid, "gc must move the volume to a new id");
    assert!(ok(&edge, &["sync"]).contains("pushed"));

    let third = root.path().join("third");
    let third_share = ok(&edge, &["share"]);
    let third_log = third_share
        .lines()
        .find_map(|line| line.strip_prefix("log: "))
        .unwrap()
        .trim()
        .to_string();
    ok(
        &third,
        &[
            "attach",
            "--vid",
            &new_vid,
            "--log",
            &third_log,
            "--remote",
            &remote,
            "--key",
            key.to_str().unwrap(),
        ],
    );
    assert_eq!(
        ok(&third, &["ls", ""]).trim(),
        format!("{} kept", kept.len())
    );
    assert_eq!(run(&third, &["get", "kept"]).stdout, kept);
}

#[test]
fn a_failed_pull_keeps_the_local_copy_readable() {
    let root = tempfile::tempdir().unwrap();
    let origin = root.path().join("origin");
    let remote_dir = root.path().join("remote");
    let remote = format!("fs:{}", remote_dir.display());

    ok(&origin, &["init", "--remote", &remote]);
    let bytes = data(32 * 1024, 13);
    let source = root.path().join("f");
    fs::write(&source, &bytes).unwrap();
    ok(&origin, &["put", "f", source.to_str().unwrap()]);
    ok(&origin, &["sync"]); // A remote that opens fine but whose log path is a file, so every commit
    // read fails — the transient-looking error a flaky bucket gives.
    let broken_dir = root.path().join("broken");
    fs::create_dir_all(&broken_dir).unwrap();
    fs::write(broken_dir.join("logs"), b"not a directory").unwrap();
    let broken = format!("fs:{}", broken_dir.display());
    let failed = run(&origin, &["--remote", &broken, "pull"]);
    assert!(
        !failed.status.success(),
        "pull from a broken remote succeeded"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("pull failed after 4 attempts"),
        "a broken remote must exhaust its retry budget, got: {stderr}"
    );
    assert!(
        stderr.matches("retrying in").count() == 3,
        "expected three retries before giving up, got: {stderr}"
    );

    // attach has no local copy to fall back on, so it goes through the same
    // budget and still refuses to write state when the remote stays unreachable.
    let share = ok(&origin, &["share"]);
    let field = |name: &str| {
        share
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
            .trim()
            .to_string()
    };
    let edge = root.path().join("edge");
    let attach_failed = run(
        &edge,
        &[
            "attach",
            "--vid",
            &field("vid: "),
            "--log",
            &field("log: "),
            "--remote",
            &broken,
            "--key",
            origin.join("volume.key").to_str().unwrap(),
        ],
    );
    assert!(!attach_failed.status.success());
    assert!(
        String::from_utf8_lossy(&attach_failed.stderr).contains("pull failed after 4 attempts"),
        "attach did not retry the pull: {}",
        String::from_utf8_lossy(&attach_failed.stderr)
    );
    assert!(!edge.join("cdc-volume").exists());

    // The local volume is untouched and the real remote still pulls.
    assert_eq!(run(&origin, &["get", "f"]).stdout, bytes);
    assert!(ok(&origin, &["pull"]).contains("pulled"));
    assert_eq!(run(&origin, &["get", "f"]).stdout, bytes);
}
