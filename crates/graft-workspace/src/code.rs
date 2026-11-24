//! Lane 2: versioned code checkpoints.
//!
//! A checkpoint is a tree manifest stored in the encrypted `cdc-volume`. The
//! manifest lists each file's relative path, blake3 hash, size and mode. File
//! contents are content-addressed, so an unchanged file is stored once across
//! any number of checkpoints.
//!
//! Code contents use the encrypted archive class rather than raw media:
//! source is often sensitive, and `cdc-volume` deduplicates it at chunk level
//! while `blob read_range` still serves restore. A `[archive]` key is therefore
//! required for `code snapshot`.
//!
//! When the tree is a git repository, a `git bundle` of all refs is stored as
//! well. The bundle is the self-contained artifact a release deposits, and it
//! is a backup independent of this catalog.
//!
//! Checkpoint blobs are pinned: [`crate::Blobs::sweep_unreferenced_cdc`] must
//! never collect the bytes of a tree that an entry still names.

use std::{
    fs::{self, File},
    io::{self, Cursor},
    path::{Component, Path, PathBuf},
    process::Command,
};

use blake3::Hasher as Blake3;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::media::{BlobErr, Blobs};

/// Restore and manifest reads in bounded chunks.
const CHUNK: u64 = 64 * 1024;
/// Manifests are small; refusing a huge one keeps a corrupt hash from
/// allocating without bound.
const MAX_MANIFEST: u64 = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum CodeErr {
    #[error("media: {0}")]
    Blob(#[from] BlobErr),

    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("manifest: {0}")]
    Json(#[from] serde_json::Error),

    #[error("path {0:?} is not a safe relative path")]
    UnsafePath(String),

    #[error("file {path:?} hashes to {actual}, manifest says {expected}")]
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("unsupported tree manifest version {0}")]
    ManifestVersion(u32),

    #[error("git: {0}")]
    Git(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeFile {
    /// Relative path with `/` separators.
    pub path: String,
    pub hash: String,
    pub size: u64,
    /// Unix permission bits (`mode & 0o777`); 0o644 when unknown.
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_bundle: Option<String>,
    files: Vec<CodeFile>,
}

/// Result of writing a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSnapshot {
    /// Hash of the tree manifest blob; this is the `code_tree` value.
    pub tree: String,
    pub files: usize,
    pub bytes: u64,
    /// Hash of the git bundle, when the tree is a repository with commits.
    pub git_bundle: Option<String>,
}

/// Result of restoring a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreStats {
    pub files: usize,
    pub bytes: u64,
    pub git_bundle: Option<String>,
}

/// Snapshot `root` into the workspace's blob store and pin every blob.
pub fn snapshot(blobs: &Blobs<'_>, root: &Path) -> Result<CodeSnapshot, CodeErr> {
    let root = root
        .canonicalize()
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", root.display())))?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        )
        .into());
    }

    let mut files = Vec::new();
    let mut bytes = 0u64;
    for relative in collect_files(&root)? {
        let full = root.join(&relative);
        let metadata = match fs::metadata(&full) {
            Ok(metadata) if metadata.is_file() => metadata,
            // A tracked-but-deleted path is simply absent from the checkpoint.
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        let ingested = blobs.ingest_cdc(File::open(&full)?, Some(metadata.len()), None, None)?;
        blobs.pin(&ingested.blob.hash)?;
        files.push(CodeFile {
            path: relative,
            hash: ingested.blob.hash,
            size: metadata.len(),
            mode: mode_of(&metadata),
        });
        bytes += metadata.len();
    }

    let git_bundle = bundle(&root, blobs)?;
    let manifest = Manifest {
        version: 1,
        git_bundle: git_bundle.clone(),
        files,
    };
    let json = serde_json::to_vec_pretty(&manifest)?;
    let ingested = blobs.ingest_cdc(
        Cursor::new(&json),
        Some(json.len() as u64),
        None,
        Some("application/json"),
    )?;
    blobs.pin(&ingested.blob.hash)?;

    Ok(CodeSnapshot {
        tree: ingested.blob.hash,
        files: manifest.files.len(),
        bytes,
        git_bundle,
    })
}

/// Restore a checkpoint into `dest`, verifying every file's hash before it is
/// moved into place. A partial restore therefore leaves `.part` files, never a
/// silently corrupted checkout.
pub fn restore(blobs: &Blobs<'_>, tree: &str, dest: &Path) -> Result<RestoreStats, CodeErr> {
    let manifest = read_manifest(blobs, tree)?;
    let mut bytes = 0u64;

    for file in &manifest.files {
        let relative = safe_relative(&file.path)?;
        let out = dest.join(&relative);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let part = out.with_file_name(format!(
            ".{}.part",
            out.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("restore")
        ));

        let mut hasher = Blake3::new();
        let mut written = 0u64;
        {
            let mut sink = File::create(&part)?;
            let mut offset = 0u64;
            loop {
                let data = blobs.read_range(&file.hash, offset, Some(CHUNK))?;
                if data.is_empty() {
                    break;
                }
                hasher.update(&data);
                io::Write::write_all(&mut sink, &data)?;
                offset += data.len() as u64;
                written += data.len() as u64;
                if data.len() < CHUNK as usize {
                    break;
                }
            }
            sink.sync_all()?;
        }

        let actual = format!("blake3:{}", hasher.finalize().to_hex());
        if actual != file.hash || written != file.size {
            let _ = fs::remove_file(&part);
            return Err(CodeErr::HashMismatch {
                path: file.path.clone(),
                expected: file.hash.clone(),
                actual,
            });
        }

        fs::rename(&part, &out)?;
        set_mode(&out, file.mode)?;
        bytes += written;
    }

    Ok(RestoreStats {
        files: manifest.files.len(),
        bytes,
        git_bundle: manifest.git_bundle,
    })
}

/// Read and validate a tree manifest without restoring it.
pub fn manifest(blobs: &Blobs<'_>, tree: &str) -> Result<Vec<CodeFile>, CodeErr> {
    Ok(read_manifest(blobs, tree)?.files)
}

/// The git bundle named by a tree manifest, if the checkpoint came from a
/// repository with commits.
pub fn git_bundle_hash(blobs: &Blobs<'_>, tree: &str) -> Result<Option<String>, CodeErr> {
    Ok(read_manifest(blobs, tree)?.git_bundle)
}

fn read_manifest(blobs: &Blobs<'_>, tree: &str) -> Result<Manifest, CodeErr> {
    let data = blobs.read_range(tree, 0, Some(MAX_MANIFEST))?;
    let manifest: Manifest = serde_json::from_slice(&data)?;
    if manifest.version != 1 {
        return Err(CodeErr::ManifestVersion(manifest.version));
    }
    Ok(manifest)
}

fn bundle(root: &Path, blobs: &Blobs<'_>) -> Result<Option<String>, CodeErr> {
    if !root.join(".git").exists() {
        return Ok(None);
    }
    let has_head = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()?;
    if !has_head.status.success() {
        // A repository with no commits has nothing to bundle.
        return Ok(None);
    }

    let path = std::env::temp_dir().join(format!(
        "graft-code-{}.bundle",
        std::process::id() as u64
            ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or_default()
    ));
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("bundle")
        .arg("create")
        .arg(&path)
        .arg("--all")
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let _ = fs::remove_file(&path);
        return Err(CodeErr::Git(stderr));
    }

    let size = fs::metadata(&path)?.len();
    let ingested = blobs.ingest_cdc(File::open(&path)?, Some(size), None, None)?;
    fs::remove_file(&path)?;
    blobs.pin(&ingested.blob.hash)?;
    Ok(Some(ingested.blob.hash))
}

/// Relative file paths, sorted. In a git repository `git ls-files` supplies
/// the ignore rules (tracked files plus untracked, non-ignored files). Other
/// trees are walked directly, skipping `.git` and `.workspaceignore`.
pub(crate) fn collect_files(root: &Path) -> Result<Vec<String>, CodeErr> {
    if root.join(".git").exists() {
        return git_files(root);
    }

    let ignores = read_ignore(root)?;
    let mut files = Vec::new();
    walk(root, root, &ignores, &mut files)?;
    files.sort();
    Ok(files)
}

fn git_files(root: &Path) -> Result<Vec<String>, CodeErr> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()?;
    if !output.status.success() {
        return Err(CodeErr::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let mut files = Vec::new();
    for path in output.stdout.split(|byte| *byte == 0) {
        if path.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(path)
            .map_err(|_| CodeErr::UnsafePath(String::from_utf8_lossy(path).into()))?;
        files.push(path.to_string());
    }
    files.sort();
    Ok(files)
}

fn walk(root: &Path, dir: &Path, ignores: &[String], files: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("walk stays under root")
            .to_string_lossy()
            .replace('\\', "/");
        if ignored(&relative, ignores) {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            walk(root, &path, ignores, files)?;
        } else if metadata.is_file() {
            files.push(relative);
        }
    }
    Ok(())
}

fn read_ignore(root: &Path) -> Result<Vec<String>, CodeErr> {
    let path = root.join(".workspaceignore");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

/// Ignore entries are relative paths; a trailing `/` matches a directory
/// prefix. `*` matches within one path component, `**` across components.
fn ignored(relative: &str, ignores: &[String]) -> bool {
    relative == ".workspaceignore"
        || ignores.iter().any(|pattern| {
            if let Some(dir) = pattern.strip_suffix('/') {
                relative == dir || relative.starts_with(&format!("{dir}/"))
            } else {
                glob(pattern, relative)
            }
        })
}

fn glob(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let mut dp = vec![vec![false; value.len() + 1]; pattern.len() + 1];
    dp[0][0] = true;
    for i in 1..=pattern.len() {
        if pattern[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
        for j in 1..=value.len() {
            dp[i][j] = match pattern[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                ch => dp[i - 1][j - 1] && ch == value[j - 1],
            };
        }
    }
    dp[pattern.len()][value.len()]
}

pub(crate) fn safe_relative(path: &str) -> Result<PathBuf, CodeErr> {
    let candidate = Path::new(path);
    if candidate.as_os_str().is_empty() || candidate.is_absolute() {
        return Err(CodeErr::UnsafePath(path.to_string()));
    }
    let mut safe = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CodeErr::UnsafePath(path.to_string()));
            }
        }
    }
    if safe.as_os_str().is_empty() {
        return Err(CodeErr::UnsafePath(path.to_string()));
    }
    Ok(safe)
}

#[cfg(unix)]
pub(crate) fn mode_of(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_metadata: &fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Read a code blob as a plain reader, for callers that want the git bundle.
/// Raw blobs still work because deduplication may find the same bytes in the
/// media class first.
pub fn read_blob(blobs: &Blobs<'_>, hash: &str, mut sink: impl io::Write) -> Result<u64, CodeErr> {
    let mut offset = 0u64;
    loop {
        let data = blobs.read_range(hash, offset, Some(CHUNK))?;
        if data.is_empty() {
            break;
        }
        sink.write_all(&data)?;
        offset += data.len() as u64;
        if data.len() < CHUNK as usize {
            break;
        }
    }
    Ok(offset)
}
