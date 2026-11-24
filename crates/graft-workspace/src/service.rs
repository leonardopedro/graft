//! Service-file generation for scheduled workspace backups.
//!
//! The generated unit runs `workspace backup` once per interval and leaves
//! scheduling to systemd or launchd. That is preferable to supervising the
//! foreground `--every` loop: the service manager records missed runs and can
//! restart a failed backup without a second scheduler inside the process.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ServiceKind {
    Systemd,
    Launchd,
}

#[derive(Debug, Clone)]
pub struct ServiceOptions<'a> {
    pub kind: ServiceKind,
    /// Unit/plist base name. Restricted to a conservative filename alphabet.
    pub name: &'a str,
    pub interval_secs: u64,
    pub keep: usize,
    pub out: &'a Path,
    pub exec: &'a Path,
    pub config: &'a Path,
    pub data: Option<&'a Path>,
    pub remote: Option<&'a str>,
}

/// Write the service file(s) and return their paths.
pub fn generate(options: &ServiceOptions<'_>) -> Result<Vec<PathBuf>> {
    if options.interval_secs == 0 {
        bail!("backup interval must be at least 1 second");
    }
    if options.keep == 0 {
        bail!("--keep must be at least 1");
    }
    if !valid_name(options.name) {
        bail!(
            "service name {:?} must use only letters, digits, '.', '_' and '-'",
            options.name
        );
    }

    let config = absolute(options.config)?;
    let exec = options.exec.to_path_buf();
    let working_dir = std::env::current_dir().context("read current directory")?;
    let mut args = vec![
        exec.display().to_string(),
        "--config".to_string(),
        config.display().to_string(),
    ];
    if let Some(data) = options.data {
        args.push("--data".to_string());
        args.push(absolute(data)?.display().to_string());
    }
    if let Some(remote) = options.remote {
        args.push("--remote".to_string());
        args.push(remote.to_string());
    }
    args.push("backup".to_string());
    args.push("--keep".to_string());
    args.push(options.keep.to_string());

    fs::create_dir_all(options.out)
        .with_context(|| format!("create service directory {}", options.out.display()))?;

    let written = match options.kind {
        ServiceKind::Systemd => {
            let service = options.out.join(format!("{}.service", options.name));
            let timer = options.out.join(format!("{}.timer", options.name));
            fs::write(&service, systemd_service(&args, &working_dir))?;
            fs::write(&timer, systemd_timer(options.name, options.interval_secs))?;
            vec![service, timer]
        }
        ServiceKind::Launchd => {
            let plist = options
                .out
                .join(format!("com.graft.{}.plist", options.name));
            fs::write(
                &plist,
                launchd_plist(options.name, &args, &working_dir, options.interval_secs),
            )?;
            vec![plist]
        }
    };
    Ok(written)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("read current directory")?
        .join(path))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn systemd_service(args: &[String], working_dir: &Path) -> String {
    let exec = args
        .iter()
        .map(|arg| systemd_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
         Description=Graft workspace backup\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         WorkingDirectory={}\n\
         ExecStart={exec}\n\
         Nice=10\n\
         IOSchedulingClass=best-effort\n\
         IOSchedulingPriority=7\n",
        systemd_arg(&working_dir.display().to_string())
    )
}

fn systemd_timer(name: &str, interval_secs: u64) -> String {
    format!(
        "[Unit]\n\
         Description=Run the Graft workspace backup every {interval_secs} seconds\n\
         \n\
         [Timer]\n\
         OnBootSec=5min\n\
         OnUnitActiveSec={interval_secs}s\n\
         AccuracySec=1min\n\
         Persistent=true\n\
         Unit={name}.service\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

fn launchd_plist(name: &str, args: &[String], working_dir: &Path, interval_secs: u64) -> String {
    let mut arguments = String::new();
    for arg in args {
        arguments.push_str(&format!("        <string>{}</string>\n", xml(arg)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>Label</key>\n\
         \x20   <string>com.graft.{name}</string>\n\
         \x20   <key>ProgramArguments</key>\n\
         \x20   <array>\n{arguments}    </array>\n\
         \x20   <key>WorkingDirectory</key>\n\
         \x20   <string>{}</string>\n\
         \x20   <key>StartInterval</key>\n\
         \x20   <integer>{interval_secs}</integer>\n\
         \x20   <key>RunAtLoad</key>\n\
         \x20   <true/>\n\
         \x20   <key>ProcessType</key>\n\
         \x20   <string>Background</string>\n\
         </dict>\n\
         </plist>\n",
        xml(&working_dir.display().to_string())
    )
}

/// Quote a systemd command argument when it contains anything beyond the
/// conservative filename/URL alphabet.
fn systemd_arg(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'/' | b'.' | b'_' | b'-' | b':' | b'@' | b'+' | b'=' | b','
                )
        })
    {
        return value.to_string();
    }
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options(out: &Path) -> ServiceOptions<'_> {
        ServiceOptions {
            kind: ServiceKind::Systemd,
            name: "workspace-backup",
            interval_secs: 3600,
            keep: 7,
            out,
            exec: Path::new("/usr/local/bin/workspace"),
            config: Path::new("workspace.toml"),
            data: None,
            remote: None,
        }
    }

    #[test]
    fn writes_a_systemd_service_and_timer_with_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let written = generate(&base_options(dir.path())).unwrap();
        assert_eq!(written.len(), 2);
        let service = fs::read_to_string(&written[0]).unwrap();
        assert!(
            service.contains("ExecStart=/usr/local/bin/workspace --config "),
            "{service}"
        );
        assert!(service.contains("backup --keep 7"), "{service}");
        assert!(service.contains("WorkingDirectory="), "{service}");
        let timer = fs::read_to_string(&written[1]).unwrap();
        assert!(timer.contains("OnUnitActiveSec=3600s"), "{timer}");
        assert!(timer.contains("Unit=workspace-backup.service"), "{timer}");
    }

    #[test]
    fn writes_a_launchd_plist() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = base_options(dir.path());
        options.kind = ServiceKind::Launchd;
        options.name = "nightly";
        let written = generate(&options).unwrap();
        assert_eq!(written.len(), 1);
        assert!(written[0].ends_with("com.graft.nightly.plist"));
        let plist = fs::read_to_string(&written[0]).unwrap();
        assert!(plist.contains("<string>backup</string>"), "{plist}");
        assert!(plist.contains("<integer>3600</integer>"), "{plist}");
        assert!(plist.contains("<key>WorkingDirectory</key>"), "{plist}");
    }

    #[test]
    fn rejects_unsafe_names_and_zero_intervals() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = base_options(dir.path());
        options.name = "../escape";
        assert!(generate(&options).is_err());
        let mut options = base_options(dir.path());
        options.interval_secs = 0;
        assert!(generate(&options).is_err());
    }
}
