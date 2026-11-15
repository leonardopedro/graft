//! `cdc` command line interface.
//!
//! Local state lives in the data directory: the master key file and a small
//! `cdc-volume` descriptor holding the volume id, the remote spec and the key
//! path, so later commands only need `--data`.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use cdc_volume::{Store, crypto::MASTER_KEY_LEN};
use clap::{Parser, Subcommand};
use graft::{
    core::{LogId, VolumeId},
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft},
};

/// File inside the data directory that records the volume identity.
const STATE_FILE: &str = "cdc-volume";
/// Default master key file name inside the data directory.
const KEY_FILE: &str = "volume.key";
/// Default remote directory name inside the data directory.
const REMOTE_DIR: &str = "remote";

#[derive(Debug, Parser)]
#[command(
    name = "cdc",
    about = "Encrypted, deduplicating content-addressed store on Graft",
    version
)]
pub struct Cli {
    /// Graft data directory (local index and cache).
    #[arg(long, global = true, default_value = "./cdc-data", value_name = "DIR")]
    data: PathBuf,

    /// Master key file. Defaults to `<data>/volume.key`.
    #[arg(long, global = true, value_name = "FILE")]
    key: Option<PathBuf>,

    /// Remote: `memory`, `fs:DIR`, or `s3:BUCKET[/PREFIX]`.
    #[arg(long, global = true, value_name = "SPEC")]
    remote: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a local volume descriptor and master key.
    Init,
    /// Chunk and store one file under NAME.
    Put {
        /// Name (path) to store the file under.
        name: String,
        /// Local file to upload.
        file: PathBuf,
    },
    /// Write one file, or a range of it, to stdout or a file.
    Get {
        name: String,
        #[arg(short, long, value_name = "FILE")]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 0, value_name = "BYTES")]
        offset: u64,
        #[arg(long, value_name = "BYTES")]
        length: Option<u64>,
    },
    /// List stored files, optionally restricted to a prefix.
    Ls { prefix: Option<String> },
    /// Show one file's size, chunk count and mtime.
    Stat { name: String },
    /// Remove a file entry. Chunk data is reclaimed by GC, not by this command.
    Rm { name: String },
    /// Push local commits to the configured remote.
    Sync,
    /// Fetch the remote's commits and fast-forward the local volume to them.
    Pull,
    /// Print what another machine needs to attach to this volume.
    Share,
    /// Point a fresh data directory at an existing remote volume.
    Attach {
        /// Volume id, as printed by `cdc share`.
        #[arg(long, value_name = "VID")]
        vid: String,
        /// Remote log id, as printed by `cdc share`.
        #[arg(long, value_name = "LOG")]
        log: String,
    },
    /// Rewrite the volume with only live data and point the state file at it.
    Gc,
}

/// Records what `init` created: volume id, remote spec, key path.
#[derive(Debug, Clone)]
struct State {
    vid: VolumeId,
    remote: String,
    key: PathBuf,
}

pub fn run(cli: Cli) -> Result<()> {
    let data = cli.data.clone();
    match &cli.command {
        Command::Init => init(&data, cli.key.clone(), cli.remote.clone()),
        Command::Put { name, file } => {
            let (rt, state) = open(&data, cli.key.clone(), cli.remote.clone())?;
            let key = load_key(&state.key)?;
            let store = Store::open(rt, state.vid, &key)?;
            let input = fs::File::open(file).with_context(|| format!("open {}", file.display()))?;
            let stat = store.put_file(name.as_bytes(), io::BufReader::new(input))?;
            println!(
                "put {name}: {} bytes in {} chunks",
                stat.size, stat.chunk_count
            );
            Ok(())
        }
        Command::Get { name, out, offset, length } => get(
            &data,
            cli.key.clone(),
            cli.remote.clone(),
            name,
            out.as_deref(),
            *offset,
            *length,
        ),
        Command::Ls { prefix } => {
            let (_, store) = store(&data, cli.key.clone(), cli.remote.clone())?;
            let prefix = prefix.clone().unwrap_or_default();
            for stat in store.ls(prefix.as_bytes())? {
                println!("{:>12} {}", stat.size, String::from_utf8_lossy(&stat.path));
            }
            Ok(())
        }
        Command::Stat { name } => {
            let (_, store) = store(&data, cli.key.clone(), cli.remote.clone())?;
            let stat = store
                .stat(name.as_bytes())?
                .ok_or_else(|| anyhow!("no such file: {name}"))?;
            println!("name: {}", String::from_utf8_lossy(&stat.path));
            println!("size: {}", stat.size);
            println!("chunks: {}", stat.chunk_count);
            println!("mtime_ns: {}", stat.mtime_ns);
            Ok(())
        }
        Command::Rm { name } => {
            let (_, store) = store(&data, cli.key.clone(), cli.remote.clone())?;
            if store.delete_file(name.as_bytes())? {
                println!("removed {name}");
                Ok(())
            } else {
                bail!("no such file: {name}")
            }
        }
        Command::Sync => {
            let (rt, state) = open(&data, cli.key.clone(), cli.remote.clone())?;
            rt.volume_push(state.vid.clone())?;
            println!("pushed {}", state.vid);
            Ok(())
        }
        Command::Pull => {
            let (rt, state) = open(&data, cli.key.clone(), cli.remote.clone())?;
            pull(&rt, state.vid.clone())?;
            println!("pulled {}", state.vid);
            Ok(())
        }
        Command::Share => {
            let (rt, state) = open(&data, cli.key.clone(), cli.remote.clone())?;
            let log = rt.volume_get(&state.vid)?.remote;
            println!("vid: {}", state.vid);
            println!("log: {}", log.serialize());
            println!("remote: {}", state.remote);
            println!(
                "key: {} (copy this file to the other machine)",
                state.key.display()
            );
            println!(
                "next: `cdc attach --vid {} --log {} --remote {} --key <key-file>`",
                state.vid,
                log.serialize(),
                state.remote,
            );
            Ok(())
        }
        Command::Attach { vid, log } => {
            attach(&data, cli.key.clone(), cli.remote.clone(), vid, log)
        }
        Command::Gc => gc(&data, cli.key.clone(), cli.remote.clone()),
    }
}

fn get(
    data: &Path,
    key_override: Option<PathBuf>,
    remote_override: Option<String>,
    name: &str,
    out: Option<&Path>,
    offset: u64,
    length: Option<u64>,
) -> Result<()> {
    let (_, store) = store(data, key_override, remote_override)?;
    let len = length.unwrap_or(u64::MAX);
    let bytes = store
        .read_range(name.as_bytes(), offset, len)
        .with_context(|| format!("read {name}"))?;
    match out {
        Some(path) => {
            fs::write(path, &bytes).with_context(|| format!("write {}", path.display()))?;
            println!("wrote {} bytes to {}", bytes.len(), path.display());
        }
        None => {
            io::stdout().write_all(&bytes)?;
        }
    }
    Ok(())
}

/// Rewrite the live set into a fresh volume, then swap the state file over.
///
/// The swap is a rename, so a crash before it leaves the old volume
/// authoritative; the old volume is deliberately kept for a grace period.
fn gc(data: &Path, key_override: Option<PathBuf>, remote_override: Option<String>) -> Result<()> {
    let (rt, state) = open(data, key_override, remote_override)?;
    let key = load_key(&state.key)?;
    let source = Store::open(rt.clone(), state.vid.clone(), &key)?;
    let before = cdc_volume::gc::stats(&source)?;

    let new_vid = rt.volume_open(None, None, None)?.vid;
    let target = Store::create(rt, new_vid.clone(), &key)?;
    let moved = cdc_volume::gc::rewrite(&source, &target)?;

    write_state(
        data,
        &State {
            vid: new_vid.clone(),
            remote: state.remote.clone(),
            key: state.key.clone(),
        },
    )?;

    println!("rewrote {} -> {new_vid}", state.vid);
    println!(
        "pages: {} -> {} ({} reclaimed, {} dead frames dropped)",
        moved.before.pages,
        moved.after.pages,
        moved.pages_reclaimed(),
        before
            .dead_chunk_frames
            .saturating_sub(moved.after.dead_chunk_frames)
    );
    println!(
        "kept {} live: {} files, {} chunks, {} bytes",
        moved.after.pages, moved.after.files, moved.after.chunks, moved.after.live_bytes
    );
    println!(
        "old volume {} is still on disk; `cdc sync` pushes the new one",
        state.vid
    );
    Ok(())
}

/// Point a fresh data directory at a volume another machine already pushed.
///
/// A fresh runtime has no local log for the volume, so both ids must come from
/// `cdc share`: opening by volume id alone would mint a new remote log and see an
/// empty volume. The pull only moves the sync point; pages stay on the remote
/// until something reads them.
fn attach(
    data: &Path,
    key_override: Option<PathBuf>,
    remote_override: Option<String>,
    vid: &str,
    log: &str,
) -> Result<()> {
    if read_state(data)?.is_some() {
        bail!(
            "{} already holds a volume; use another --data directory",
            data.display()
        );
    }
    let remote_spec = remote_override
        .ok_or_else(|| anyhow!("attach needs --remote, e.g. --remote fs:/srv/cdc-remote"))?;
    let key_path = absolute(&key_override.ok_or_else(|| {
        anyhow!("attach needs --key FILE, the key file copied from the other machine")
    })?)?;
    if !key_path.exists() {
        bail!("key file {} does not exist", key_path.display());
    }
    let key = load_key(&key_path)?;
    let vid: VolumeId = vid
        .parse()
        .map_err(|err| anyhow!("bad volume id `{vid}`: {err}"))?;
    let log: LogId = log
        .parse()
        .map_err(|err| anyhow!("bad log id `{log}`: {err}"))?;

    let rt = runtime(data, &remote_spec)?;
    rt.volume_open(Some(vid.clone()), None, Some(log))?;
    // Attach is the one moment an edge has no local copy to fall back on, so a
    // single transient remote error must not end the whole setup.
    pull(&rt, vid.clone())?;

    // Pulling can succeed against a readable log while the key is wrong; only a
    // superblock read proves this machine can actually decrypt the volume. Read
    // exactly that one page: walking the file index here would download frames
    // far beyond what attach needs, and attach is the one command an edge runs
    // against a volume it has never seen.
    let store = Store::open(rt, vid.clone(), &key)
        .with_context(|| format!("open {vid} with {}", key_path.display()))?;
    // Byte-level accounting is only exact after GC (see the plan's risks), so
    // the superblock's generation is the honest thing to report here.
    let generation = store.volume().superblock()?.generation;

    write_state(
        data,
        &State {
            vid: vid.clone(),
            remote: remote_spec,
            key: key_path,
        },
    )?;
    println!("attached to {} at generation {generation}", vid.serialize());
    println!("local index is empty; pages are fetched on demand from the remote");
    Ok(())
}

fn init(data: &Path, key_override: Option<PathBuf>, remote_override: Option<String>) -> Result<()> {
    if read_state(data)?.is_some() {
        bail!(
            "{} already holds a volume; delete {} to start over",
            data.display(),
            data.join(STATE_FILE).display()
        );
    }
    fs::create_dir_all(data).with_context(|| format!("create {}", data.display()))?;

    let key_path = absolute(&key_override.unwrap_or_else(|| data.join(KEY_FILE)))?;
    let remote_spec =
        remote_override.unwrap_or_else(|| format!("fs:{}", data.join(REMOTE_DIR).display()));
    let key = create_key(&key_path)?;

    let rt = runtime(data, &remote_spec)?;
    let vid = rt.volume_open(None, None, None)?.vid;
    Store::create(rt, vid.clone(), &key)?;
    write_state(
        data,
        &State {
            vid: vid.clone(),
            remote: remote_spec,
            key: key_path,
        },
    )?;
    println!("initialized volume {vid} in {}", data.display());
    Ok(())
}

fn store(
    data: &Path,
    key_override: Option<PathBuf>,
    remote_override: Option<String>,
) -> Result<(Runtime, Store)> {
    let (rt, state) = open(data, key_override, remote_override)?;
    let key = load_key(&state.key)?;
    let store = Store::open(rt.clone(), state.vid, &key)?;
    Ok((rt, store))
}

/// Build a runtime for an initialized volume.
fn open(
    data: &Path,
    key_override: Option<PathBuf>,
    remote_override: Option<String>,
) -> Result<(Runtime, State)> {
    let mut state = read_state(data)?.ok_or_else(|| {
        anyhow!(
            "no volume in {}; run `cdc init` there first",
            data.display()
        )
    })?;
    if let Some(key) = key_override {
        state.key = absolute(&key)?;
    }
    if let Some(remote) = remote_override {
        state.remote = remote;
    }
    let rt = runtime(data, &state.remote)?;
    Ok((rt, state))
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// How many times a pull is attempted before a remote error is fatal.
const PULL_ATTEMPTS: u32 = 4;
/// First backoff between pull attempts; doubles on each retry.
const PULL_BACKOFF: Duration = Duration::from_millis(50);

/// Pull the remote's commits, retrying transient failures.
///
/// `volume_pull` either advances the sync point or leaves it alone, so retrying
/// is safe: a second attempt re-reads the log and applies whatever arrived.
fn pull(rt: &Runtime, vid: VolumeId) -> Result<()> {
    retrying(PULL_ATTEMPTS, PULL_BACKOFF, "pull", || {
        rt.volume_pull(vid.clone()).map_err(Into::into)
    })
}

/// Run `op`, retrying failures with exponential backoff.
///
/// A remote that is briefly unreachable — a dropped connection, a throttled
/// request — should not fail an `attach`, which has no local copy to fall back
/// on. Operations passed here must be safe to repeat.
fn retrying<T>(
    attempts: u32,
    mut backoff: Duration,
    label: &str,
    mut op: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut last = None;
    for attempt in 1..=attempts {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) if attempt < attempts => {
                eprintln!(
                    "{label} attempt {attempt}/{attempts} failed ({err}); retrying in {}ms",
                    backoff.as_millis()
                );
                thread::sleep(backoff);
                backoff *= 2;
            }
            Err(err) => last = Some(err),
        }
    }
    Err(anyhow!(
        "{label} failed after {attempts} attempts: {}",
        last.expect("the loop only exits with a stored error")
    ))
}

fn runtime(data: &Path, remote_spec: &str) -> Result<Runtime> {
    fs::create_dir_all(data).with_context(|| format!("create {}", data.display()))?;
    let remote = parse_remote(remote_spec)?;
    setup_graft(GraftConfig {
        remote,
        data_dir: data.to_path_buf(),
        autosync: None,
    })
    .map_err(|err| anyhow!("graft setup: {err}"))
}

/// Parse `memory`, `fs:DIR`, or `s3:BUCKET[/PREFIX]`.
///
/// S3 credentials, region and endpoint come from the environment, which is what
/// Graft's S3-compatible remote already reads (`AWS_*`).
fn parse_remote(spec: &str) -> Result<RemoteConfig> {
    if spec == "memory" {
        return Ok(RemoteConfig::Memory);
    }
    if let Some(root) = spec.strip_prefix("fs:") {
        if root.is_empty() {
            bail!("`fs:` needs a directory, e.g. fs:/var/lib/cdc-remote");
        }
        return Ok(RemoteConfig::Fs { root: root.to_string() });
    }
    if let Some(rest) = spec.strip_prefix("s3:") {
        let (bucket, prefix) = match rest.split_once('/') {
            Some((bucket, prefix)) if !prefix.is_empty() => (bucket, Some(prefix.to_string())),
            _ => (rest, None),
        };
        if bucket.is_empty() {
            bail!("`s3:` needs a bucket, e.g. s3:my-bucket/volumes/cdc");
        }
        return Ok(RemoteConfig::S3Compatible { bucket: bucket.to_string(), prefix });
    }
    bail!("unknown remote `{spec}`; use memory, fs:DIR, or s3:BUCKET[/PREFIX]")
}

fn read_state(data: &Path) -> Result<Option<State>> {
    let path = data.join(STATE_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };

    let mut vid = None;
    let mut remote = None;
    let mut key = None;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once('=') else {
            bail!("malformed line in {}: {line:?}", path.display());
        };
        match name {
            "vid" => {
                vid = Some(
                    value
                        .parse::<VolumeId>()
                        .map_err(|err| anyhow!("bad volume id in {}: {err}", path.display()))?,
                )
            }
            "remote" => remote = Some(value.to_string()),
            "key" => key = Some(PathBuf::from(value)),
            other => bail!("unknown key {other:?} in {}", path.display()),
        }
    }

    Ok(Some(State {
        vid: vid.ok_or_else(|| anyhow!("missing vid in {}", path.display()))?,
        remote: remote.ok_or_else(|| anyhow!("missing remote in {}", path.display()))?,
        key: key.ok_or_else(|| anyhow!("missing key in {}", path.display()))?,
    }))
}

/// Write the state file by rename, so a reader never sees a half-written one.
fn write_state(data: &Path, state: &State) -> Result<()> {
    let path = data.join(STATE_FILE);
    let staging = data.join(format!("{STATE_FILE}.tmp"));
    let body = format!(
        "vid={}\nremote={}\nkey={}\n",
        state.vid,
        state.remote,
        state.key.display()
    );
    fs::write(&staging, body).with_context(|| format!("write {}", staging.display()))?;
    fs::rename(&staging, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn create_key(path: &Path) -> Result<[u8; MASTER_KEY_LEN]> {
    if path.exists() {
        return load_key(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let key: [u8; MASTER_KEY_LEN] = rand::random();
    fs::write(path, format!("{}\n", encode_hex(&key)))
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(key)
}

fn load_key(path: &Path) -> Result<[u8; MASTER_KEY_LEN]> {
    let text = fs::read_to_string(path).with_context(|| format!("read key {}", path.display()))?;
    let bytes = decode_hex(text.trim()).with_context(|| format!("parse key {}", path.display()))?;
    <[u8; MASTER_KEY_LEN]>::try_from(bytes.as_slice())
        .map_err(|_| anyhow!("key {} must be {MASTER_KEY_LEN} bytes", path.display()))
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    let digits = text.as_bytes();
    if !digits.len().is_multiple_of(2) {
        bail!("hex string has an odd length");
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.as_chunks::<2>().0 {
        let hi = hex_digit(pair[0])?;
        let lo = hex_digit(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => bail!("invalid hex digit {:?}", char::from(byte)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn retrying_survives_transient_failures() {
        let calls = Cell::new(0);
        let result = retrying(4, Duration::ZERO, "pull", || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(anyhow!("connection reset"))
            } else {
                Ok(calls.get())
            }
        });
        assert_eq!(result.unwrap(), 3);
        assert_eq!(calls.get(), 3, "a success must stop the retries");
    }

    #[test]
    fn retrying_gives_up_after_the_budget() {
        let calls = Cell::new(0);
        let err = retrying::<()>(3, Duration::ZERO, "pull", || {
            calls.set(calls.get() + 1);
            Err(anyhow!("no such bucket"))
        })
        .unwrap_err();
        assert_eq!(calls.get(), 3);
        let text = format!("{err:#}");
        assert!(
            text.contains("pull failed after 3 attempts") && text.contains("no such bucket"),
            "unhelpful failure: {text}"
        );
    }
}
