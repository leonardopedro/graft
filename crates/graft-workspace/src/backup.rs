//! Off-site backup: a consistent catalog export plus one git bundle per code
//! checkpoint, written under a timestamped prefix in a separate store.
//!
//! The manifest is written last. A prefix without a manifest is an incomplete
//! backup; a prefix with one names every object and its blake3 hash, so a
//! restore can verify what it reads before trusting it.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, Read},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use blake3::Hasher as Blake3;
use graft::{
    core::{LogId, VolumeId},
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft},
};
use loro::{ExportMode, LoroDoc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ArchiveBackupConfig, ArchiveStore, BlobErr, BlobStorage, Blobs, MediaStore, Workspace,
    WorkspaceConfig,
    catalog::CatalogErr,
    code::{self, CodeErr},
};

const CHUNK: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum BackupErr {
    #[error("no backup target configured; add a [backup] section to workspace.toml")]
    NoBackup,

    #[error("workspace is not initialized; run `workspace init` first")]
    NotInitialized,

    #[error("catalog: {0}")]
    Catalog(#[from] CatalogErr),

    #[error("media: {0}")]
    Blob(#[from] BlobErr),

    #[error("code: {0}")]
    Code(#[from] CodeErr),

    #[error("oplog: {0}")]
    Oplog(#[from] crate::OplogErr),

    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("manifest: {0}")]
    Json(#[from] serde_json::Error),

    #[error("object storage: {0}")]
    Store(#[from] opendal::Error),

    #[error("loro: {0}")]
    Loro(String),

    #[error("blob {0:?} named by a code checkpoint is missing")]
    MissingBlob(String),

    #[error("backup object {key:?} hashed to {actual}, expected {expected}")]
    Corrupt {
        key: String,
        expected: String,
        actual: String,
    },

    #[error("backup prefix {0:?} is absolute or contains a traversal segment")]
    UnsafePrefix(String),

    #[error("refusing to restore over a non-empty workspace")]
    RestoreNotEmpty,

    #[error("retention must keep at least one backup")]
    InvalidRetention,

    #[error("archive backup: {0}")]
    Archive(String),

    #[error("restore: {0}")]
    Restore(String),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupObject {
    pub key: String,
    pub hash: String,
    pub size: u64,
}

/// One content-addressed blob, copied so a restore can rebuild the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobBackup {
    pub storage: String,
    pub locator: String,
    #[serde(flatten)]
    pub object: BackupObject,
}

/// One document's compacted snapshot and remaining oplog frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocBackup {
    pub doc_id: String,
    pub frames: i64,
    /// Ordered frame log, kept for replay and inspection.
    #[serde(flatten)]
    pub object: BackupObject,
    /// True when the snapshot plus log describe the whole document history.
    /// A compacted document is complete when its stored snapshot exists.
    #[serde(default = "default_true")]
    pub complete: bool,
    /// Materialized Loro snapshot; absent when the document is empty or its
    /// history has a gap.
    #[serde(default)]
    pub snapshot: Option<BackupObject>,
    /// Per-producer sequence covered by `snapshot`. Empty for a snapshot
    /// generated from the full frame log, which restore replays from 1.
    #[serde(default)]
    pub snapshot_frontier: Vec<(String, i64)>,
}

/// Encrypted CDC copy stored on the backup archive remote. `volume` and `log`
/// are the Graft ids an empty data directory needs before it can pull the
/// archive pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveBackup {
    pub volume: String,
    pub log: String,
    pub blobs: usize,
}

#[derive(Serialize, Deserialize)]
struct ArchiveRefFile {
    version: u32,
    volume: String,
    log: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveGcReport {
    /// CDC files no retained backup references.
    pub files_removed: usize,
    /// Page rewrite stats for the backup archive volume.
    pub stats: crate::GcStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionReport {
    /// Newest complete backup prefixes that were kept.
    pub kept: Vec<String>,
    /// Older or incomplete prefixes whose objects were deleted.
    pub pruned: Vec<String>,
    /// Set when the encrypted backup archive was rewritten after pruning.
    pub archive_gc: Option<ArchiveGcReport>,
}

#[derive(Debug, Clone)]
pub struct BackupReport {
    pub prefix: String,
    pub created_at: i64,
    pub catalog: BackupObject,
    pub code_bundles: Vec<BackupObject>,
    pub blobs: Vec<BlobBackup>,
    pub skipped_cdc: usize,
    pub archive: Option<ArchiveBackup>,
    pub doc_logs: Vec<DocBackup>,
    pub manifest: BackupObject,
}

#[derive(Serialize)]
struct Manifest<'a> {
    version: u32,
    workspace: &'a str,
    created_at: i64,
    catalog: &'a BackupObject,
    code_bundles: &'a [BackupObject],
    blobs: &'a [BlobBackup],
    skipped_cdc: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive: Option<&'a ArchiveBackup>,
    doc_logs: &'a [DocBackup],
}

#[derive(Serialize, Deserialize)]
struct FrameLine {
    peer: String,
    seq: i64,
    /// Frame bytes as lowercase hex, so the JSONL stays text and diffable.
    data: String,
}

#[derive(Deserialize)]
struct ManifestFile {
    version: u32,
    workspace: String,
    catalog: BackupObject,
    #[serde(default)]
    blobs: Vec<BlobBackup>,
    #[serde(default)]
    archive: Option<ArchiveBackup>,
    #[serde(default)]
    doc_logs: Vec<DocBackup>,
}

#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub prefix: String,
    pub workspace: String,
    pub entries: i64,
    pub blobs: usize,
    pub docs: usize,
    /// Catalog rows whose bytes the backup did not carry (old backups).
    pub missing_blobs: i64,
    /// Archive blobs the backup intentionally did not decrypt.
    pub skipped_cdc: usize,
    /// Documents whose frames began after a compacted prefix; they were not replayed.
    pub partial_docs: usize,
}

/// Write a backup to the configured target. `prefix_override` replaces the
/// configured base prefix but keeps the workspace and timestamp components.
pub fn run(
    workspace: &Workspace,
    config: &WorkspaceConfig,
    prefix_override: Option<&str>,
) -> Result<BackupReport, BackupErr> {
    let backup = config.backup.as_ref().ok_or(BackupErr::NoBackup)?;
    let store = MediaStore::open(&backup.store)?;
    let workspace_id = workspace
        .catalog()
        .workspace_id()
        .ok_or(BackupErr::NotInitialized)?;
    let created_at = now_unix();
    let base = prefix_override.unwrap_or(&backup.prefix).trim_matches('/');
    validate_key(base)?;
    let stamp = format!("{created_at}-{}", std::process::id());
    let workspace_prefix = if base.is_empty() {
        workspace_id.to_string()
    } else {
        format!("{base}/{workspace_id}")
    };
    let prefix = format!("{workspace_prefix}/{stamp}");
    let archive_ref_key = format!("{workspace_prefix}/archive.json");

    let export = env::temp_dir().join(format!(
        "graft-workspace-backup-{}-{created_at}-{}.db",
        std::process::id(),
        workspace_id
    ));
    workspace.catalog().export_to(&export)?;
    let catalog = match upload_file(&store, &format!("{prefix}/catalog.db"), &export) {
        Ok(object) => object,
        Err(err) => {
            let _ = fs::remove_file(&export);
            return Err(err);
        }
    };
    let _ = fs::remove_file(&export);

    let entries = workspace.catalog().entry_list("", true)?;
    let trees: BTreeSet<String> = entries
        .into_iter()
        .filter_map(|entry| entry.code_tree)
        .collect();
    let mut seen = BTreeSet::new();
    let mut code_bundles = Vec::new();
    if !trees.is_empty() {
        let blobs = workspace.blobs()?;
        for tree in trees {
            let Some(hash) = code::git_bundle_hash(&blobs, &tree)? else {
                continue;
            };
            if !seen.insert(hash.clone()) {
                continue;
            }
            let blob = workspace
                .catalog()
                .blob_get(&hash)?
                .ok_or_else(|| BackupErr::MissingBlob(hash.clone()))?;
            let digest = hash.strip_prefix("blake3:").unwrap_or(&hash);
            let key = format!("{prefix}/code/{digest}.bundle");
            let object = upload_blob(&store, &blobs, &hash, blob.size as u64, &key)?;
            code_bundles.push(object);
        }
    }

    let mut blob_backups = Vec::new();
    let mut skipped_cdc = 0usize;
    let mut archive_backup = None;
    if workspace.media().is_some() || workspace.archive().is_some() {
        let blobs = workspace.blobs()?;
        let catalog_blobs = workspace.catalog().blob_list()?;
        let cdc_blobs: Vec<_> = catalog_blobs
            .iter()
            .filter(|blob| blob.storage == BlobStorage::Cdc)
            .collect();

        if !cdc_blobs.is_empty() {
            match config
                .backup
                .as_ref()
                .and_then(|backup| backup.archive.as_ref())
            {
                Some(archive_config) => {
                    let source = workspace.archive().ok_or_else(|| {
                        BackupErr::Archive(
                            "catalog has cdc blobs but no [archive] key is configured".to_string(),
                        )
                    })?;
                    let (runtime, target) = open_archive_backup(
                        config,
                        archive_config,
                        workspace_id,
                        &store,
                        &archive_ref_key,
                    )?;
                    for blob in &cdc_blobs {
                        validate_cdc_locator(&blob.locator, workspace_id)?;
                        source.copy_to_verified(
                            &blob.locator,
                            &target,
                            &blob.locator,
                            blob.size as u64,
                            &blob.hash,
                        )?;
                    }
                    runtime
                        .volume_push(target.volume_id())
                        .map_err(|err| BackupErr::Archive(format!("push archive: {err}")))?;
                    let info = runtime
                        .volume_get(&target.volume_id())
                        .map_err(|err| BackupErr::Archive(format!("read archive ref: {err}")))?;
                    let reference = ArchiveRefFile {
                        version: 1,
                        volume: info.vid.serialize(),
                        log: info.remote.serialize(),
                    };
                    store.write_bytes(&archive_ref_key, &serde_json::to_vec(&reference)?)?;
                    archive_backup = Some(ArchiveBackup {
                        volume: reference.volume,
                        log: reference.log,
                        blobs: cdc_blobs.len(),
                    });
                }
                None => skipped_cdc = cdc_blobs.len(),
            }
        }

        for blob in &catalog_blobs {
            // Archive blobs are copied only through the encrypted backup
            // archive. Never put their plaintext in the object backup store.
            if blob.storage == BlobStorage::Cdc {
                continue;
            }
            let digest = blob.hash.strip_prefix("blake3:").unwrap_or(&blob.hash);
            let key = format!("{prefix}/blobs/{digest}.bin");
            let object = upload_blob(&store, &blobs, &blob.hash, blob.size as u64, &key)?;
            blob_backups.push(BlobBackup {
                storage: blob.storage.as_str().to_string(),
                locator: blob.locator.clone(),
                object,
            });
        }
    }

    let mut doc_logs = Vec::new();
    for doc in workspace.oplog().docs()? {
        let stored = workspace.oplog().snapshot(&doc.doc_id)?;
        let mut jsonl = String::new();
        let loro = LoroDoc::new();
        let mut expected: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        if let Some(snapshot) = &stored {
            loro.import(&snapshot.data)
                .map_err(|err| BackupErr::Loro(err.to_string()))?;
            for (peer, seq) in &snapshot.frontier {
                expected.insert(peer.clone(), seq + 1);
            }
        }
        let mut complete = true;
        let mut applied = 0usize;
        for frame in workspace.oplog().frames_since(&doc.doc_id, &[])? {
            let next = expected.entry(frame.peer.clone()).or_insert(1);
            if frame.seq < *next {
                // Already covered by the stored snapshot.
                continue;
            }
            if frame.seq != *next {
                complete = false;
            }
            *next = frame.seq + 1;
            let line = FrameLine {
                peer: frame.peer,
                seq: frame.seq,
                data: hex(&frame.data),
            };
            jsonl.push_str(&serde_json::to_string(&line)?);
            jsonl.push('\n');
            loro.import(&frame.data)
                .map_err(|err| BackupErr::Loro(err.to_string()))?;
            applied += 1;
        }
        let name = safe_name(&doc.doc_id);
        let key = format!("{prefix}/docs/{name}.jsonl");
        store.write_bytes(&key, jsonl.as_bytes())?;

        let (snapshot, snapshot_frontier) = if complete {
            let bytes = if let Some(snapshot) = &stored {
                Some(snapshot.data.clone())
            } else if applied > 0 {
                Some(
                    loro.export(ExportMode::Snapshot)
                        .map_err(|err| BackupErr::Loro(err.to_string()))?,
                )
            } else {
                None
            };
            match bytes {
                Some(bytes) => {
                    let snapshot_key = format!("{prefix}/docs/{name}.snapshot");
                    store.write_bytes(&snapshot_key, &bytes)?;
                    let object = BackupObject {
                        key: snapshot_key,
                        hash: format!("blake3:{}", blake3::hash(&bytes).to_hex()),
                        size: bytes.len() as u64,
                    };
                    let frontier = stored
                        .as_ref()
                        .map(|snapshot| snapshot.frontier.clone())
                        .unwrap_or_default();
                    (Some(object), frontier)
                }
                None => (None, Vec::new()),
            }
        } else {
            (None, Vec::new())
        };
        doc_logs.push(DocBackup {
            doc_id: doc.doc_id,
            frames: applied as i64,
            complete,
            object: BackupObject {
                key,
                hash: format!("blake3:{}", blake3::hash(jsonl.as_bytes()).to_hex()),
                size: jsonl.len() as u64,
            },
            snapshot,
            snapshot_frontier,
        });
    }

    let manifest = Manifest {
        version: 1,
        workspace: workspace_id,
        created_at,
        catalog: &catalog,
        code_bundles: &code_bundles,
        blobs: &blob_backups,
        skipped_cdc,
        archive: archive_backup.as_ref(),
        doc_logs: &doc_logs,
    };
    let data = serde_json::to_vec_pretty(&manifest)?;
    let key = format!("{prefix}/manifest.json");
    store.write_bytes(&key, &data)?;
    let manifest = BackupObject {
        key,
        hash: format!("blake3:{}", blake3::hash(&data).to_hex()),
        size: data.len() as u64,
    };

    Ok(BackupReport {
        prefix,
        created_at,
        catalog,
        code_bundles,
        blobs: blob_backups,
        skipped_cdc,
        archive: archive_backup,
        doc_logs,
        manifest,
    })
}

/// Apply retention to this workspace using the configured backup target.
/// `prefix_override` matches the base prefix used for the backup run.
pub fn prune(
    workspace: &Workspace,
    config: &WorkspaceConfig,
    prefix_override: Option<&str>,
    keep: usize,
) -> Result<RetentionReport, BackupErr> {
    let backup = config.backup.as_ref().ok_or(BackupErr::NoBackup)?;
    let store = MediaStore::open(&backup.store)?;
    let workspace_id = workspace
        .catalog()
        .workspace_id()
        .ok_or(BackupErr::NotInitialized)?;
    let base = prefix_override.unwrap_or(&backup.prefix);
    let mut report = prune_prefix(&store, base, workspace_id, keep)?;
    report.archive_gc = gc_archive_backup(config, base, workspace_id, &report.kept)?;
    Ok(report)
}

/// Keep the newest `keep` complete backups under the configured target,
/// deleting every older backup and any incomplete run.
///
/// Backup keys are `<base>/<workspace>/<unix>-<pid>/…` and the manifest is
/// written last, so a run without `manifest.json` never counts as a backup.
/// Only valid timestamp-prefixed runs are touched; unrelated objects under the
/// same workspace prefix are left alone.
pub fn prune_prefix(
    store: &MediaStore,
    base: &str,
    workspace: &str,
    keep: usize,
) -> Result<RetentionReport, BackupErr> {
    if keep == 0 {
        return Err(BackupErr::InvalidRetention);
    }
    let base = base.trim_matches('/');
    validate_key(base)?;
    validate_key(workspace)?;
    let prefix = if base.is_empty() {
        format!("{workspace}/")
    } else {
        format!("{base}/{workspace}/")
    };

    struct Run {
        created_at: i64,
        complete: bool,
        keys: Vec<String>,
    }

    let mut runs: BTreeMap<String, Run> = BTreeMap::new();
    for object in store.list(&prefix)? {
        let Some(rest) = object.key.strip_prefix(&prefix) else {
            continue;
        };
        let Some((run, name)) = rest.split_once('/') else {
            continue;
        };
        let Some(created_at) = run
            .split_once('-')
            .and_then(|(stamp, pid)| stamp.parse::<i64>().ok().map(|stamp| (stamp, pid)))
            .filter(|(stamp, pid)| *stamp > 0 && !pid.is_empty())
            .map(|(stamp, _)| stamp)
        else {
            continue;
        };
        let entry = runs.entry(run.to_string()).or_insert_with(|| Run {
            created_at,
            complete: false,
            keys: Vec::new(),
        });
        if name == "manifest.json" {
            entry.complete = true;
        }
        entry.keys.push(object.key);
    }

    let mut complete: Vec<(&String, &Run)> = runs.iter().filter(|(_, run)| run.complete).collect();
    complete.sort_by(|(left_name, left), (right_name, right)| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right_name.cmp(left_name))
    });
    let kept: BTreeSet<&str> = complete
        .iter()
        .take(keep)
        .map(|(name, _)| name.as_str())
        .collect();

    let mut report = RetentionReport {
        kept: complete
            .iter()
            .take(keep)
            .map(|(name, _)| format!("{prefix}{name}"))
            .collect(),
        pruned: Vec::new(),
        archive_gc: None,
    };
    for (name, run) in &runs {
        if kept.contains(name.as_str()) {
            continue;
        }
        for key in &run.keys {
            store.delete(key)?;
        }
        report.pruned.push(format!("{prefix}{name}"));
    }
    Ok(report)
}

/// Rewrite the encrypted backup archive after retention. Liveness is the
/// union of CDC locators in every retained backup's catalog, so a pruned run's
/// blobs are removed while older kept runs can still restore.
fn gc_archive_backup(
    config: &WorkspaceConfig,
    base: &str,
    workspace_id: &str,
    kept: &[String],
) -> Result<Option<ArchiveGcReport>, BackupErr> {
    let backup = config.backup.as_ref().ok_or(BackupErr::NoBackup)?;
    let Some(archive_config) = backup.archive.as_ref() else {
        return Ok(None);
    };
    if kept.is_empty() {
        return Ok(None);
    }
    let store = MediaStore::open(&backup.store)?;
    let base = base.trim_matches('/');
    validate_key(base)?;
    validate_key(workspace_id)?;
    let workspace_prefix = if base.is_empty() {
        workspace_id.to_string()
    } else {
        format!("{base}/{workspace_id}")
    };
    let archive_ref_key = format!("{workspace_prefix}/archive.json");
    if store.stat(&archive_ref_key)?.is_none() {
        return Ok(None);
    }

    let temp = env::temp_dir().join(format!(
        "graft-workspace-archive-gc-{}-{}",
        std::process::id(),
        now_unix()
    ));
    fs::create_dir_all(&temp)?;
    let result = (|| {
        let mut live = BTreeSet::new();
        let catalog_path = temp.join("catalog.db");
        for prefix in kept {
            let key = format!("{prefix}/manifest.json");
            let data = store.read_range(&key, 0, None)?;
            let manifest: ManifestFile = serde_json::from_slice(&data)?;
            if manifest.version != 1 {
                return Err(BackupErr::Restore(format!(
                    "unsupported manifest version {}",
                    manifest.version
                )));
            }
            if manifest.workspace != workspace_id {
                return Err(BackupErr::Restore(format!(
                    "manifest workspace {:?} does not match {workspace_id:?}",
                    manifest.workspace
                )));
            }
            fetch(&store, prefix, &manifest.catalog, &catalog_path)?;
            let conn = rusqlite::Connection::open(&catalog_path)?;
            let mut statement = conn.prepare("SELECT locator FROM blobs WHERE storage = 'cdc'")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                let locator = row?;
                validate_cdc_locator(&locator, workspace_id)?;
                live.insert(locator);
            }
            let _ = fs::remove_file(&catalog_path);
        }

        let (runtime, target) = open_archive_backup(
            config,
            archive_config,
            workspace_id,
            &store,
            &archive_ref_key,
        )?;
        let prefix = format!("media/cdc/{workspace_id}/");
        let mut files_removed = 0usize;
        for path in target.list(&prefix)? {
            if !live.contains(&path) {
                target.delete(&path)?;
                files_removed += 1;
            }
        }
        if files_removed == 0 {
            return Ok(None);
        }

        let tag = format!("ws-backup-{workspace_id}");
        let archive = config.archive.as_ref().ok_or_else(|| {
            BackupErr::Archive("archive GC needs the workspace [archive] key".to_string())
        })?;
        let (stats, target) = target.gc(&runtime, &tag, archive)?;
        runtime
            .volume_push(target.volume_id())
            .map_err(|err| BackupErr::Archive(format!("push collected archive: {err}")))?;
        let info = runtime
            .volume_get(&target.volume_id())
            .map_err(|err| BackupErr::Archive(format!("read collected archive ref: {err}")))?;
        let reference = ArchiveRefFile {
            version: 1,
            volume: info.vid.serialize(),
            log: info.remote.serialize(),
        };
        store.write_bytes(&archive_ref_key, &serde_json::to_vec(&reference)?)?;
        Ok(Some(ArchiveGcReport { files_removed, stats }))
    })();
    let _ = fs::remove_dir_all(&temp);
    result
}

fn open_archive_backup(
    config: &WorkspaceConfig,
    backup: &ArchiveBackupConfig,
    workspace_id: &str,
    store: &MediaStore,
    archive_ref_key: &str,
) -> Result<(Runtime, ArchiveStore), BackupErr> {
    let archive_config = config.archive.as_ref().ok_or_else(|| {
        BackupErr::Archive("archive backup needs the workspace [archive] key".to_string())
    })?;
    fs::create_dir_all(&backup.data_dir).map_err(|err| {
        BackupErr::Archive(format!(
            "create archive backup data directory {}: {err}",
            backup.data_dir.display()
        ))
    })?;
    let runtime = setup_graft(GraftConfig {
        remote: backup.remote.clone(),
        data_dir: backup.data_dir.clone(),
        autosync: None,
    })
    .map_err(|err| BackupErr::Archive(format!("open backup archive remote: {err}")))?;
    let tag = format!("ws-backup-{workspace_id}");

    // The local backup data directory is the writer for this volume. If its
    // tag exists, a previous failed run may have local commits that were never
    // pushed; do not replace them with an older remote head. The next
    // successful pass pushes everything.
    let has_local = runtime
        .tag_exists(&tag)
        .map_err(|err| BackupErr::Archive(format!("read backup archive tag: {err}")))?;
    if !has_local
        && store
            .stat(archive_ref_key)
            .map_err(|err| BackupErr::Archive(format!("read archive backup reference: {err}")))?
            .is_some()
    {
        let data = store
            .read_range(archive_ref_key, 0, None)
            .map_err(|err| BackupErr::Archive(format!("read archive backup reference: {err}")))?;
        let reference: ArchiveRefFile = serde_json::from_slice(&data)?;
        if reference.version != 1 {
            return Err(BackupErr::Archive(format!(
                "unsupported archive reference version {}",
                reference.version
            )));
        }
        let volume: VolumeId = reference
            .volume
            .parse()
            .map_err(|err| BackupErr::Archive(format!("bad archive backup volume id: {err}")))?;
        let log: LogId = reference
            .log
            .parse()
            .map_err(|err| BackupErr::Archive(format!("bad archive backup log id: {err}")))?;
        runtime
            .volume_open(Some(volume.clone()), None, Some(log))
            .map_err(|err| BackupErr::Archive(format!("attach backup archive volume: {err}")))?;
        runtime
            .tag_replace(&tag, volume.clone())
            .map_err(|err| BackupErr::Archive(format!("tag backup archive volume: {err}")))?;
        runtime
            .volume_pull(volume)
            .map_err(|err| BackupErr::Archive(format!("pull backup archive: {err}")))?;
    }

    let archive =
        ArchiveStore::open(runtime.clone(), &tag, archive_config).map_err(BackupErr::Blob)?;
    Ok((runtime, archive))
}

/// Rebuild an empty workspace from one backup prefix.
///
/// The catalog is restored first, then every backed-up blob is streamed into
/// the matching store at its original locator, then document snapshots are
/// imported and newer frames are re-appended. The caller must point at a fresh
/// data directory: a non-empty catalog is refused before anything is changed.
pub fn restore(
    workspace: &mut Workspace,
    config: &WorkspaceConfig,
    prefix: &str,
) -> Result<RestoreReport, BackupErr> {
    let backup = config.backup.as_ref().ok_or(BackupErr::NoBackup)?;
    let store = MediaStore::open(&backup.store)?;
    validate_key(prefix)?;
    if prefix.is_empty() {
        return Err(BackupErr::UnsafePrefix(prefix.to_string()));
    }
    let status = workspace.catalog().status()?;
    if status.entries != 0 || status.blobs != 0 || status.releases != 0 {
        return Err(BackupErr::RestoreNotEmpty);
    }

    let key = format!("{prefix}/manifest.json");
    let data = store.read_range(&key, 0, None)?;
    let manifest: ManifestFile = serde_json::from_slice(&data)?;
    if manifest.version != 1 {
        return Err(BackupErr::Restore(format!(
            "unsupported manifest version {}",
            manifest.version
        )));
    }
    validate_key(&manifest.workspace)?;
    if manifest.workspace.is_empty() {
        return Err(BackupErr::Restore(
            "manifest has no workspace id".to_string(),
        ));
    }

    let temp = env::temp_dir().join(format!(
        "graft-workspace-restore-{}-{}",
        std::process::id(),
        now_unix()
    ));
    fs::create_dir_all(&temp)?;
    let result = restore_inner(workspace, config, &store, prefix, &manifest, &temp);
    let _ = fs::remove_dir_all(&temp);
    result
}

fn restore_inner(
    workspace: &mut Workspace,
    config: &WorkspaceConfig,
    store: &MediaStore,
    prefix: &str,
    manifest: &ManifestFile,
    temp: &Path,
) -> Result<RestoreReport, BackupErr> {
    let catalog_path = temp.join("catalog.db");
    fetch(store, prefix, &manifest.catalog, &catalog_path)?;
    workspace.restore_catalog(&catalog_path)?;

    for blob in &manifest.blobs {
        let path = temp.join("blob.bin");
        fetch(store, prefix, &blob.object, &path)?;
        let actual = hash_file(&path)?;
        if actual != blob.object.hash {
            return Err(BackupErr::Corrupt {
                key: blob.object.key.clone(),
                expected: blob.object.hash.clone(),
                actual,
            });
        }
        match blob.storage.as_str() {
            "raw" => {
                let expected = format!("media/raw/{}/", manifest.workspace);
                validate_key(&blob.locator)?;
                if !blob.locator.starts_with(&expected) {
                    return Err(BackupErr::Restore(format!(
                        "raw locator {:?} is outside {expected}",
                        blob.locator
                    )));
                }
                let media = workspace.media().ok_or(BlobErr::NoMediaStore)?;
                media.put_file(&blob.locator, &path)?;
            }
            "cdc" => {
                let expected = format!("media/cdc/{}/", manifest.workspace);
                validate_key(&blob.locator)?;
                if !blob.locator.starts_with(&expected) {
                    return Err(BackupErr::Restore(format!(
                        "cdc locator {:?} is outside {expected}",
                        blob.locator
                    )));
                }
                let archive = workspace.archive().ok_or(BlobErr::NoArchiveStore)?;
                let file = fs::File::open(&path)?;
                archive.put(&blob.locator, file)?;
            }
            other => {
                return Err(BackupErr::Restore(format!(
                    "unknown storage class {other:?}"
                )));
            }
        }
        let _ = fs::remove_file(&path);
    }

    let cdc_blobs: Vec<_> = workspace
        .catalog()
        .blob_list()?
        .into_iter()
        .filter(|blob| blob.storage == BlobStorage::Cdc)
        .collect();
    let mut restored_cdc = 0usize;
    if let Some(reference) = &manifest.archive {
        if reference.blobs != cdc_blobs.len() {
            return Err(BackupErr::Restore(format!(
                "archive manifest lists {} cdc blobs but the catalog has {}",
                reference.blobs,
                cdc_blobs.len()
            )));
        }
        let backup_config = config.backup.as_ref().ok_or(BackupErr::NoBackup)?;
        let archive_config = backup_config.archive.as_ref().ok_or_else(|| {
            BackupErr::Archive(
                "this backup has an encrypted archive; add [backup.archive] to restore it"
                    .to_string(),
            )
        })?;
        let source = open_archive_from_ref(config, archive_config, &manifest.workspace, reference)?;
        let target = workspace.archive().ok_or(BlobErr::NoArchiveStore)?;
        for blob in &cdc_blobs {
            validate_cdc_locator(&blob.locator, &manifest.workspace)?;
            source.copy_to_verified(
                &blob.locator,
                target,
                &blob.locator,
                blob.size as u64,
                &blob.hash,
            )?;
            restored_cdc += 1;
        }
    }

    let mut partial_docs = 0usize;
    for doc in &manifest.doc_logs {
        if !doc.complete {
            partial_docs += 1;
            continue;
        }
        if let Some(snapshot) = &doc.snapshot {
            let snapshot_path = temp.join("doc.snapshot");
            fetch(store, prefix, snapshot, &snapshot_path)?;
            let bytes = fs::read(&snapshot_path)?;
            LoroDoc::new()
                .import(&bytes)
                .map_err(|err| BackupErr::Loro(format!("{}: {err}", doc.doc_id)))?;
            if !doc.snapshot_frontier.is_empty() {
                workspace
                    .oplog()
                    .put_snapshot(&doc.doc_id, &doc.snapshot_frontier, &bytes)?;
            }
            let _ = fs::remove_file(&snapshot_path);
        }
        let path = temp.join("doc.jsonl");
        fetch(store, prefix, &doc.object, &path)?;
        let data = fs::read(&path)?;
        for line in data.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let frame: FrameLine = serde_json::from_slice(line)?;
            let data = unhex(&frame.data)?;
            match workspace
                .oplog()
                .append(&doc.doc_id, &frame.peer, frame.seq, &data)?
            {
                crate::AppendOutcome::Applied { .. } => {}
                crate::AppendOutcome::Duplicate { .. } => {}
                crate::AppendOutcome::Gap { expected, .. } => {
                    return Err(BackupErr::Restore(format!(
                        "doc {:?} has a frame gap before {expected}",
                        doc.doc_id
                    )));
                }
            }
        }
        let _ = fs::remove_file(&path);
    }

    let status = workspace.catalog().status()?;
    let backed_up = manifest.blobs.len() as i64;
    let skipped_cdc = cdc_blobs.len().saturating_sub(restored_cdc);
    Ok(RestoreReport {
        prefix: prefix.to_string(),
        workspace: manifest.workspace.clone(),
        entries: status.entries,
        blobs: manifest.blobs.len(),
        docs: manifest.doc_logs.len(),
        missing_blobs: status
            .blobs
            .saturating_sub(backed_up)
            .saturating_sub(restored_cdc as i64),
        skipped_cdc,
        partial_docs,
    })
}

fn open_archive_from_ref(
    config: &WorkspaceConfig,
    backup: &ArchiveBackupConfig,
    workspace_id: &str,
    reference: &ArchiveBackup,
) -> Result<ArchiveStore, BackupErr> {
    let archive_config = config.archive.as_ref().ok_or_else(|| {
        BackupErr::Archive("archive restore needs the workspace [archive] key".to_string())
    })?;
    fs::create_dir_all(&backup.data_dir).map_err(|err| {
        BackupErr::Archive(format!(
            "create archive backup data directory {}: {err}",
            backup.data_dir.display()
        ))
    })?;
    let runtime = setup_graft(GraftConfig {
        remote: backup.remote.clone(),
        data_dir: backup.data_dir.clone(),
        autosync: None,
    })
    .map_err(|err| BackupErr::Archive(format!("open backup archive remote: {err}")))?;
    let volume: VolumeId = reference
        .volume
        .parse()
        .map_err(|err| BackupErr::Archive(format!("bad archive volume id: {err}")))?;
    let log: LogId = reference
        .log
        .parse()
        .map_err(|err| BackupErr::Archive(format!("bad archive log id: {err}")))?;
    runtime
        .volume_open(Some(volume.clone()), None, Some(log))
        .map_err(|err| BackupErr::Archive(format!("attach backup archive volume: {err}")))?;
    let tag = format!("ws-restore-{workspace_id}");
    runtime
        .tag_replace(&tag, volume.clone())
        .map_err(|err| BackupErr::Archive(format!("tag backup archive volume: {err}")))?;
    runtime
        .volume_pull(volume)
        .map_err(|err| BackupErr::Archive(format!("pull backup archive: {err}")))?;
    ArchiveStore::open(runtime, &tag, archive_config).map_err(BackupErr::Blob)
}

fn validate_cdc_locator(locator: &str, workspace: &str) -> Result<(), BackupErr> {
    validate_key(locator)?;
    let expected = format!("media/cdc/{workspace}/");
    if !locator.starts_with(&expected) {
        return Err(BackupErr::Archive(format!(
            "cdc locator {locator:?} is outside {expected}"
        )));
    }
    Ok(())
}

fn fetch(
    store: &MediaStore,
    prefix: &str,
    object: &BackupObject,
    path: &Path,
) -> Result<(), BackupErr> {
    let expected = format!("{prefix}/");
    if !object.key.starts_with(&expected) {
        return Err(BackupErr::Restore(format!(
            "object {:?} is outside backup {prefix:?}",
            object.key
        )));
    }
    validate_key(&object.key)?;
    let size = store.download(&object.key, path)?;
    if size != object.size {
        return Err(BackupErr::Corrupt {
            key: object.key.clone(),
            expected: format!("{} bytes", object.size),
            actual: format!("{size} bytes"),
        });
    }
    let actual = hash_file(path)?;
    if actual != object.hash {
        return Err(BackupErr::Corrupt {
            key: object.key.clone(),
            expected: object.hash.clone(),
            actual,
        });
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, BackupErr> {
    let mut file = fs::File::open(path)?;
    let mut blake = Blake3::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        blake.update(&buf[..read]);
    }
    Ok(format!("blake3:{}", blake.finalize().to_hex()))
}

fn unhex(text: &str) -> Result<Vec<u8>, BackupErr> {
    if !text.len().is_multiple_of(2) {
        return Err(BackupErr::Restore("odd-length frame hex".to_string()));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(pair).map_err(|err| BackupErr::Restore(err.to_string()))?;
        out.push(u8::from_str_radix(pair, 16).map_err(|err| BackupErr::Restore(err.to_string()))?);
    }
    Ok(out)
}

fn upload_file(store: &MediaStore, key: &str, path: &Path) -> Result<BackupObject, BackupErr> {
    let mut file = fs::File::open(path)?;
    let mut writer = store.writer(key)?;
    let mut blake = Blake3::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        blake.update(&buf[..read]);
        size += read as u64;
        writer.write(buf[..read].to_vec())?;
    }
    writer.close()?;
    Ok(BackupObject {
        key: key.to_string(),
        hash: format!("blake3:{}", blake.finalize().to_hex()),
        size,
    })
}

fn upload_blob(
    store: &MediaStore,
    blobs: &Blobs<'_>,
    hash: &str,
    size: u64,
    key: &str,
) -> Result<BackupObject, BackupErr> {
    let result = (|| {
        let mut writer = store.writer(key)?;
        let mut blake = Blake3::new();
        let mut offset = 0u64;
        while offset < size {
            let len = (CHUNK as u64).min(size - offset);
            let data = blobs.read_range(hash, offset, Some(len))?;
            if data.is_empty() {
                break;
            }
            blake.update(&data);
            offset += data.len() as u64;
            writer.write(data)?;
        }
        writer.close()?;
        let actual = format!("blake3:{}", blake.finalize().to_hex());
        if actual != hash || offset != size {
            return Err(BackupErr::Corrupt {
                key: key.to_string(),
                expected: hash.to_string(),
                actual,
            });
        }
        Ok(BackupObject { key: key.to_string(), hash: actual, size })
    })();
    if result.is_err() {
        let _ = store.delete(key);
    }
    result
}

/// Document ids allow `.` and `:`, so encode anything outside a conservative
/// filename alphabet before using one as a key segment.
fn safe_name(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' {
            out.push(byte as char);
        } else {
            out.push_str(&format!("_{byte:02x}"));
        }
    }
    out
}

fn hex(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Store keys are untrusted: reject absolute paths, backslashes, NULs and
/// traversal segments before composing one from config or a manifest.
fn validate_key(key: &str) -> Result<(), BackupErr> {
    if key.is_empty() {
        return Ok(());
    }
    if key.starts_with('/') || key.contains('\\') || key.contains('\0') {
        return Err(BackupErr::UnsafePrefix(key.to_string()));
    }
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(BackupErr::UnsafePrefix(key.to_string()));
        }
    }
    Ok(())
}

fn default_true() -> bool {
    true
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MediaConfig;

    #[test]
    fn cdc_locators_must_stay_under_the_workspace_prefix() {
        assert!(validate_cdc_locator("media/cdc/ws-1/blob", "ws-1").is_ok());
        for locator in [
            "media/cdc/ws-2/blob",
            "media/cdc/ws-1/../ws-2/blob",
            "media/cdc/ws-1//blob",
            "/media/cdc/ws-1/blob",
            "media\\cdc\\ws-1\\blob",
            "media/cdc/ws-1/blob\0",
        ] {
            assert!(
                matches!(
                    validate_cdc_locator(locator, "ws-1"),
                    Err(BackupErr::Archive(_) | BackupErr::UnsafePrefix(_))
                ),
                "{locator:?} must be rejected"
            );
        }
    }

    #[test]
    fn retention_keeps_newest_complete_backups_and_removes_incomplete_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            MediaStore::open(&MediaConfig::Fs { root: dir.path().display().to_string() }).unwrap();

        for (run, complete) in [
            ("100-1", true),
            ("200-2", true),
            ("300-3", true),
            ("400-4", false),
        ] {
            store
                .write_bytes(&format!("backups/ws/{run}/catalog.db"), b"catalog")
                .unwrap();
            if complete {
                store
                    .write_bytes(&format!("backups/ws/{run}/manifest.json"), b"{}")
                    .unwrap();
            }
        }
        store
            .write_bytes("backups/ws/not-a-run/manifest.json", b"{}")
            .unwrap();

        let report = prune_prefix(&store, "backups", "ws", 2).unwrap();
        assert_eq!(
            report.kept,
            vec![
                "backups/ws/300-3".to_string(),
                "backups/ws/200-2".to_string()
            ]
        );
        assert_eq!(
            report.pruned,
            vec![
                "backups/ws/100-1".to_string(),
                "backups/ws/400-4".to_string()
            ]
        );

        assert!(
            store
                .stat("backups/ws/300-3/manifest.json")
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .stat("backups/ws/200-2/manifest.json")
                .unwrap()
                .is_some()
        );
        assert!(store.stat("backups/ws/100-1/catalog.db").unwrap().is_none());
        assert!(store.stat("backups/ws/400-4/catalog.db").unwrap().is_none());
        assert!(
            store
                .stat("backups/ws/not-a-run/manifest.json")
                .unwrap()
                .is_some(),
            "unrelated objects under the workspace prefix are left alone"
        );

        assert!(matches!(
            prune_prefix(&store, "backups", "ws", 0),
            Err(BackupErr::InvalidRetention)
        ));
    }
}
