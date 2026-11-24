//! Binary update repositories: `FastCDC` chunk reuse, verification and install.

use std::{fs, path::Path};

use graft_workspace::{
    MediaConfig, UpdateErr,
    update::{self, UpdateRepo},
};

fn repo(dir: &Path) -> UpdateRepo {
    UpdateRepo::from_config(
        &MediaConfig::Fs {
            root: dir.join("repo").display().to_string(),
        },
        "updates",
        "demo",
    )
    .unwrap()
}

/// Deterministic, varied bytes so `FastCDC` sees realistic boundaries.
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

fn write_tree(root: &Path, big: &[u8], message: &str, extra: Option<&[u8]>) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/app"), big).unwrap();
    fs::write(root.join("version.txt"), message.as_bytes()).unwrap();
    if let Some(extra) = extra {
        fs::write(root.join("notes.txt"), extra).unwrap();
    }
}

#[test]
fn second_release_downloads_only_missing_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let big = payload(600_000, 7);

    let v1 = dir.join("v1");
    write_tree(&v1, &big, "version 1\n", None);
    let publish1 = repo.publish("1.0.0", &v1, None).unwrap();
    assert_eq!(publish1.files, 2);
    assert!(publish1.bytes >= big.len() as u64);
    assert!(publish1.chunks > 1);

    let install = dir.join("install");
    let state = dir.join("installed.json");
    let first = repo
        .apply("1.0.0", &install, &state, None, false, None, None)
        .unwrap();
    assert_eq!(first.files_written, 2);
    assert_eq!(first.bytes_downloaded, publish1.bytes);
    assert_eq!(fs::read(install.join("bin/app")).unwrap(), big);
    assert_eq!(
        fs::read(install.join("version.txt")).unwrap(),
        b"version 1\n"
    );

    let installed = update::read_state(&state, "demo").unwrap();
    assert_eq!(installed.release, "1.0.0");

    // Version 2 keeps the large binary byte-identical, changes one small file
    // and adds another. Only the new small chunks should cross the network.
    let v2 = dir.join("v2");
    write_tree(&v2, &big, "version 2\n", Some(b"release notes\n"));
    let publish2 = repo.publish("2.0.0", &v2, None).unwrap();
    assert!(publish2.bytes > 600_000);
    assert!(
        publish2.reused_chunks > 0,
        "unchanged chunks are already in the repository"
    );
    assert!(
        publish2.reused_chunks > 0,
        "unchanged chunks are already in the repository"
    );

    let plan = repo.plan("2.0.0", Some(&installed), None).unwrap();
    assert_eq!(plan.files_unchanged, 1);
    assert_eq!(plan.files_changed, 1);
    assert_eq!(plan.files_new, 1);
    assert_eq!(plan.files_removed, 0);
    assert!(plan.chunks_missing > 0);
    assert!(
        plan.bytes_missing < 32 * 1024,
        "small edits should need few chunks: {plan:?}"
    );

    let second = repo
        .apply(
            "2.0.0",
            &install,
            &state,
            Some(&installed),
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(second.files_unchanged, 1);
    assert_eq!(second.files_written, 2);
    assert!(
        second.bytes_downloaded < 32 * 1024,
        "unchanged binary must not be fetched again: {second:?}"
    );
    assert_eq!(fs::read(install.join("bin/app")).unwrap(), big);
    assert_eq!(
        fs::read(install.join("version.txt")).unwrap(),
        b"version 2\n"
    );
    assert_eq!(
        fs::read(install.join("notes.txt")).unwrap(),
        b"release notes\n"
    );
    assert_eq!(update::read_state(&state, "demo").unwrap().release, "2.0.0");
}

#[test]
fn compressed_chunks_install_and_coexist_with_raw_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let root = dir.join("release");
    fs::create_dir_all(&root).unwrap();
    let mut text = Vec::new();
    while text.len() < 400_000 {
        text.extend_from_slice(b"the same compressible line of release data\n");
    }
    fs::write(root.join("notes.txt"), &text).unwrap();

    let compressed = repo
        .publish_with_compression("1.0.0", &root, None, Some(9))
        .unwrap();
    assert!(compressed.compressed_chunks > 0, "{compressed:?}");
    assert_eq!(compressed.compression_level, Some(9));
    let err = repo
        .publish_with_compression("bad", &root, None, Some(23))
        .unwrap_err();
    assert!(
        matches!(err, UpdateErr::InvalidCompressionLevel(23)),
        "{err:?}"
    );
    let manifest = repo.manifest("1.0.0", None).unwrap();
    let encoded = manifest.files[0]
        .chunks
        .iter()
        .find(|chunk| chunk.encoding.is_some())
        .expect("at least one chunk is compressed");
    let hex = encoded.hash.strip_prefix("blake3:").unwrap();
    let object = dir
        .join("repo/updates/demo/chunks")
        .join(&hex[..2])
        .join(format!("{hex}.zst"));
    assert!(object.is_file());
    assert!(fs::metadata(&object).unwrap().len() < u64::from(encoded.size));

    let install = dir.join("compressed-install");
    let state = dir.join("compressed-state.json");
    repo.apply("1.0.0", &install, &state, None, false, None, None)
        .unwrap();
    assert_eq!(fs::read(install.join("notes.txt")).unwrap(), text);

    // The same tree published without compression needs separate raw objects;
    // the plaintext hash is shared, but the stored encodings are not.
    let raw = repo.publish("2.0.0", &root, None).unwrap();
    assert_eq!(raw.compressed_chunks, 0);
    assert!(raw.uploaded_chunks > 0, "{raw:?}");
    let raw_install = dir.join("raw-install");
    let raw_state = dir.join("raw-state.json");
    repo.apply("2.0.0", &raw_install, &raw_state, None, false, None, None)
        .unwrap();
    assert_eq!(fs::read(raw_install.join("notes.txt")).unwrap(), text);

    // Removing the compressed ref lets GC reclaim only its `.zst` objects; the
    // raw release keeps the same plaintext chunks alive.
    fs::remove_file(dir.join("repo/updates/demo/refs/1.0.0")).unwrap();
    let raw_manifest = repo.manifest("2.0.0", None).unwrap();
    let raw_chunk = &raw_manifest.files[0].chunks[0];
    let raw_hex = raw_chunk.hash.strip_prefix("blake3:").unwrap();
    let raw_object = dir
        .join("repo/updates/demo/chunks")
        .join(&raw_hex[..2])
        .join(raw_hex);
    repo.gc(0, false).unwrap();
    assert!(!object.exists());
    assert!(raw_object.is_file());
}

#[test]
fn interrupted_apply_resumes_staged_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let root = dir.join("release");
    let bytes = payload(500_000, 41);
    write_tree(&root, &bytes, "one\n", None);
    let published = repo.publish("1.0.0", &root, None).unwrap();

    let manifest = repo.manifest("1.0.0", None).unwrap();
    let chunk = manifest
        .files
        .iter()
        .find(|file| file.path == "bin/app")
        .expect("binary")
        .chunks
        .last()
        .expect("binary has chunks");
    let hex = chunk.hash.strip_prefix("blake3:").unwrap();
    let object = dir
        .join("repo/updates/demo/chunks")
        .join(&hex[..2])
        .join(hex);
    let saved = fs::read(&object).unwrap();
    fs::remove_file(&object).unwrap();

    let install = dir.join("install");
    let state = dir.join("state.json");
    let err = repo
        .apply("1.0.0", &install, &state, None, false, None, None)
        .unwrap_err();
    assert!(matches!(err, UpdateErr::MissingChunk { .. }), "{err:?}");
    assert!(!state.exists());
    let staged = dir.join(".install.graft-update/tree/bin/app");
    assert!(fs::metadata(&staged).unwrap().len() > 0);

    fs::write(&object, saved).unwrap();
    let report = repo
        .apply("1.0.0", &install, &state, None, false, None, None)
        .unwrap();
    assert!(report.chunks_resumed > 0, "{report:?}");
    assert!(report.bytes_resumed > 0, "{report:?}");
    assert!(
        report.bytes_downloaded < published.bytes,
        "the retry should not fetch the resumed prefix: {report:?}"
    );
    assert_eq!(fs::read(install.join("bin/app")).unwrap(), bytes);
    assert_eq!(update::read_state(&state, "demo").unwrap().release, "1.0.0");
    assert!(
        !dir.join(".install.graft-update").exists(),
        "successful apply removes its staging tree"
    );
}

#[test]
fn corrupt_chunks_are_rejected_before_install() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let root = dir.join("release");
    let bytes = payload(300_000, 99);
    write_tree(&root, &bytes, "one\n", None);
    repo.publish("1.0.0", &root, None).unwrap();

    let manifest = repo.manifest("1.0.0", None).unwrap();
    let chunk = manifest
        .files
        .iter()
        .flat_map(|file| &file.chunks)
        .next()
        .expect("release has chunks");
    let hex = chunk.hash.strip_prefix("blake3:").unwrap();
    let object = dir
        .join("repo/updates/demo/chunks")
        .join(&hex[..2])
        .join(hex);
    fs::write(&object, b"not the chunk").unwrap();

    let install = dir.join("install");
    let state = dir.join("state.json");
    let err = repo
        .apply("1.0.0", &install, &state, None, false, None, None)
        .unwrap_err();
    assert!(
        matches!(
            err,
            UpdateErr::ChunkSize { .. } | UpdateErr::CorruptChunk { .. }
        ),
        "{err:?}"
    );
    assert!(!state.exists(), "a failed apply must not write state");
    assert!(!install.join("bin/app").exists());
}

#[test]
fn delete_removes_files_only_after_a_verified_apply() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let bytes = payload(200_000, 5);
    let v1 = dir.join("v1");
    write_tree(&v1, &bytes, "one\n", Some(b"old notes\n"));
    repo.publish("1.0.0", &v1, None).unwrap();

    let install = dir.join("install");
    let state = dir.join("state.json");
    repo.apply("1.0.0", &install, &state, None, false, None, None)
        .unwrap();
    let old = update::read_state(&state, "demo").unwrap();
    assert!(install.join("notes.txt").exists());

    let v2 = dir.join("v2");
    write_tree(&v2, &bytes, "two\n", None);
    repo.publish("2.0.0", &v2, None).unwrap();
    let report = repo
        .apply("2.0.0", &install, &state, Some(&old), true, None, None)
        .unwrap();
    assert_eq!(report.files_deleted, 1);
    assert!(!install.join("notes.txt").exists());
    assert_eq!(fs::read(install.join("version.txt")).unwrap(), b"two\n");
}

#[test]
fn signed_manifests_require_the_matching_key() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let private = dir.join("private.key");
    let public = dir.join("public.key");
    update::generate_keypair(&private, &public).unwrap();
    let signing = graft_workspace::ManifestSigningKey::load(&private).unwrap();
    let verifying = graft_workspace::ManifestPublicKey::load(&public).unwrap();

    let root = dir.join("release");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("version.txt"), b"signed\n").unwrap();
    repo.publish("1.0.0", &root, Some(&signing)).unwrap();

    // Unsigned access still works for local inspection.
    repo.manifest("1.0.0", None).unwrap();
    // The matching key verifies.
    repo.manifest("1.0.0", Some(&verifying)).unwrap();

    let other_private = dir.join("other-private.key");
    let other_public = dir.join("other-public.key");
    update::generate_keypair(&other_private, &other_public).unwrap();
    let other = graft_workspace::ManifestPublicKey::load(&other_public).unwrap();
    let err = repo.manifest("1.0.0", Some(&other)).unwrap_err();
    assert!(matches!(err, UpdateErr::BadSignature { .. }), "{err:?}");

    let unsigned = dir.join("unsigned");
    fs::create_dir_all(&unsigned).unwrap();
    fs::write(unsigned.join("version.txt"), b"unsigned\n").unwrap();
    repo.publish("2.0.0", &unsigned, None).unwrap();
    let err = repo.manifest("2.0.0", Some(&verifying)).unwrap_err();
    assert!(matches!(err, UpdateErr::UnsignedRelease { .. }), "{err:?}");
}

#[test]
fn gc_removes_only_unreachable_objects() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let root = dir.join("release");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("version.txt"), b"one\n").unwrap();
    repo.publish("1.0.0", &root, None).unwrap();
    fs::write(root.join("version.txt"), b"two\n").unwrap();
    repo.publish("2.0.0", &root, None).unwrap();

    let orphan = format!("blake3:{}", blake3::hash(b"orphan").to_hex());
    let orphan_hex = orphan.strip_prefix("blake3:").unwrap();
    let chunk = dir
        .join("repo/updates/demo/chunks")
        .join(&orphan_hex[..2])
        .join(orphan_hex);
    fs::create_dir_all(chunk.parent().unwrap()).unwrap();
    fs::write(&chunk, b"orphan").unwrap();
    let manifest = dir
        .join("repo/updates/demo/manifests")
        .join(format!("{orphan_hex}.json"));
    fs::write(&manifest, b"orphan manifest").unwrap();

    let dry = repo.gc(0, true).unwrap();
    assert_eq!(dry.manifests_removed, 1);
    assert_eq!(dry.chunks_removed, 1);
    assert!(dry.dry_run);
    assert!(chunk.exists() && manifest.exists());

    let report = repo.gc(0, false).unwrap();
    assert_eq!(report.manifests_removed, 1);
    assert_eq!(report.chunks_removed, 1);
    assert!(!chunk.exists());
    assert!(!manifest.exists());

    // Live releases still resolve and install after collection.
    repo.manifest("1.0.0", None).unwrap();
    repo.manifest("2.0.0", None).unwrap();
}

#[test]
fn catalog_pins_survive_ref_pruning_during_gc() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let root = dir.join("release");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("version.txt"), b"pinned\n").unwrap();
    let pinned = repo.publish("1.0.0", &root, None).unwrap();
    let pinned_manifest = repo.manifest("1.0.0", None).unwrap();
    fs::write(root.join("version.txt"), b"newer\n").unwrap();
    repo.publish("2.0.0", &root, None).unwrap();

    repo.prune_refs(1, 0, false).unwrap();
    assert!(!dir.join("repo/updates/demo/refs/1.0.0").exists());

    let manifest_hex = pinned.manifest.strip_prefix("blake3:").unwrap();
    let manifest = dir
        .join("repo/updates/demo/manifests")
        .join(format!("{manifest_hex}.json"));
    let chunk = pinned_manifest
        .files
        .into_iter()
        .flat_map(|file| file.chunks)
        .next()
        .unwrap();
    let chunk_hex = chunk.hash.strip_prefix("blake3:").unwrap();
    let chunk_path = dir
        .join("repo/updates/demo/chunks")
        .join(&chunk_hex[..2])
        .join(chunk_hex);

    let report = repo
        .gc_with_pins(0, false, std::slice::from_ref(&pinned.manifest))
        .unwrap();
    assert_eq!(report.manifests_removed, 0);
    assert!(manifest.exists());
    assert!(chunk_path.exists());

    let report = repo.gc(0, false).unwrap();
    assert_eq!(report.manifests_removed, 1);
    assert!(!manifest.exists());
}

#[test]
fn gc_grace_period_keeps_fresh_unreachable_objects() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let root = dir.join("release");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("version.txt"), b"one\n").unwrap();
    repo.publish("1.0.0", &root, None).unwrap();

    let orphan = format!("blake3:{}", blake3::hash(b"fresh orphan").to_hex());
    let orphan_hex = orphan.strip_prefix("blake3:").unwrap();
    let chunk = dir
        .join("repo/updates/demo/chunks")
        .join(&orphan_hex[..2])
        .join(orphan_hex);
    fs::create_dir_all(chunk.parent().unwrap()).unwrap();
    fs::write(&chunk, b"fresh orphan").unwrap();
    let manifest = dir
        .join("repo/updates/demo/manifests")
        .join(format!("{orphan_hex}.json"));
    fs::write(&manifest, b"fresh orphan manifest").unwrap();

    let kept = repo.gc(3_600, false).unwrap();
    assert_eq!(kept.manifests_removed, 0);
    assert_eq!(kept.chunks_removed, 0);
    assert!(chunk.exists() && manifest.exists());

    let removed = repo.gc(0, false).unwrap();
    assert_eq!(removed.manifests_removed, 1);
    assert_eq!(removed.chunks_removed, 1);
    assert!(!chunk.exists() && !manifest.exists());
}

#[test]
fn tampered_signature_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let private = dir.join("private.key");
    let public = dir.join("public.key");
    update::generate_keypair(&private, &public).unwrap();
    let signing = graft_workspace::ManifestSigningKey::load(&private).unwrap();
    let verifying = graft_workspace::ManifestPublicKey::load(&public).unwrap();

    let root = dir.join("release");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("version.txt"), b"signed\n").unwrap();
    repo.publish("1.0.0", &root, Some(&signing)).unwrap();

    let ref_path = dir.join("repo/updates/demo/refs/1.0.0");
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(&ref_path).unwrap()).unwrap();
    let signature = envelope["signatures"][0]["signature"]
        .as_str()
        .unwrap()
        .to_string();
    let mut tampered = signature.into_bytes();
    tampered[0] = if tampered[0] == b'0' { b'1' } else { b'0' };
    envelope["signatures"][0]["signature"] =
        serde_json::Value::String(String::from_utf8(tampered).unwrap());
    fs::write(&ref_path, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();

    let err = repo.manifest("1.0.0", Some(&verifying)).unwrap_err();
    assert!(matches!(err, UpdateErr::BadSignature { .. }), "{err:?}");
    // Local inspection without a trust anchor still works.
    repo.manifest("1.0.0", None).unwrap();
}

#[test]
fn prunes_old_refs_and_then_collects_their_objects() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    for release in ["1.0.0", "2.0.0", "3.0.0"] {
        let root = dir.join(release);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("version.txt"), release).unwrap();
        repo.publish(release, &root, None).unwrap();
    }

    let dry = repo.prune_refs(2, 0, true).unwrap();
    assert_eq!(dry.removed, vec!["1.0.0"]);
    assert_eq!(repo.releases().unwrap().len(), 3);

    let report = repo.prune_refs(2, 0, false).unwrap();
    assert_eq!(report.removed, vec!["1.0.0"]);
    assert_eq!(report.kept, vec!["2.0.0", "3.0.0"]);
    assert!(matches!(
        repo.manifest("1.0.0", None).unwrap_err(),
        UpdateErr::Io(_)
    ));
    let gc = repo.gc(0, false).unwrap();
    assert!(gc.manifests_removed >= 1);
    assert!(gc.chunks_removed >= 1);
    repo.manifest("2.0.0", None).unwrap();
    repo.manifest("3.0.0", None).unwrap();
}

#[test]
fn published_releases_cannot_be_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let first = dir.join("first");
    fs::create_dir_all(&first).unwrap();
    fs::write(first.join("version.txt"), b"one\n").unwrap();
    repo.publish("1.0.0", &first, None).unwrap();

    let second = dir.join("second");
    fs::create_dir_all(&second).unwrap();
    fs::write(second.join("version.txt"), b"two\n").unwrap();
    let err = repo.publish("1.0.0", &second, None).unwrap_err();
    assert!(matches!(err, UpdateErr::ReleaseExists { .. }), "{err:?}");
    let manifest = repo.manifest("1.0.0", None).unwrap();
    assert_eq!(manifest.files[0].size, 4);
}

#[cfg(unix)]
#[test]
fn apply_rejects_a_symlinked_parent_directory() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let root = dir.join("release");
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/app"), payload(20_000, 11)).unwrap();
    repo.publish("1.0.0", &root, None).unwrap();

    let install = dir.join("install");
    let outside = dir.join("outside");
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, install.join("bin")).unwrap();

    let err = repo
        .apply(
            "1.0.0",
            &install,
            &dir.join("state.json"),
            None,
            false,
            None,
            None,
        )
        .unwrap_err();
    assert!(matches!(err, UpdateErr::UnsafePath(_)), "{err:?}");
    assert!(!outside.join("app").exists());
}

#[test]
fn build_state_matches_a_release_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let root = dir.join("release");
    let bytes = payload(70_000, 3);
    write_tree(&root, &bytes, "same\n", None);
    repo.publish("1.0.0", &root, None).unwrap();

    let state = update::build_state(&root, "demo", "1.0.0").unwrap();
    let manifest = repo.manifest("1.0.0", None).unwrap();
    assert_eq!(state.files, manifest.files);
    let plan = repo.plan("1.0.0", Some(&state), None).unwrap();
    assert_eq!(plan.files_changed, 0);
    assert_eq!(plan.files_new, 0);
    assert_eq!(plan.chunks_missing, 0);
}
