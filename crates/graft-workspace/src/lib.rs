//! Workspace storage on Graft.
//!
//! Two Graft volumes make up a workspace:
//!
//! * the **catalog** (`catalog_tag`, default `ws-catalog`), a SQLite index of
//!   entries, blobs and releases;
//! * the **oplog** (`<catalog_tag>-oplog`), the sequenced frame log for the
//!   live-document lane.
//!
//! They are separate because they have different traffic: catalog writes are
//! sparse and index-shaped, oplog writes and acks arrive per edit. Keeping the
//! oplog out of the catalog volume keeps edit traffic from contending with
//! catalog commits and from syncing catalog snapshots on every keystroke.
//!
//! [`Workspace`] is the coordinator: it persists frames, hands reconnecting
//! clients the tail after their cursors, records acks, and notes compacted
//! Loro frontiers on catalog entries.
//!
//! A second machine adopts a workspace with [`Workspace::adopt`], which needs a
//! [`WorkspaceRef`]: both volume ids *and* both remote log ids, because a local
//! Graft index with no record of a volume would otherwise mint a new remote log
//! and see an empty workspace. `workspace share` prints that reference and
//! `workspace clone` consumes it; [`Workspace::pull`] fetches later commits.

pub mod archive;
pub mod backup;
pub mod catalog;
pub mod cli;
pub mod cli_docs;
pub mod code;
pub mod config;
#[cfg(unix)]
pub mod deployment;
pub mod media;
pub mod metrics;
pub mod oplog;
pub mod release;
pub mod service;
#[cfg(unix)]
pub mod transport;
pub mod update;

use std::{
    collections::HashMap,
    ffi::CString,
    fmt,
    sync::atomic::{AtomicU32, Ordering},
};

use anyhow::{Context, Result, anyhow, bail};
use graft::{
    core::{LogId, VolumeId},
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::setup_graft,
};
use graft_sqlite::vfs::GraftVfs;
use loro::{ExportMode, LoroDoc};
use rusqlite::Connection;
use sqlite_plugin::vfs::{RegisterOpts, register_static};

pub use archive::ArchiveStore;
pub use backup::{BackupErr, BackupObject, BackupReport, BlobBackup, DocBackup, RetentionReport};
pub use catalog::{
    Blob, BlobStorage, Catalog, CatalogErr, CatalogStatus, Entry, Kind, Lane, NewBlob,
    UpdateRelease,
};
pub use cdc_volume::gc::GcStats;
pub use code::{CodeErr, CodeFile, CodeSnapshot, RestoreStats};
pub use config::{ArchiveBackupConfig, ArchiveConfig, BackupConfig, MediaConfig, WorkspaceConfig};
#[cfg(unix)]
pub use deployment::{DeployErr, DeployReport, DeploymentInfo, RollbackReport};
pub use media::{BlobErr, Blobs, IngestedBlob, MediaStore, PresignedWrite, UploadTicket};
pub use metrics::{LaneMetrics, MetricsErr, StorageMetric};
pub use oplog::{
    AppendOutcome, CompactOutcome, CompactionReport, DocSummary, Frame, Oplog, OplogErr, Snapshot,
};
pub use release::{ReleaseErr, Resource, StagedRelease};
pub use service::{ServiceKind, ServiceOptions};
#[cfg(unix)]
pub use transport::{Frame as TransportFrame, Server, SnapshotAssembler};
pub use update::{
    ApplyReport, ChunkEncoding, GcReport, ManifestPublicKey, ManifestSigningKey, PublishReport,
    ReleaseInfo, ReleaseManifest, UpdateChunk, UpdateErr, UpdateFile, UpdatePlan, UpdateRepo,
};

/// One reconnecting client's catch-up batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncBatch {
    /// Compacted state to import before applying frames. Present when the
    /// caller is behind a snapshot's frontier.
    pub snapshot: Option<Snapshot>,
    /// Frames after the caller's cursors (or after the snapshot), ordered by
    /// producer then sequence.
    pub frames: Vec<Frame>,
    /// Applied watermark per producer, so the client knows what "caught up"
    /// means for this document.
    pub frontier: Vec<(String, i64)>,
}

/// Everything a fresh data directory needs to adopt a workspace.
///
/// The remote log ids are part of the reference on purpose: `VolumeId` alone is
/// not enough to attach, since the volume's remote log is not discoverable from
/// the id. The textual form is produced by `workspace share` and accepted by
/// `workspace clone`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRef {
    pub catalog_volume: VolumeId,
    pub catalog_log: LogId,
    pub oplog_volume: VolumeId,
    pub oplog_log: LogId,
    /// Present when the workspace has an encrypted archive lane.
    pub archive_volume: Option<VolumeId>,
    pub archive_log: Option<LogId>,
}

impl WorkspaceRef {
    /// Prefix marking the textual form, so a bad paste fails loudly.
    pub const PREFIX: &str = "wref:1:";

    /// Parse the form printed by [`fmt::Display`].
    pub fn parse(value: &str) -> Result<WorkspaceRef> {
        let rest = value.trim().strip_prefix(Self::PREFIX).ok_or_else(|| {
            anyhow!(
                "workspace ref must start with `{}` (copy it from `workspace share`)",
                Self::PREFIX
            )
        })?;
        let mut parts = rest.split(':');

        let mut field = |name: &str| -> Result<String> {
            parts
                .next()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("workspace ref is missing the {name} field"))
        };
        let catalog_volume = field("catalog volume")?;
        let catalog_log = field("catalog log")?;
        let oplog_volume = field("oplog volume")?;
        let oplog_log = field("oplog log")?;
        // v2 refs append the archive volume/log. Refs without them stay
        // valid so a workspace that has no archive can still be cloned.
        let archive = match (parts.next(), parts.next()) {
            (None, None) => None,
            (Some(volume), Some(log)) => Some((volume, log)),
            _ => bail!("workspace ref has a partial archive field"),
        };
        if parts.next().is_some() {
            bail!("workspace ref has extra fields");
        }

        Ok(WorkspaceRef {
            catalog_volume: parse_id(&catalog_volume, "catalog volume")?,
            catalog_log: parse_id(&catalog_log, "catalog log")?,
            oplog_volume: parse_id(&oplog_volume, "oplog volume")?,
            oplog_log: parse_id(&oplog_log, "oplog log")?,
            archive_volume: archive
                .as_ref()
                .map(|(volume, _)| parse_id(volume, "archive volume"))
                .transpose()?,
            archive_log: archive
                .as_ref()
                .map(|(_, log)| parse_id(log, "archive log"))
                .transpose()?,
        })
    }
}

fn parse_id<T: std::str::FromStr>(value: &str, field: &str) -> Result<T>
where
    T::Err: fmt::Display,
{
    value
        .parse()
        .map_err(|err| anyhow!("bad {field} id {value:?}: {err}"))
}

impl fmt::Display for WorkspaceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}:{}:{}:{}",
            Self::PREFIX,
            self.catalog_volume.serialize(),
            self.catalog_log.serialize(),
            self.oplog_volume.serialize(),
            self.oplog_log.serialize()
        )?;
        if let (Some(volume), Some(log)) = (&self.archive_volume, &self.archive_log) {
            write!(f, ":{}:{}", volume.serialize(), log.serialize())?;
        }
        Ok(())
    }
}

/// Registrations are process-global, so each workspace gets its own VFS name.
static VFS_SEQ: AtomicU32 = AtomicU32::new(0);

pub struct Workspace {
    catalog: Catalog,
    oplog: Oplog,
    media: Option<MediaStore>,
    archive: Option<ArchiveStore>,
    catalog_volume: VolumeId,
    oplog_volume: VolumeId,
    runtime: Runtime,
}

impl Workspace {
    /// Open (or create) the catalog and oplog volumes for this configuration.
    pub fn open(config: &WorkspaceConfig) -> Result<Workspace> {
        let runtime = runtime_for(config)?;
        Workspace::open_with_runtime(runtime, config)
    }

    fn open_with_runtime(runtime: Runtime, config: &WorkspaceConfig) -> Result<Workspace> {
        let vfs_name = format!("graft_ws_{}", VFS_SEQ.fetch_add(1, Ordering::Relaxed));
        let vfs_name_c = CString::new(vfs_name.clone()).expect("VFS name contains no NUL bytes");
        register_static(
            vfs_name_c,
            GraftVfs::new(runtime.clone()),
            RegisterOpts { make_default: false },
        )
        .map_err(|code| anyhow!("register Graft VFS: sqlite error {code}"))?;

        let catalog_tag = config.catalog_tag.as_str();
        let oplog_tag = config.oplog_tag();

        let catalog_conn = open_volume(&vfs_name, catalog_tag)?;
        let oplog_conn = open_volume(&vfs_name, &oplog_tag)?;

        let catalog = Catalog::open(catalog_conn)?;
        let oplog = Oplog::open(oplog_conn)?;
        let media = config.media.as_ref().map(MediaStore::open).transpose()?;
        let archive = config
            .archive
            .as_ref()
            .map(|archive| ArchiveStore::open(runtime.clone(), &config.archive_tag(), archive))
            .transpose()?;

        let catalog_volume = runtime
            .tag_get(catalog_tag)
            .map_err(|err| anyhow!("read catalog tag: {err}"))?
            .ok_or_else(|| anyhow!("catalog volume {catalog_tag:?} was not created"))?;
        let oplog_volume = runtime
            .tag_get(&oplog_tag)
            .map_err(|err| anyhow!("read oplog tag: {err}"))?
            .ok_or_else(|| anyhow!("oplog volume {oplog_tag:?} was not created"))?;

        Ok(Workspace {
            catalog,
            oplog,
            media,
            archive,
            catalog_volume,
            oplog_volume,
            runtime,
        })
    }

    /// Adopt an existing workspace from its remote into a fresh data directory.
    ///
    /// Both volumes are attached at the ids and remote logs named in
    /// `reference`, then pulled before SQLite opens them, so the catalog schema
    /// and its rows are present when [`Catalog::open`] runs. Refuses a data
    /// directory that already holds a workspace rather than mixing two.
    pub fn adopt(config: &WorkspaceConfig, reference: &WorkspaceRef) -> Result<Workspace> {
        require_remote(config, "clone")?;
        let runtime = one_shot_runtime(config)?;

        let catalog_tag = config.catalog_tag.as_str();
        let oplog_tag = config.oplog_tag();
        let archive_tag = config.archive_tag();
        if reference.archive_volume.is_some() && config.archive.is_none() {
            bail!(
                "this workspace has an encrypted archive; add [archive] with its key_file \
                 before cloning"
            );
        }
        if runtime.tag_exists(catalog_tag)?
            || runtime.tag_exists(&oplog_tag)?
            || (config.archive.is_some() && runtime.tag_exists(&archive_tag)?)
        {
            bail!(
                "{} already holds a workspace; use another --data directory",
                config.data_dir.display()
            );
        }

        let mut attachments = vec![
            (
                catalog_tag.to_string(),
                reference.catalog_volume.clone(),
                reference.catalog_log.clone(),
            ),
            (
                oplog_tag.clone(),
                reference.oplog_volume.clone(),
                reference.oplog_log.clone(),
            ),
        ];
        if let (Some(volume), Some(log)) = (&reference.archive_volume, &reference.archive_log) {
            attachments.push((archive_tag.clone(), volume.clone(), log.clone()));
        }
        for (tag, vid, log) in &attachments {
            runtime
                .volume_open(Some(vid.clone()), None, Some(log.clone()))
                .with_context(|| format!("attach volume {vid}"))?;
            runtime.tag_replace(tag, vid.clone())?;
        }

        if let Err(err) = pull_volumes(
            &runtime,
            &reference.catalog_volume,
            &reference.oplog_volume,
            reference.archive_volume.as_ref(),
        ) {
            rollback_adoption(&runtime, catalog_tag, &oplog_tag, &archive_tag, reference);
            return Err(err);
        }

        // An un-pushed workspace pulls as an empty remote log without an error.
        // `init` always writes the schema and an id, so an empty catalog remote
        // means the reference points at a workspace the remote has never seen.
        let mut missing_head = runtime
            .volume_status(&reference.catalog_volume)?
            .remote_status
            .head
            .is_none()
            .then(|| reference.catalog_volume.clone());
        if missing_head.is_none()
            && let Some(volume) = &reference.archive_volume
            && runtime.volume_status(volume)?.remote_status.head.is_none()
        {
            missing_head = Some(volume.clone());
        }
        if let Some(volume) = missing_head {
            rollback_adoption(&runtime, catalog_tag, &oplog_tag, &archive_tag, reference);
            bail!(
                "remote has no commits for volume {volume}; run `workspace sync` \
                 in the original data directory first"
            );
        }

        Workspace::open_with_runtime(runtime, config)
    }

    /// Fetch new commits for both volumes from the remote.
    ///
    /// Pulling happens before any SQLite connection opens the volumes: the
    /// connections cache their snapshot, so advancing the local log underneath
    /// an open database is not safe.
    pub fn pull(config: &WorkspaceConfig) -> Result<(VolumeId, VolumeId)> {
        require_remote(config, "pull")?;
        let (runtime, catalog, oplog) = runtime_and_volumes(config)?;
        let archive = configured_archive_volume(&runtime, config)?;

        pull_volumes(&runtime, &catalog, &oplog, archive.as_ref())?;
        Ok((catalog, oplog))
    }

    /// Push both volumes' local commits to the remote.
    ///
    /// `autosync` only pushes while a process is alive, and every command here
    /// exits quickly, so a one-shot push is the reliable way to make commits
    /// visible to another machine.
    pub fn push(config: &WorkspaceConfig) -> Result<(VolumeId, VolumeId)> {
        require_remote(config, "sync")?;
        let (runtime, catalog, oplog) = runtime_and_volumes(config)?;

        runtime
            .volume_push(catalog.clone())
            .with_context(|| format!("push catalog volume {catalog}"))?;
        runtime
            .volume_push(oplog.clone())
            .with_context(|| format!("push oplog volume {oplog}"))?;
        if let Some(archive) = configured_archive_volume(&runtime, config)? {
            runtime
                .volume_push(archive.clone())
                .with_context(|| format!("push archive volume {archive}"))?;
        }
        Ok((catalog, oplog))
    }

    /// The reference another data directory needs to adopt this workspace.
    pub fn share_ref(&self) -> Result<WorkspaceRef> {
        let catalog = self.runtime.volume_get(&self.catalog_volume)?;
        let oplog = self.runtime.volume_get(&self.oplog_volume)?;
        let archive = match &self.archive {
            Some(archive) => Some(self.runtime.volume_get(&archive.volume_id())?),
            None => None,
        };
        Ok(WorkspaceRef {
            catalog_volume: catalog.vid,
            catalog_log: catalog.remote,
            oplog_volume: oplog.vid,
            oplog_log: oplog.remote,
            archive_volume: archive.as_ref().map(|archive| archive.vid.clone()),
            archive_log: archive.map(|archive| archive.remote),
        })
    }

    /// Assign the workspace id if it does not exist yet.
    pub fn init(&mut self) -> Result<String> {
        Ok(self.catalog.init()?)
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn oplog(&self) -> &Oplog {
        &self.oplog
    }

    pub fn media(&self) -> Option<&MediaStore> {
        self.media.as_ref()
    }

    /// Blob operations for this workspace. Fails when no `[media]` store is
    /// configured or the workspace has no id yet.
    pub fn blobs(&self) -> Result<Blobs<'_>, BlobErr> {
        if self.media.is_none() && self.archive.is_none() {
            return Err(BlobErr::NoMediaStore);
        }
        let workspace = self.catalog.workspace_id().ok_or(BlobErr::NotInitialized)?;
        Ok(Blobs::new(
            &self.catalog,
            self.media.as_ref(),
            self.archive.as_ref(),
            workspace,
        ))
    }

    pub fn archive(&self) -> Option<&ArchiveStore> {
        self.archive.as_ref()
    }

    /// Replace an empty catalog with the export at `path`.
    pub fn restore_catalog(&mut self, path: &std::path::Path) -> Result<(), crate::CatalogErr> {
        self.catalog.restore_from(path)
    }

    /// Delete archived blobs no entry references and no release pins, drop
    /// manifests no catalog row names, then rewrite the archive volume into a
    /// fresh one and repoint the archive tag at it. Returns the forgotten blob
    /// hashes and the rewrite stats.
    ///
    /// The old volume is left in place for a grace period; `sync` publishes the
    /// new one, and a clone from the new reference never needs the old one.
    pub fn gc_archive(
        &mut self,
        config: &WorkspaceConfig,
    ) -> Result<(Vec<String>, GcStats), BlobErr> {
        let archive_config = config.archive.as_ref().ok_or(BlobErr::NoArchiveStore)?;
        let removed = self.blobs()?.sweep_unreferenced_cdc()?;
        let (stats, archive) = self.archive.as_ref().ok_or(BlobErr::NoArchiveStore)?.gc(
            &self.runtime,
            &config.archive_tag(),
            archive_config,
        )?;
        self.archive = Some(archive);
        Ok((removed, stats))
    }

    /// Checkpoint `root` as a code tree and record it on `entry_path`. Every
    /// file, the manifest and any git bundle are stored in the encrypted
    /// archive and pinned, so `blob gc-cdc` cannot collect them. A configured
    /// `[archive]` key is required.
    pub fn snapshot_code(
        &self,
        root: &std::path::Path,
        entry_path: &str,
    ) -> Result<code::CodeSnapshot> {
        let blobs = self.blobs()?;
        let snapshot = code::snapshot(&blobs, root)?;
        let mut entry = Entry::new(entry_path, Lane::Code, Kind::Dir);
        entry.code_tree = Some(snapshot.tree.clone());
        entry.size = Some(snapshot.files as i64);
        self.catalog.entry_put(&entry)?;
        Ok(snapshot)
    }

    pub fn catalog_volume(&self) -> &VolumeId {
        &self.catalog_volume
    }

    pub fn oplog_volume(&self) -> &VolumeId {
        &self.oplog_volume
    }

    /// Catch a client up: the compacted snapshot when the client is behind it,
    /// every frame after its per-producer cursors (or after that snapshot), and
    /// the frontier the client is catching up to.
    pub fn join(&self, doc_id: &str, cursors: &[(String, i64)]) -> Result<SyncBatch> {
        let stored = self.oplog.snapshot(doc_id)?;
        let mut effective = cursors.to_vec();
        let mut needs_snapshot = false;
        if let Some(snapshot) = &stored {
            for (peer, seq) in &snapshot.frontier {
                let cursor = effective
                    .iter_mut()
                    .find(|(name, _)| name == peer)
                    .map(|(_, value)| value);
                match cursor {
                    Some(value) => {
                        if *value < *seq {
                            *value = *seq;
                            needs_snapshot = true;
                        }
                    }
                    None => {
                        effective.push((peer.clone(), *seq));
                        needs_snapshot = true;
                    }
                }
            }
        }
        Ok(SyncBatch {
            snapshot: stored.filter(|_| needs_snapshot),
            frames: self.oplog.frames_since(doc_id, &effective)?,
            frontier: self.oplog.frontier(doc_id)?,
        })
    }

    /// Build a snapshot from the stored snapshot plus the frames that followed
    /// it. Returns `None` when the stored frame history has a gap, so a caller
    /// never records a snapshot that silently omits edits.
    fn materialize_snapshot(&self, doc_id: &str) -> Result<Option<Snapshot>> {
        let stored = self.oplog.snapshot(doc_id)?;
        let loro = LoroDoc::new();
        let mut expected: HashMap<String, i64> = HashMap::new();
        if let Some(snapshot) = &stored {
            loro.import(&snapshot.data)
                .map_err(|err| anyhow!("import snapshot for {doc_id:?}: {err}"))?;
            for (peer, seq) in &snapshot.frontier {
                expected.insert(peer.clone(), seq + 1);
            }
        }

        let mut applied = 0usize;
        for frame in self.oplog.frames_since(doc_id, &[])? {
            let next = expected.entry(frame.peer.clone()).or_insert(1);
            if frame.seq < *next {
                continue;
            }
            if frame.seq != *next {
                return Ok(None);
            }
            loro.import(&frame.data)
                .map_err(|err| anyhow!("import frame for {doc_id:?}: {err}"))?;
            *next = frame.seq + 1;
            applied += 1;
        }

        if applied == 0 {
            return Ok(stored);
        }
        let frontier = self.oplog.frontier(doc_id)?;
        let data = loro
            .export(ExportMode::Snapshot)
            .map_err(|err| anyhow!("export snapshot for {doc_id:?}: {err}"))?;
        Ok(Some(Snapshot {
            doc_id: doc_id.to_string(),
            frontier,
            data,
        }))
    }

    /// Materialize the document snapshot, then prune one producer's frames
    /// through `through`. The snapshot makes a later join and backup complete
    /// even though early frames are gone; the existing ack guard still decides
    /// whether the prune is allowed.
    pub fn compact_document(
        &self,
        doc_id: &str,
        producer: &str,
        through: i64,
    ) -> Result<CompactOutcome> {
        if through < 1 {
            return Ok(CompactOutcome::Nothing);
        }
        let Some(snapshot) = self.materialize_snapshot(doc_id)? else {
            bail!(
                "cannot compact {doc_id:?}: its remaining frames do not describe a complete document"
            );
        };
        let stored = self.oplog.snapshot(doc_id)?;
        if stored
            .as_ref()
            .is_none_or(|old| old.frontier != snapshot.frontier || old.data != snapshot.data)
        {
            self.oplog
                .put_snapshot(doc_id, &snapshot.frontier, &snapshot.data)?;
        }
        Ok(self.oplog.compact(doc_id, producer, through)?)
    }

    /// Materialize a snapshot for every document with stored frames, then
    /// compact the prefixes every known consumer has acknowledged. A document
    /// whose frame history has a gap is left untouched.
    pub fn compact_idle(&self) -> Result<CompactionReport> {
        let mut report = CompactionReport::default();
        let stored = self.oplog.docs()?;
        for doc in stored {
            if doc.frames == 0 {
                continue;
            }
            report.docs += 1;
            let materialized = match self.materialize_snapshot(&doc.doc_id) {
                Ok(value) => value,
                Err(_) => {
                    report.failed += 1;
                    continue;
                }
            };
            let Some(snapshot) = materialized else {
                continue;
            };
            let previous = self.oplog.snapshot(&doc.doc_id)?;
            if previous
                .as_ref()
                .is_none_or(|old| old.frontier != snapshot.frontier || old.data != snapshot.data)
            {
                self.oplog
                    .put_snapshot(&doc.doc_id, &snapshot.frontier, &snapshot.data)?;
            }
            report.snapshots += 1;
            for (peer, applied) in &snapshot.frontier {
                match self.oplog.compact(&doc.doc_id, peer, *applied)? {
                    CompactOutcome::Compacted { frames, .. } => {
                        report.compacted += 1;
                        report.frames_deleted += frames;
                    }
                    CompactOutcome::Blocked { .. } => report.blocked += 1,
                    CompactOutcome::Nothing => {}
                }
            }
        }
        Ok(report)
    }

    /// Offer a frame from a producer. Returns whether it was applied, was a
    /// replay, or skipped a sequence.
    pub fn submit(&self, doc_id: &str, peer: &str, seq: i64, data: &[u8]) -> Result<AppendOutcome> {
        Ok(self.oplog.append(doc_id, peer, seq, data)?)
    }

    /// Record a consumer's progress through a producer's stream.
    pub fn ack(&self, doc_id: &str, producer: &str, consumer: &str, seq: i64) -> Result<i64> {
        Ok(self.oplog.ack(doc_id, producer, consumer, seq)?)
    }

    /// Note the frontier of a compacted snapshot on the catalog entries that
    /// point at this document. Returns the number of entries updated.
    pub fn note_snapshot(&self, doc_id: &str, frontier: &[u8]) -> Result<usize> {
        Ok(self.catalog.note_snapshot(doc_id, frontier)?)
    }
}

fn runtime_for(config: &WorkspaceConfig) -> Result<Runtime> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("create data directory {}", config.data_dir.display()))?;
    setup_graft(config.graft_config()).map_err(|err| anyhow!("graft setup: {err}"))
}

/// Pulling against the process-local memory remote can only fail: a fresh
/// process starts with an empty log. Say so instead of reporting a missing log.
fn require_remote(config: &WorkspaceConfig, command: &str) -> Result<()> {
    if matches!(config.remote, RemoteConfig::Memory) {
        bail!("{command} needs a remote: pass `--remote` or set `remote` in workspace.toml");
    }
    Ok(())
}

/// A runtime with `autosync` disabled.
///
/// One-shot push and pull finish while the command is exiting, and a periodic
/// autosync tick would race them into Graft's single-pending-commit assertion.
fn one_shot_runtime(config: &WorkspaceConfig) -> Result<Runtime> {
    let mut config = config.clone();
    config.autosync = None;
    runtime_for(&config)
}

/// The runtime plus the volumes this data directory already holds.
fn runtime_and_volumes(config: &WorkspaceConfig) -> Result<(Runtime, VolumeId, VolumeId)> {
    let runtime = one_shot_runtime(config)?;
    let missing = || {
        anyhow!(
            "no workspace in {}; run `workspace init` or `workspace clone` first",
            config.data_dir.display()
        )
    };
    let catalog = runtime.tag_get(&config.catalog_tag)?.ok_or_else(missing)?;
    let oplog = runtime.tag_get(&config.oplog_tag())?.ok_or_else(missing)?;
    Ok((runtime, catalog, oplog))
}

/// Undo a failed adoption: leave no tags behind, or a later clone into this
/// directory would be refused as already holding a workspace.
fn rollback_adoption(
    runtime: &Runtime,
    catalog_tag: &str,
    oplog_tag: &str,
    archive_tag: &str,
    reference: &WorkspaceRef,
) {
    let _ = runtime.tag_delete(catalog_tag);
    let _ = runtime.tag_delete(oplog_tag);
    let _ = runtime.volume_delete(&reference.catalog_volume);
    let _ = runtime.volume_delete(&reference.oplog_volume);
    if let Some(archive) = &reference.archive_volume {
        let _ = runtime.tag_delete(archive_tag);
        let _ = runtime.volume_delete(archive);
    }
}

fn pull_volumes(
    runtime: &Runtime,
    catalog: &VolumeId,
    oplog: &VolumeId,
    archive: Option<&VolumeId>,
) -> Result<()> {
    runtime
        .volume_pull(catalog.clone())
        .with_context(|| format!("pull catalog volume {catalog}"))?;
    runtime
        .volume_pull(oplog.clone())
        .with_context(|| format!("pull oplog volume {oplog}"))?;
    if let Some(archive) = archive {
        runtime
            .volume_pull(archive.clone())
            .with_context(|| format!("pull archive volume {archive}"))?;
    }
    Ok(())
}

/// The archive volume, when configured and already created in this data
/// directory. An old workspace without an archive simply skips sync.
fn configured_archive_volume(
    runtime: &Runtime,
    config: &WorkspaceConfig,
) -> Result<Option<VolumeId>> {
    if config.archive.is_none() {
        return Ok(None);
    }
    runtime
        .tag_get(&config.archive_tag())
        .map_err(|err| anyhow!("read archive tag: {err}"))
}

fn open_volume(vfs_name: &str, tag: &str) -> Result<Connection> {
    Connection::open(format!("file:{tag}?vfs={vfs_name}"))
        .with_context(|| format!("open workspace volume {tag:?}"))
}
