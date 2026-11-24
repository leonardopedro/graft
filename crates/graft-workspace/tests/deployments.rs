//! Atomic deployment directories, symlink switching and rollback.

#![cfg(unix)]

use std::{fs, path::Path};

use graft_workspace::{
    MediaConfig, UpdateRepo,
    deployment::{self, DeployErr},
    update,
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

fn write_release(root: &Path, big: &[u8], version: &str) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/app"), big).unwrap();
    fs::write(root.join("version.txt"), version).unwrap();
}

#[test]
fn deploy_switches_atomically_and_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let big = payload(500_000, 21);

    let v1 = dir.join("v1");
    write_release(&v1, &big, "one\n");
    repo.publish("1.0.0", &v1, None).unwrap();

    let v2 = dir.join("v2");
    write_release(&v2, &big, "two\n");
    repo.publish("2.0.0", &v2, None).unwrap();

    let root = dir.join("deploy");
    let first = deployment::deploy(&repo, "1.0.0", &root, None).unwrap();
    assert!(!first.reused);
    assert_eq!(
        fs::read(root.join("current/version.txt")).unwrap(),
        b"one\n"
    );
    assert_eq!(fs::read(root.join("current/bin/app")).unwrap(), big);
    assert!(first.applied.unwrap().bytes_downloaded >= big.len() as u64);

    let second = deployment::deploy(&repo, "2.0.0", &root, None).unwrap();
    assert!(!second.reused);
    assert_eq!(second.previous.as_deref(), Some(first.deployment.as_str()));
    assert_eq!(
        fs::read(root.join("current/version.txt")).unwrap(),
        b"two\n"
    );
    // The unchanged 500 KB binary is reused from the previous deployment.
    assert!(second.applied.unwrap().bytes_downloaded < 32 * 1024);

    let listing = deployment::list(&root).unwrap();
    assert_eq!(listing.len(), 2);
    assert_eq!(
        listing
            .iter()
            .filter(|deployment| deployment.current)
            .count(),
        1
    );

    // Repeating the same deployment is idempotent.
    let again = deployment::deploy(&repo, "2.0.0", &root, None).unwrap();
    assert!(again.reused);

    let rollback = deployment::rollback(&root).unwrap();
    assert_eq!(rollback.to, first.deployment);
    assert_eq!(
        fs::read(root.join("current/version.txt")).unwrap(),
        b"one\n"
    );

    // Rolling back again returns to the release we just left.
    let forward = deployment::rollback(&root).unwrap();
    assert_eq!(forward.to, second.deployment);
    assert_eq!(
        fs::read(root.join("current/version.txt")).unwrap(),
        b"two\n"
    );
}

#[test]
fn deploy_requires_a_valid_signature_when_a_key_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let private = dir.join("private.key");
    let public = dir.join("public.key");
    update::generate_keypair(&private, &public).unwrap();
    let signing = graft_workspace::ManifestSigningKey::load(&private).unwrap();
    let verifying = graft_workspace::ManifestPublicKey::load(&public).unwrap();

    let release = dir.join("release");
    write_release(&release, &payload(90_000, 3), "signed\n");
    repo.publish("1.0.0", &release, Some(&signing)).unwrap();

    let root = dir.join("deploy");
    deployment::deploy(&repo, "1.0.0", &root, Some(&verifying)).unwrap();
    assert_eq!(
        fs::read(root.join("current/version.txt")).unwrap(),
        b"signed\n"
    );

    let other_private = dir.join("other-private.key");
    let other_public = dir.join("other-public.key");
    update::generate_keypair(&other_private, &other_public).unwrap();
    let other = graft_workspace::ManifestPublicKey::load(&other_public).unwrap();
    let err = deployment::deploy(&repo, "1.0.0", &root, Some(&other)).unwrap_err();
    assert!(matches!(err, DeployErr::Update(_)), "{err:?}");
}

#[test]
fn prunes_old_deployments_without_breaking_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let big = payload(120_000, 9);
    let root = dir.join("deploy");
    let mut ids = Vec::new();
    for (index, release) in ["1.0.0", "2.0.0", "3.0.0", "4.0.0"].iter().enumerate() {
        let release_root = dir.join(release);
        write_release(&release_root, &big, &format!("{index}\n"));
        repo.publish(release, &release_root, None).unwrap();
        ids.push(
            deployment::deploy(&repo, release, &root, None)
                .unwrap()
                .deployment,
        );
    }
    let dry = deployment::prune(&root, 3, true).unwrap();
    assert_eq!(dry.removed.len(), 1);
    assert!(dry.dry_run);
    assert_eq!(deployment::list(&root).unwrap().len(), 4);

    let report = deployment::prune(&root, 3, false).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.kept.len(), 3);
    assert!(report.bytes_removed > 0);
    let removed = report.removed[0].clone();
    let listing = deployment::list(&root).unwrap();
    assert_eq!(listing.len(), 3);
    assert!(listing.iter().any(|deployment| deployment.current));
    assert!(listing.iter().any(|deployment| deployment.id == ids[3]));

    let rollback = deployment::rollback(&root).unwrap();
    assert_eq!(rollback.to, ids[2]);
    assert_eq!(fs::read(root.join("current/version.txt")).unwrap(), b"2\n");

    let previous = root.join("states").join(format!("{removed}.json"));
    assert!(!previous.exists());
}

#[test]
fn rollback_without_a_previous_deployment_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let repo = repo(dir);
    let release = dir.join("release");
    write_release(&release, &payload(20_000, 4), "one\n");
    repo.publish("1.0.0", &release, None).unwrap();

    let root = dir.join("deploy");
    deployment::deploy(&repo, "1.0.0", &root, None).unwrap();
    let err = deployment::rollback(&root).unwrap_err();
    assert!(matches!(err, DeployErr::NoPrevious), "{err:?}");
}
