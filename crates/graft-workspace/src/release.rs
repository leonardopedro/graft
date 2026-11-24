//! Lane 4 staging: turn selected catalog entries into a self-describing
//! release directory.
//!
//! A staged release contains `datapackage.json` plus the bytes it names. Media
//! entries contribute their blob; code entries contribute every file in their
//! tree manifest. Nothing is published here: Zenodo deposit and approval need
//! credentials and the gatekeeper, so this is the artifact that path operates
//! on.
//!
//! Every resource carries both the workspace-canonical blake3 hash and, when
//! the bytes were streamed here, a sha256 digest that outlives Graft.

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use blake3::Hasher as Blake3;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{CatalogErr, Entry, Lane, Workspace, code};

const CHUNK: u64 = 64 * 1024;

#[derive(Debug, Error)]
pub enum ReleaseErr {
    #[error("catalog: {0}")]
    Catalog(#[from] CatalogErr),

    #[error("code: {0}")]
    Code(#[from] code::CodeErr),

    #[error("blob: {0}")]
    Blob(#[from] crate::BlobErr),

    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("manifest: {0}")]
    Json(#[from] serde_json::Error),

    #[error("no entry at {0:?}")]
    NoEntry(String),

    #[error("entry {0:?} has no bytes to stage")]
    EmptyEntry(String),

    #[error("{0} already holds a release")]
    NotEmpty(String),

    #[error("path {0:?} cannot be staged safely")]
    UnsafePath(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Resource {
    /// Entry path the resource came from.
    pub name: String,
    /// Path inside the staging directory, with `/` separators.
    pub path: String,
    pub bytes: u64,
    pub hash: String,
    pub sha256: String,
    pub lane: String,
}

#[derive(Debug, Clone, Serialize)]
struct DataPackage {
    name: String,
    version: String,
    workspace: String,
    resources: Vec<Resource>,
}

#[derive(Debug, Clone)]
pub struct StagedRelease {
    /// Hash of the datapackage bytes; this is the `manifest_hash`.
    pub manifest_hash: String,
    pub dir: PathBuf,
    pub resources: Vec<Resource>,
}

/// Write the selected entries into `out` and record their digests in
/// `datapackage.json`.
pub fn stage(
    workspace: &Workspace,
    entry_paths: &[String],
    out: &Path,
) -> Result<StagedRelease, ReleaseErr> {
    if entry_paths.is_empty() {
        return Err(ReleaseErr::EmptyEntry("<selection>".to_string()));
    }

    let mut entries = Vec::new();
    for path in entry_paths {
        let entry = workspace
            .catalog()
            .entry_get(path)?
            .ok_or_else(|| ReleaseErr::NoEntry(path.clone()))?;
        if entry.blob_hash.is_none() && entry.code_tree.is_none() {
            return Err(ReleaseErr::EmptyEntry(path.clone()));
        }
        entries.push(entry);
    }

    if out.exists() {
        let mut dir = fs::read_dir(out)?;
        if dir.next().is_some() {
            return Err(ReleaseErr::NotEmpty(out.display().to_string()));
        }
    }
    fs::create_dir_all(out)?;

    let mut resources = Vec::new();
    let mut seen = HashSet::new();
    for entry in &entries {
        if let Some(hash) = &entry.blob_hash {
            let relative = format!("files/{}", relative_path(&entry.path)?);
            if !seen.insert(relative.clone()) {
                return Err(ReleaseErr::UnsafePath(entry.path.clone()));
            }
            let resource = copy_blob(workspace, hash, entry, &relative, out)?;
            resources.push(resource);
        } else if let Some(tree) = &entry.code_tree {
            let files = code::manifest(&workspace.blobs()?, tree)?;
            for file in files {
                let relative = format!(
                    "files/{}/{}",
                    relative_path(&entry.path)?,
                    relative_path(&file.path)?
                );
                if !seen.insert(relative.clone()) {
                    return Err(ReleaseErr::UnsafePath(file.path));
                }
                let resource = copy_blob(workspace, &file.hash, entry, &relative, out)?;
                resources.push(resource);
            }
        }
    }

    let workspace_id = workspace
        .catalog()
        .workspace_id()
        .unwrap_or("(uninitialized)")
        .to_string();
    let package = DataPackage {
        name: format!("{workspace_id} release"),
        version: "1".to_string(),
        workspace: workspace_id,
        resources,
    };
    let json = serde_json::to_vec_pretty(&package)?;
    let manifest_hash = format!("blake3:{}", Blake3::new().update(&json).finalize().to_hex());
    fs::write(out.join("datapackage.json"), &json)?;

    Ok(StagedRelease {
        manifest_hash,
        dir: out.to_path_buf(),
        resources: package.resources,
    })
}

fn copy_blob(
    workspace: &Workspace,
    hash: &str,
    entry: &Entry,
    relative: &str,
    out: &Path,
) -> Result<Resource, ReleaseErr> {
    let target = out.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    let blobs = workspace.blobs()?;
    let mut file = File::create(&target)?;
    let mut blake = Blake3::new();
    let mut sha = Sha256::new();
    let mut written = 0u64;
    let mut offset = 0u64;
    loop {
        let data = blobs.read_range(hash, offset, Some(CHUNK))?;
        if data.is_empty() {
            break;
        }
        blake.update(&data);
        sha.update(&data);
        file.write_all(&data)?;
        written += data.len() as u64;
        offset += data.len() as u64;
        if data.len() < CHUNK as usize {
            break;
        }
    }
    file.sync_all()?;

    let actual = format!("blake3:{}", blake.finalize().to_hex());
    if actual != hash {
        let _ = fs::remove_file(&target);
        return Err(ReleaseErr::Blob(crate::BlobErr::HashMismatch {
            expected: hash.to_string(),
            actual,
        }));
    }

    Ok(Resource {
        name: entry.path.clone(),
        path: relative.to_string(),
        bytes: written,
        hash: actual,
        sha256: hex(&sha.finalize()),
        lane: lane_name(entry.lane).to_string(),
    })
}

fn lane_name(lane: Lane) -> &'static str {
    lane.as_str()
}

/// A path used inside the staging directory: relative, no `..`, no root. The
/// catalog validates entry paths, but a checkpoint manifest is data too.
fn relative_path(path: &str) -> Result<String, ReleaseErr> {
    let candidate = Path::new(path);
    if candidate.as_os_str().is_empty() || candidate.is_absolute() {
        return Err(ReleaseErr::UnsafePath(path.to_string()));
    }
    let mut parts = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ReleaseErr::UnsafePath(path.to_string()));
            }
        }
    }
    if parts.is_empty() {
        return Err(ReleaseErr::UnsafePath(path.to_string()));
    }
    Ok(parts.join("/"))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
