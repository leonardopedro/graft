//! Validate generated service files with the platform's own parser.
//!
//! Linux uses `systemd-analyze verify`; macOS uses `plutil -lint`. CI sets
//! `GRAFT_REQUIRE_SERVICE_PARSER=1` so a missing parser fails instead of being
//! skipped.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use graft_workspace::service::{ServiceKind, ServiceOptions, generate};

fn require_parser() -> bool {
    env::var_os("GRAFT_REQUIRE_SERVICE_PARSER").is_some()
}

fn run_parser(command: &str, args: &[&Path]) {
    if Command::new(command).arg("--version").output().is_err() {
        if require_parser() {
            panic!("{command} is required to validate generated service files");
        }
        eprintln!("skipping service-file validation: {command} is not installed");
        return;
    }
    let output = Command::new(command)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("run {command}: {err}"));
    assert!(
        output.status.success(),
        "{command} rejected generated service files:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn options<'a>(
    kind: ServiceKind,
    out: &'a Path,
    exec: &'a Path,
    config: &'a Path,
) -> ServiceOptions<'a> {
    ServiceOptions {
        kind,
        name: "workspace-backup",
        interval_secs: 3600,
        keep: 7,
        out,
        exec,
        config,
        data: None,
        remote: None,
    }
}

#[test]
fn generated_service_files_pass_platform_validators() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("workspace.toml");
    fs::write(
        &config,
        "[backup]\n[backup.store]\ntype = \"fs\"\nroot = \"backup\"\n",
    )
    .unwrap();
    let exec = PathBuf::from(env!("CARGO_BIN_EXE_workspace"));

    #[cfg(target_os = "linux")]
    {
        let out = dir.path().join("systemd");
        let written = generate(&options(ServiceKind::Systemd, &out, &exec, &config)).unwrap();
        let paths: Vec<&Path> = written.iter().map(PathBuf::as_path).collect();
        run_parser(
            "systemd-analyze",
            &[&[Path::new("verify")], &paths[..]].concat(),
        );
    }

    #[cfg(target_os = "macos")]
    {
        let out = dir.path().join("launchd");
        let written = generate(&options(ServiceKind::Launchd, &out, &exec, &config)).unwrap();
        run_parser("plutil", &[Path::new("-lint"), written[0].as_path()]);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        if require_parser() {
            panic!("no service parser configured for this platform");
        }
    }
}
