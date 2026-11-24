//! Atomic binary deployments: versioned trees plus a symlink switch.
//!
//! Layout:
//!
//! ```text
//! <root>/deployments/<release>-<hash>/   immutable installed tree + metadata
//! <root>/states/<release>-<hash>.json    installed-state manifest
//! <root>/current -> deployments/<id>     atomically renamed into place
//! <root>/previous -> deployments/<id>    convenience link; metadata is truth
//! ```
//!
//! The active deployment records its predecessor in
//! `.graft-deployment.json`, so rollback still works if the process dies
//! between switching `current` and updating `previous`.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::update::{self, ApplyReport, ManifestPublicKey, UpdateErr, UpdateRepo};

const META: &str = ".graft-deployment.json";
const META_SCHEMA: u32 = 1;

#[derive(Debug, Error)]
pub enum DeployErr {
    #[error("update: {0}")]
    Update(#[from] UpdateErr),

    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("deployment metadata: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0} is not a deployment root")]
    NotADeployment(PathBuf),

    #[error("no active deployment in {0}")]
    NoCurrent(PathBuf),

    #[error("the active deployment has no previous deployment")]
    NoPrevious,

    #[error("deployment {0} exists with different metadata")]
    Conflict(String),

    #[error("invalid deployment prune: {0}")]
    InvalidPrune(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeploymentMeta {
    schema: u32,
    release: String,
    manifest: String,
    previous: Option<String>,
    created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentInfo {
    pub id: String,
    pub release: String,
    pub manifest: String,
    pub current: bool,
}

#[derive(Debug, Clone)]
pub struct DeployReport {
    pub deployment: String,
    pub previous: Option<String>,
    pub reused: bool,
    pub applied: Option<ApplyReport>,
}

#[derive(Debug, Clone)]
pub struct RollbackReport {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default)]
pub struct PruneReport {
    pub kept: Vec<String>,
    pub removed: Vec<String>,
    pub bytes_removed: u64,
    pub dry_run: bool,
}

/// Install `release` into a new deployment and atomically switch `current`.
pub fn deploy(
    repo: &UpdateRepo,
    release: &str,
    root: &Path,
    trust: Option<&ManifestPublicKey>,
) -> Result<DeployReport, DeployErr> {
    let (target, hash) = repo.manifest_with_hash(release, trust)?;
    fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let deployments = root.join("deployments");
    let states = root.join("states");
    fs::create_dir_all(&deployments)?;
    fs::create_dir_all(&states)?;

    let id = deployment_id(release, &hash);
    let destination = deployments.join(&id);
    let current = current_id(&root)?;

    if destination.is_dir() {
        let meta = read_meta(&destination)?;
        if meta.manifest != hash {
            return Err(DeployErr::Conflict(id));
        }
        let state = states.join(format!("{id}.json"));
        if !state.is_file() {
            return Err(DeployErr::Conflict(id));
        }
        let previous = current
            .as_deref()
            .filter(|current| *current != id)
            .map(str::to_string);
        switch(&root, "current", &id)?;
        if let Some(previous) = &previous {
            switch(&root, "previous", previous)?;
        }
        return Ok(DeployReport {
            deployment: id,
            previous,
            reused: true,
            applied: None,
        });
    }

    let (previous_state, previous_root) = match &current {
        Some(current) => {
            let path = states.join(format!("{current}.json"));
            let state = path
                .is_file()
                .then(|| update::read_state(&path, &target.name))
                .transpose()?;
            (state, Some(deployments.join(current)))
        }
        None => (None, None),
    };

    let ticket = ticket();
    let temp = deployments.join(format!(".{id}.tmp-{ticket}"));
    let temp_state = states.join(format!(".{id}.tmp-{ticket}.json"));
    fs::create_dir_all(&temp)?;
    let applied = match repo.apply(
        release,
        &temp,
        &temp_state,
        previous_state.as_ref(),
        false,
        trust,
        previous_root.as_deref(),
    ) {
        Ok(report) => report,
        Err(err) => {
            let _ = fs::remove_dir_all(&temp);
            let _ = fs::remove_file(&temp_state);
            return Err(err.into());
        }
    };

    let meta = DeploymentMeta {
        schema: META_SCHEMA,
        release: release.to_string(),
        manifest: hash,
        previous: current.clone(),
        created_at: now_unix(),
    };
    if let Err(err) = fs::write(temp.join(META), serde_json::to_vec_pretty(&meta)?) {
        let _ = fs::remove_dir_all(&temp);
        let _ = fs::remove_file(&temp_state);
        return Err(err.into());
    }

    // Move the state into place first: a crash here leaves an unreferenced
    // state file, which the next deploy overwrites. Moving the deployment
    // first would leave a deployment that can never be redeployed.
    let state_path = states.join(format!("{id}.json"));
    fs::rename(&temp_state, &state_path)?;
    if let Err(err) = fs::rename(&temp, &destination) {
        let _ = fs::remove_dir_all(&temp);
        let _ = fs::remove_file(&state_path);
        return Err(err.into());
    }

    switch(&root, "current", &id)?;
    match &current {
        Some(previous) => switch(&root, "previous", previous)?,
        None => remove_link(&root, "previous")?,
    }

    Ok(DeployReport {
        deployment: id,
        previous: current,
        reused: false,
        applied: Some(applied),
    })
}

/// Switch `current` back to the predecessor recorded by the active deployment.
pub fn rollback(root: &Path) -> Result<RollbackReport, DeployErr> {
    let root = root.canonicalize()?;
    let from = current_id(&root)?.ok_or_else(|| DeployErr::NoCurrent(root.clone()))?;
    let meta = read_meta(&root.join("deployments").join(&from))?;
    let to = match meta.previous {
        Some(previous) => previous,
        None => link_id(&root, "previous")?
            .filter(|previous| previous != &from)
            .ok_or(DeployErr::NoPrevious)?,
    };
    let target = root.join("deployments").join(&to);
    let target_meta = read_meta(&target)?;
    if target_meta.schema != META_SCHEMA {
        return Err(DeployErr::Conflict(to));
    }
    switch(&root, "current", &to)?;
    switch(&root, "previous", &from)?;
    Ok(RollbackReport { from, to })
}

/// Remove old deployments while keeping the active deployment, its rollback
/// target, and the newest `keep - protected` others.
///
/// Predecessor links in retained deployments are rewritten to skip removed
/// deployments, so rollback keeps working across a prune. Removed trees are
/// renamed to hidden tombstones before recursive deletion; a crash leaves an
/// ignored tombstone, never a partial visible deployment.
pub fn prune(root: &Path, keep: usize, dry_run: bool) -> Result<PruneReport, DeployErr> {
    if keep < 2 {
        return Err(DeployErr::InvalidPrune(
            "--keep must be at least 2 (current and previous)".to_string(),
        ));
    }
    let root = root.canonicalize()?;
    let deployments = root.join("deployments");
    if !deployments.is_dir() {
        return Err(DeployErr::NotADeployment(root));
    }
    let current = current_id(&root)?.ok_or_else(|| DeployErr::NoCurrent(root.clone()))?;
    let previous = link_id(&root, "previous")?.filter(|id| id != &current);

    let mut all = std::collections::BTreeMap::new();
    for entry in fs::read_dir(&deployments)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if id.starts_with('.') {
            if !dry_run {
                cleanup_tombstone(&entry.path());
            }
            continue;
        }
        let meta = match read_meta(&entry.path()) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        all.insert(id, meta);
    }
    if !all.contains_key(&current) {
        return Err(DeployErr::NotADeployment(root));
    }

    let mut kept: Vec<String> = Vec::new();
    kept.push(current);
    if let Some(previous) = &previous
        && all.contains_key(previous)
    {
        kept.push(previous.clone());
    }
    let mut candidates: Vec<(&String, &DeploymentMeta)> = all.iter().collect();
    candidates.sort_by(|(left_id, left), (right_id, right)| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right_id.cmp(left_id))
    });
    for (id, _) in candidates {
        if kept.len() >= keep {
            break;
        }
        if !kept.contains(id) {
            kept.push(id.clone());
        }
    }
    kept.sort();
    let removed: Vec<String> = all
        .keys()
        .filter(|id| !kept.contains(id))
        .cloned()
        .collect();

    let removed_set: std::collections::HashSet<&str> = removed.iter().map(String::as_str).collect();
    if !dry_run {
        for id in &kept {
            let Some(meta) = all.get(id) else {
                continue;
            };
            let mut next = meta.previous.clone();
            let mut seen = std::collections::HashSet::new();
            while let Some(previous) = &next {
                if !removed_set.contains(previous.as_str()) {
                    break;
                }
                if !seen.insert(previous.clone()) {
                    next = None;
                    break;
                }
                next = all.get(previous).and_then(|meta| meta.previous.clone());
            }
            if next != meta.previous {
                let mut meta = meta.clone();
                meta.previous = next;
                write_meta(&deployments.join(id), &meta)?;
            }
        }
    }

    let states = root.join("states");
    let mut bytes_removed = 0u64;
    for id in &removed {
        let path = deployments.join(id);
        bytes_removed += tree_bytes(&path)?;
        if dry_run {
            continue;
        }
        let tombstone = deployments.join(format!(".prune-{id}-{}", ticket()));
        fs::rename(&path, &tombstone)?;
        cleanup_tombstone(&tombstone);
        let state = states.join(format!("{id}.json"));
        match fs::remove_file(&state) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }

    Ok(PruneReport { kept, removed, bytes_removed, dry_run })
}

/// Every deployment in `root`, in deployment-id order.
pub fn list(root: &Path) -> Result<Vec<DeploymentInfo>, DeployErr> {
    let root = root.canonicalize()?;
    let deployments = root.join("deployments");
    if !deployments.is_dir() {
        return Err(DeployErr::NotADeployment(root));
    }
    let current = current_id(&root)?;
    let mut infos = Vec::new();
    for entry in fs::read_dir(&deployments)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if id.starts_with('.') {
            continue;
        }
        let meta = match read_meta(&entry.path()) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        infos.push(DeploymentInfo {
            current: current.as_deref() == Some(id.as_str()),
            id,
            release: meta.release,
            manifest: meta.manifest,
        });
    }
    infos.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(infos)
}

fn deployment_id(release: &str, hash: &str) -> String {
    let hex = hash.strip_prefix("blake3:").unwrap_or(hash);
    format!("{release}-{}", &hex[..12])
}

fn current_id(root: &Path) -> Result<Option<String>, DeployErr> {
    link_id(root, "current")
}

fn link_id(root: &Path, name: &str) -> Result<Option<String>, DeployErr> {
    match fs::symlink_metadata(root.join(name)) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = fs::read_link(root.join(name))?;
            let id = target
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| DeployErr::NotADeployment(root.to_path_buf()))?;
            Ok(Some(id.to_string()))
        }
        Ok(_) => Err(DeployErr::NotADeployment(root.to_path_buf())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn switch(root: &Path, name: &str, id: &str) -> Result<(), DeployErr> {
    let target = PathBuf::from("deployments").join(id);
    let temp = root.join(format!(".{name}.new-{}", ticket()));
    symlink(&target, &temp)?;
    fs::rename(&temp, root.join(name))?;
    Ok(())
}

fn remove_link(root: &Path, name: &str) -> Result<(), DeployErr> {
    match fs::remove_file(root.join(name)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn read_meta(deployment: &Path) -> Result<DeploymentMeta, DeployErr> {
    let path = deployment.join(META);
    let meta: DeploymentMeta = serde_json::from_slice(&fs::read(&path)?)?;
    if meta.schema != META_SCHEMA {
        return Err(DeployErr::Conflict(deployment.display().to_string()));
    }
    Ok(meta)
}

fn write_meta(deployment: &Path, meta: &DeploymentMeta) -> Result<(), DeployErr> {
    let path = deployment.join(META);
    let temp = deployment.join(format!(".{META}.tmp-{}", ticket()));
    fs::write(&temp, serde_json::to_vec_pretty(meta)?)?;
    fs::rename(&temp, &path)?;
    Ok(())
}

fn cleanup_tombstone(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn tree_bytes(path: &Path) -> io::Result<u64> {
    let mut total = 0;
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

fn ticket() -> String {
    let hi = rand::random::<u64>().to_le_bytes();
    let lo = rand::random::<u64>().to_le_bytes();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi);
    bytes[8..].copy_from_slice(&lo);
    bs58::encode(&bytes[..12]).into_string()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}
