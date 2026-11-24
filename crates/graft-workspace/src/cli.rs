//! `workspace` command line interface.
//!
//! Every invocation opens the workspace volumes, does one thing, and exits.
//! That keeps the durability story honest: each command is a fresh process, so
//! reopening both volumes from the local index and the remote is exercised
//! constantly.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::{
    AppendOutcome, Blob, BlobStorage, Catalog, CompactOutcome, Entry, Kind, Lane, UploadTicket,
    Workspace, WorkspaceConfig, WorkspaceRef,
};

#[derive(Debug, Parser)]
#[command(
    name = "workspace",
    about = "Workspace catalog and live-document oplog on Graft",
    version
)]
pub struct Cli {
    /// Config file. Defaults to `./workspace.toml` when it exists.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Graft data directory; overrides `data_dir` from the config.
    #[arg(long, global = true, value_name = "DIR")]
    pub data: Option<PathBuf>,

    /// Remote: `memory`, `fs:DIR`, or `s3:BUCKET[/PREFIX]`.
    #[arg(long, global = true, value_name = "SPEC")]
    pub remote: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the catalog volume and assign a workspace id.
    Init,
    /// Show the workspace id, schema version and row counts.
    Status,
    /// Print the reference another data directory needs to adopt this workspace.
    Share,
    /// Adopt an existing workspace from a remote into a fresh data directory.
    Clone {
        /// Workspace reference printed by `workspace share`.
        #[arg(value_name = "REF")]
        reference: String,
    },
    /// Serve live-document frames over a Unix socket.
    Serve {
        /// Socket path for document clients.
        #[arg(long, value_name = "PATH")]
        socket: PathBuf,
        /// Compact acknowledged documents after this many seconds without a
        /// committed frame or ack. Zero disables idle compaction.
        #[arg(long, default_value_t = 300, value_name = "SECS")]
        compact_after_secs: u64,
    },
    /// Push local commits for both volumes to the remote.
    Sync,
    /// Fetch new commits for both volumes from the remote.
    Pull,
    /// Read and write catalog entries.
    Entry {
        #[command(subcommand)]
        command: EntryCommand,
    },
    /// Read, write and inspect media blobs.
    Blob {
        #[command(subcommand)]
        command: BlobCommand,
    },
    /// Live-document frames: the Loro oplog lane.
    Doc {
        #[command(subcommand)]
        command: DocCommand,
    },
    /// Versioned code checkpoints: tree manifests and git bundles.
    Code {
        #[command(subcommand)]
        command: CodeCommand,
    },
    /// Release staging: assemble entries into a datapackage.
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
    /// Binary updates: `FastCDC` content-addressed releases and deltas.
    Update {
        /// Repository store: `fs:DIR`, `s3:BUCKET[/PREFIX]`, or a read-only
        /// `https://`, `http2://` (h2c), or `quic://` (HTTP/3) URL.
        #[arg(long, value_name = "SPEC")]
        repo: Option<String>,
        /// Prefix inside the repository store.
        #[arg(long, default_value = "updates", value_name = "PREFIX")]
        prefix: String,
        /// Additional PEM certificate authority for HTTPS or QUIC.
        #[arg(long, value_name = "FILE")]
        ca_file: Option<PathBuf>,
        #[command(subcommand)]
        command: UpdateCommand,
    },
    /// Print lane counters as Prometheus text.
    Metrics,
    /// Export the catalog, blobs and document snapshots to the backup target.
    Backup {
        /// Replace the configured base prefix for this run.
        #[arg(long, value_name = "PREFIX")]
        prefix: Option<String>,
        /// Run continuously, taking a backup every SECS. Omit for one backup.
        #[arg(long, value_name = "SECS")]
        every: Option<u64>,
        /// Keep the newest N complete backups; older ones are deleted. In
        /// scheduled mode this defaults to 7.
        #[arg(long, value_name = "N")]
        keep: Option<usize>,
    },
    /// Rebuild a workspace from a backup prefix into an empty data directory.
    Restore {
        /// Exact prefix printed by `backup` (for example `backup/ws/…`).
        prefix: String,
    },
    /// Generate service files that schedule `workspace backup`.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum EntryCommand {
    /// Insert or update an entry.
    Put {
        /// Workspace path, e.g. `notes/hello`.
        path: String,
        #[arg(long, value_enum)]
        lane: Lane,
        #[arg(long, value_enum)]
        kind: Kind,
        /// Blob hash (`blake3:<hex>`) this entry points at.
        #[arg(long, value_name = "HASH")]
        blob: Option<String>,
        /// Loro document id for a live document.
        #[arg(long, value_name = "ID")]
        loro_doc: Option<String>,
        /// Code tree manifest hash.
        #[arg(long, value_name = "HASH")]
        code_tree: Option<String>,
        /// DOI of the release this entry belongs to.
        #[arg(long, value_name = "DOI")]
        release_doi: Option<String>,
        #[arg(long, value_name = "MIME")]
        mime: Option<String>,
        #[arg(long, value_name = "BYTES")]
        size: Option<i64>,
    },
    /// Print one entry.
    Get {
        /// Workspace path.
        path: String,
    },
    /// List entries under a prefix.
    Ls {
        /// Path prefix; the whole workspace when omitted.
        prefix: Option<String>,
        /// Include soft-deleted entries.
        #[arg(long)]
        all: bool,
    },
    /// Soft delete an entry.
    Rm {
        /// Workspace path.
        path: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum BlobCommand {
    /// Ingest a file and point an entry at it.
    Put {
        /// Workspace path the entry is created at.
        path: String,
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
        /// `raw` streams into object storage; `cdc` chunks and encrypts into
        /// the archive volume and supports range reads too.
        #[arg(long, value_enum, default_value = "raw")]
        storage: BlobStorage,
        #[arg(long, value_name = "MIME")]
        mime: Option<String>,
    },
    /// Get a short-lived URL for a direct upload to staging.
    Presign {
        /// Expected blob hash (`blake3:<hex>`), computed by the client.
        hash: String,
        /// Presigned request lifetime.
        #[arg(long, default_value_t = 900, value_name = "SECS")]
        expires: u64,
    },
    /// Verify a direct upload, store it canonically, and create its entry.
    Commit {
        /// Workspace path for the new entry.
        path: String,
        /// Staging key printed by `blob presign`.
        #[arg(long, value_name = "KEY")]
        ticket: String,
        /// Hash the client computed for the uploaded bytes.
        #[arg(long, value_name = "HASH")]
        hash: String,
        /// Expected byte count; checked before the object is copied.
        #[arg(long, value_name = "BYTES")]
        size: Option<u64>,
        #[arg(long, value_name = "MIME")]
        mime: Option<String>,
    },
    /// Write an entry's blob, or a range of it, to a file.
    Get {
        /// Workspace path whose blob is read.
        path: String,
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
        #[arg(long, default_value_t = 0, value_name = "BYTES")]
        offset: u64,
        #[arg(long, value_name = "BYTES")]
        limit: Option<u64>,
    },
    /// Remove abandoned staging objects.
    SweepStaging {
        /// Only remove objects at least this old.
        #[arg(long, default_value_t = 86_400, value_name = "SECS")]
        older_than: u64,
    },
    /// Delete raw objects that no entry references and that no release pins.
    Gc,
    /// Delete unreferenced archived blobs, then rewrite the encrypted archive
    /// volume into a fresh one and repoint its tag.
    GcCdc,
    /// List recorded blobs.
    Ls,
    /// Print one blob's record.
    Show {
        /// Blob hash (`blake3:<hex>`).
        hash: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum UpdateCommand {
    /// Publish a directory as an immutable release.
    Publish {
        /// Repository name, e.g. `my-app`.
        name: String,
        /// Release identifier, e.g. `1.4.0`.
        release: String,
        /// Directory to publish.
        root: PathBuf,
        /// Ed25519 private key (64 hex characters) used to sign the manifest.
        #[arg(long, value_name = "FILE")]
        signing_key: Option<PathBuf>,
        /// Store zstd-compressed chunks when they are smaller than raw chunks.
        #[arg(long)]
        compress: bool,
        /// Zstd compression level (1-22); implies --compress.
        #[arg(long, value_name = "LEVEL")]
        compress_level: Option<i32>,
    },
    /// Compare a release with an installed-state manifest.
    Plan {
        /// Repository name.
        name: String,
        /// Release identifier.
        release: String,
        /// Manifest written by a previous apply or `update state`.
        #[arg(long, value_name = "FILE")]
        state: Option<PathBuf>,
        /// Require a manifest signed by this Ed25519 public key.
        #[arg(long, value_name = "FILE")]
        public_key: Option<PathBuf>,
    },
    /// Write a manifest for an installed directory without contacting a repo.
    State {
        /// Repository name to record.
        name: String,
        /// Release identifier to record.
        release: String,
        /// Installed directory to scan.
        root: PathBuf,
        /// Manifest to write.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Apply a release, downloading only chunks absent from the installed tree.
    Apply {
        /// Repository name.
        name: String,
        /// Release identifier.
        release: String,
        /// Destination directory.
        root: PathBuf,
        /// Installed-state manifest; written after a successful update.
        #[arg(long, value_name = "FILE")]
        state: PathBuf,
        /// Remove files that the previous state contains and this release does not.
        #[arg(long)]
        delete: bool,
        /// Require a manifest signed by this Ed25519 public key.
        #[arg(long, value_name = "FILE")]
        public_key: Option<PathBuf>,
    },
    /// List release refs in the repository.
    Ls {
        /// Repository name.
        name: String,
    },
    /// Generate an Ed25519 signing key pair.
    Keygen {
        /// Destination for the private key (created with mode 0600).
        #[arg(long, value_name = "FILE")]
        private_key: PathBuf,
        /// Destination for the public key.
        #[arg(long, value_name = "FILE")]
        public_key: PathBuf,
    },
    /// Remove manifests and chunks unreachable from every release ref.
    Gc {
        /// Repository name.
        name: String,
        /// Keep unreferenced objects newer than this many seconds.
        #[arg(long, default_value_t = 86_400, value_name = "SECS")]
        grace_secs: u64,
        /// Report what would be removed without deleting it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete old release refs, keeping the newest ones.
    PruneRefs {
        /// Repository name.
        name: String,
        /// Number of most recently published refs to keep.
        #[arg(long, value_name = "N")]
        keep: usize,
        /// Keep refs published within this many seconds.
        #[arg(long, default_value_t = 86_400, value_name = "SECS")]
        grace_secs: u64,
        /// Report what would be removed without deleting it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install a release into a new deployment and atomically switch `current`.
    Deploy {
        /// Repository name.
        name: String,
        /// Release identifier.
        release: String,
        /// Deployment root (holds `deployments/`, `states/` and `current`).
        root: PathBuf,
        /// Require a manifest signed by this Ed25519 public key.
        #[arg(long, value_name = "FILE")]
        public_key: Option<PathBuf>,
    },
    /// Switch `current` back to the previous deployment.
    Rollback {
        /// Deployment root.
        root: PathBuf,
    },
    /// List deployments under a deployment root.
    Deployments {
        /// Deployment root.
        root: PathBuf,
    },
    /// Delete old deployments, keeping the active and previous versions.
    PruneDeployments {
        /// Deployment root.
        root: PathBuf,
        /// Number of deployments to keep, including current and previous.
        #[arg(long, value_name = "N")]
        keep: usize,
        /// Report what would be removed without deleting it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ReleaseCommand {
    /// Copy selected entries into a staging directory.
    Stage {
        /// Catalog entry to include; repeatable.
        #[arg(long = "entry", required = true)]
        entries: Vec<String>,
        /// Staging directory; must be absent or empty.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Write systemd units or a launchd plist for scheduled one-shot backups.
    Generate {
        /// Service manager format. Defaults to the current platform.
        #[arg(long, value_enum, value_name = "KIND")]
        kind: Option<crate::service::ServiceKind>,
        /// Seconds between backups.
        #[arg(long, default_value_t = 3600, value_name = "SECS")]
        interval: u64,
        /// Number of complete backups to keep.
        #[arg(long, default_value_t = 7, value_name = "N")]
        keep: usize,
        /// Unit/plist base name.
        #[arg(long, default_value = "workspace-backup", value_name = "NAME")]
        name: String,
        /// Directory to write the generated files into.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// `workspace` executable to invoke; defaults to this binary.
        #[arg(long, value_name = "PATH")]
        exec: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum CodeCommand {
    /// Checkpoint a directory into the encrypted archive (requires [archive]).
    Snapshot {
        /// Directory to snapshot.
        dir: PathBuf,
        /// Catalog path to record, e.g. `code/graft`.
        #[arg(long)]
        path: String,
    },
    /// Restore a checkpoint into a directory, verifying every hash.
    Restore {
        /// Tree manifest hash (`blake3:<hex>`), from `code snapshot` or an entry.
        tree: String,
        /// Destination directory; created if missing.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum DocCommand {
    /// List documents with frame counts.
    Ls,
    /// Print one document's oplog state and frontier.
    Stats {
        /// Document id.
        doc: String,
    },
    /// Offer one frame from a producer. The payload is read from a file.
    Append {
        /// Document id.
        doc: String,
        /// Producer name; sequences are per (document, producer).
        #[arg(long)]
        peer: String,
        /// Next sequence number for this producer, starting at 1.
        #[arg(long)]
        seq: i64,
        /// File holding the opaque frame payload.
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
    },
    /// Print, or write out, frames after the given cursors.
    Frames {
        /// Document id.
        doc: String,
        /// Cursor as `PEER=SEQ`, repeatable. Producers without one start at 1.
        #[arg(long = "cursor", value_name = "PEER=SEQ")]
        cursors: Vec<String>,
        /// Write each payload to `<DIR>/<peer>-<seq>.bin` instead of printing
        /// only its length.
        #[arg(long, value_name = "DIR")]
        out_dir: Option<PathBuf>,
        /// Write the compacted snapshot that precedes these frames here.
        #[arg(long, value_name = "FILE")]
        snapshot_out: Option<PathBuf>,
    },
    /// Record how far a consumer has read a producer's stream.
    Ack {
        /// Document id.
        doc: String,
        /// Producer whose stream is being acknowledged.
        #[arg(long)]
        producer: String,
        /// Consumer doing the acknowledging.
        #[arg(long)]
        consumer: String,
        /// Sequence number consumed through.
        #[arg(long)]
        seq: i64,
    },
    /// Delete frames every known consumer has acknowledged.
    Compact {
        /// Document id.
        doc: String,
        /// Producer whose stream is being compacted.
        #[arg(long)]
        producer: String,
        /// Sequence to compact through; every consumer must have acked it.
        #[arg(long)]
        through: i64,
    },
    /// Record a compacted snapshot frontier on catalog entries.
    Snapshot {
        /// Document id.
        doc: String,
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
    },
}

pub fn run(cli: Cli) -> Result<()> {
    let config_path = cli.config.clone().or_else(|| {
        let default = PathBuf::from("workspace.toml");
        default.is_file().then_some(default)
    });
    let data_override = cli.data.clone();
    let remote_override = cli.remote.clone();
    let config = WorkspaceConfig::load(cli.config.as_deref(), cli.data, cli.remote.as_deref())?;

    // Clone and pull must not open the volumes first: opening would create an
    // empty catalog in the target directory, and pulling through an open SQLite
    // connection would advance the local log underneath a cached snapshot.
    let command = match cli.command {
        Command::Clone { reference } => {
            let reference = WorkspaceRef::parse(&reference)?;
            let workspace = Workspace::adopt(&config, &reference)?;
            let status = workspace.catalog().status()?;
            let docs = workspace.oplog().docs()?.len();
            match &status.workspace_id {
                Some(id) => println!("workspace: {id}"),
                None => println!("workspace: (none)"),
            }
            println!("ref: {}", workspace.share_ref()?);
            println!("remote: {}", config.remote_label());
            println!("entries: {}", status.entries);
            println!("docs: {docs}");
            return Ok(());
        }
        Command::Sync => {
            let (catalog, oplog) = Workspace::push(&config)?;
            println!("pushed catalog {catalog}");
            println!("pushed oplog {oplog}");
            return Ok(());
        }
        Command::Pull => {
            let (catalog, oplog) = Workspace::pull(&config)?;
            println!("pulled catalog {catalog}");
            println!("pulled oplog {oplog}");
            return Ok(());
        }
        Command::Service { command } => {
            let config_path = config_path.ok_or_else(|| {
                anyhow::anyhow!("service generation needs workspace.toml; pass --config <FILE>")
            })?;
            if config.backup.is_none() {
                bail!("no backup target configured; add a [backup] section to workspace.toml");
            }
            service(
                &config_path,
                command,
                data_override.as_deref(),
                remote_override.as_deref(),
            )?;
            return Ok(());
        }
        Command::Update { repo, prefix, ca_file, command } => {
            update(
                repo.as_deref(),
                &prefix,
                ca_file.as_deref(),
                &config,
                config_path.is_some(),
                command,
            )?;
            return Ok(());
        }
        command => command,
    };

    let mut workspace = Workspace::open(&config)?;

    match command {
        Command::Init => {
            let id = workspace.init()?;
            println!("workspace: {id}");
            println!("catalog tag: {}", config.catalog_tag);
            println!("oplog tag: {}", config.oplog_tag());
            println!("data: {}", config.data_dir.display());
            println!("remote: {}", config.remote_label());
        }
        Command::Status => status(&workspace, &config)?,
        #[cfg(unix)]
        Command::Serve { socket, compact_after_secs } => {
            let server = crate::transport::Server::bind(workspace, &socket)?
                .compact_when_idle(Duration::from_secs(compact_after_secs));
            println!("listening on {}", socket.display());
            server.run()?;
        }
        #[cfg(not(unix))]
        Command::Serve { .. } => {
            bail!("the coordinator transport needs a Unix socket");
        }
        Command::Share => {
            let status = workspace.catalog().status()?;
            match &status.workspace_id {
                Some(id) => println!("workspace: {id}"),
                None => println!("workspace: (none; run `workspace init`)"),
            }
            println!("ref: {}", workspace.share_ref()?);
            println!("remote: {}", config.remote_label());
            println!("next: workspace clone <ref> --data <DIR>");
        }
        Command::Entry { command } => entry(workspace.catalog(), command)?,
        Command::Blob { command } => blob(&mut workspace, &config, command)?,
        Command::Doc { command } => doc(&workspace, command)?,
        Command::Code { command } => code(&workspace, command)?,
        Command::Release { command } => release(&workspace, command)?,
        Command::Update { .. } => unreachable!("handled before opening"),
        Command::Metrics => metrics(&workspace)?,
        Command::Backup { prefix, every, keep } => {
            backup(&workspace, &config, prefix.as_deref(), every, keep)?
        }
        Command::Restore { prefix } => restore(&mut workspace, &config, &prefix)?,
        Command::Clone { .. } | Command::Sync | Command::Pull | Command::Service { .. } => {
            unreachable!("handled before opening")
        }
    }

    Ok(())
}

fn status(workspace: &Workspace, config: &WorkspaceConfig) -> Result<()> {
    let status = workspace.catalog().status()?;
    let docs = workspace.oplog().docs()?.len();

    match &status.workspace_id {
        Some(id) => println!("workspace: {id}"),
        None => println!("workspace: (none; run `workspace init`)"),
    }
    println!("catalog tag: {}", config.catalog_tag);
    println!("oplog tag: {}", config.oplog_tag());
    println!("schema: {}", status.schema_version);
    println!("remote: {}", config.remote_label());
    println!("entries: {}", status.entries);
    println!("docs: {docs}");
    println!("blobs: {} (pinned {})", status.blobs, status.pinned_blobs);
    println!("releases: {}", status.releases);
    Ok(())
}

fn entry(catalog: &Catalog, command: EntryCommand) -> Result<()> {
    match command {
        EntryCommand::Put {
            path,
            lane,
            kind,
            blob,
            loro_doc,
            code_tree,
            release_doi,
            mime,
            size,
        } => {
            let mut entry = Entry::new(path, lane, kind);
            entry.blob_hash = blob;
            entry.loro_doc_id = loro_doc;
            entry.code_tree = code_tree;
            entry.release_doi = release_doi;
            entry.mime = mime;
            entry.size = size;
            let revision = catalog.entry_put(&entry)?;
            println!("put {}: revision {revision}", entry.path);
        }
        EntryCommand::Get { path } => {
            let entry = catalog
                .entry_get(&path)?
                .ok_or_else(|| anyhow::anyhow!("no entry at {path:?}"))?;
            print_entry(&entry);
        }
        EntryCommand::Ls { prefix, all } => {
            let prefix = prefix.unwrap_or_default();
            for entry in catalog.entry_list(&prefix, all)? {
                println!(
                    "{}\t{}\t{}\t{}",
                    entry.path,
                    entry.lane.as_str(),
                    entry.kind.as_str(),
                    entry.revision
                );
            }
        }
        EntryCommand::Rm { path } => {
            if !catalog.entry_remove(&path)? {
                bail!("no live entry at {path:?}");
            }
            println!("removed {path}");
        }
    }
    Ok(())
}

fn blob(workspace: &mut Workspace, config: &WorkspaceConfig, command: BlobCommand) -> Result<()> {
    let catalog = workspace.catalog();
    match command {
        BlobCommand::Put { path, file, storage, mime } => {
            let input =
                fs::File::open(&file).with_context(|| format!("open {}", file.display()))?;
            let blobs = workspace.blobs()?;
            let ingested = match storage {
                BlobStorage::Raw => blobs.ingest(input, None, None, mime.as_deref())?,
                BlobStorage::Cdc => blobs.ingest_cdc(input, None, None, mime.as_deref())?,
            };

            let mut entry = Entry::new(path.clone(), Lane::Media, Kind::File);
            entry.blob_hash = Some(ingested.blob.hash.clone());
            entry.mime = ingested.blob.mime.clone();
            entry.size = Some(ingested.blob.size);
            let revision = catalog.entry_put(&entry)?;

            let how = if ingested.deduplicated {
                "deduplicated"
            } else {
                "uploaded"
            };
            println!(
                "put {path}: {} {} bytes {} rev {revision} ({how})",
                ingested.blob.hash,
                ingested.blob.size,
                ingested.blob.storage.as_str()
            );
        }
        BlobCommand::Presign { hash, expires } => {
            match workspace
                .blobs()?
                .presign_ingest(&hash, Duration::from_secs(expires))?
            {
                UploadTicket::Existing(ingested) => {
                    println!(
                        "exists {} ({} bytes)",
                        ingested.blob.hash, ingested.blob.size
                    );
                }
                UploadTicket::Presigned { key, url, method, headers, expires_in } => {
                    println!("ticket: {key}");
                    println!("method: {method}");
                    println!("url: {url}");
                    for (name, value) in headers {
                        println!("header: {name}: {value}");
                    }
                    println!("expires: {expires_in}");
                }
            }
        }
        BlobCommand::Commit { path, ticket, hash, size, mime } => {
            let ingested =
                workspace
                    .blobs()?
                    .commit_presigned(&ticket, size, &hash, mime.as_deref())?;
            let mut entry = Entry::new(path.clone(), Lane::Media, Kind::File);
            entry.blob_hash = Some(ingested.blob.hash.clone());
            entry.mime = ingested.blob.mime.clone();
            entry.size = Some(ingested.blob.size);
            let revision = catalog.entry_put(&entry)?;
            let how = if ingested.deduplicated {
                "deduplicated"
            } else {
                "committed"
            };
            println!(
                "put {path}: {} {} bytes {} rev {revision} ({how})",
                ingested.blob.hash,
                ingested.blob.size,
                ingested.blob.storage.as_str()
            );
        }
        BlobCommand::Get { path, out, offset, limit } => {
            let entry = catalog
                .entry_get(&path)?
                .ok_or_else(|| anyhow::anyhow!("no entry at {path:?}"))?;
            let hash = entry
                .blob_hash
                .ok_or_else(|| anyhow::anyhow!("entry {path:?} has no blob"))?;
            let data = workspace.blobs()?.read_range(&hash, offset, limit)?;
            fs::write(&out, &data).with_context(|| format!("write {}", out.display()))?;
            println!("read {path}: {} bytes at offset {offset}", data.len());
        }
        BlobCommand::SweepStaging { older_than } => {
            let removed = workspace
                .blobs()?
                .sweep_staging(Duration::from_secs(older_than))?;
            println!("swept {} staging objects", removed.len());
            for key in removed {
                println!("{key}");
            }
        }
        BlobCommand::Gc => {
            let removed = workspace.blobs()?.sweep_unreferenced()?;
            println!("deleted {} unreferenced blobs", removed.len());
            for hash in removed {
                println!("{hash}");
            }
        }
        BlobCommand::GcCdc => {
            let (removed, stats) = workspace.gc_archive(config)?;
            println!("deleted {} unreferenced cdc blobs", removed.len());
            for hash in removed {
                println!("{hash}");
            }
            println!(
                "archive gc: {} -> {} pages ({} reclaimed), {} chunks / {} bytes copied",
                stats.before.pages,
                stats.after.pages,
                stats.pages_reclaimed(),
                stats.copied_chunks,
                stats.copied_bytes
            );
            println!("old archive volume kept; run `workspace sync` to publish the new one");
        }
        BlobCommand::Ls => {
            for blob in catalog.blob_list()? {
                let pinned = if blob.pinned { "pinned" } else { "-" };
                println!(
                    "{}\t{}\t{}\t{}",
                    blob.hash,
                    blob.size,
                    blob.storage.as_str(),
                    pinned
                );
            }
        }
        BlobCommand::Show { hash } => {
            let blob = catalog
                .blob_get(&hash)?
                .ok_or_else(|| anyhow::anyhow!("no blob {hash:?}"))?;
            print_blob(&blob);
        }
    }
    Ok(())
}

fn metrics(workspace: &Workspace) -> Result<()> {
    let metrics = crate::metrics::collect(workspace)?;
    println!("workspace_oplog_docs {}", metrics.oplog_docs);
    println!("workspace_oplog_frames {}", metrics.oplog_frames);
    println!("workspace_oplog_bytes {}", metrics.oplog_bytes);
    for storage in &metrics.storage {
        println!(
            "workspace_blobs{{storage=\"{}\"}} {}",
            storage.storage, storage.blobs
        );
        println!(
            "workspace_blob_bytes{{storage=\"{}\"}} {}",
            storage.storage, storage.bytes
        );
    }
    println!("workspace_pinned_blobs {}", metrics.pinned_blobs);
    println!("workspace_releases {}", metrics.releases);
    println!("workspace_blob_ingested_total {}", metrics.ingested_blobs);
    println!(
        "workspace_blob_ingested_bytes_total {}",
        metrics.ingested_bytes
    );
    println!("workspace_blob_swept_total {}", metrics.swept_blobs);
    println!("workspace_blob_swept_bytes_total {}", metrics.swept_bytes);
    println!(
        "workspace_release_stage_seconds_count {}",
        metrics.release_stages
    );
    println!(
        "workspace_release_stage_seconds_total {:.3}",
        metrics.release_stage_millis as f64 / 1000.0
    );
    Ok(())
}

fn service(
    config: &std::path::Path,
    command: ServiceCommand,
    data: Option<&std::path::Path>,
    remote: Option<&str>,
) -> Result<()> {
    match command {
        ServiceCommand::Generate { kind, interval, keep, name, out, exec } => {
            let exec = match exec {
                Some(exec) => exec,
                None => std::env::current_exe().context("locate the workspace executable")?,
            };
            let kind = kind.unwrap_or(if cfg!(target_os = "macos") {
                crate::service::ServiceKind::Launchd
            } else {
                crate::service::ServiceKind::Systemd
            });
            let written = crate::service::generate(&crate::service::ServiceOptions {
                kind,
                name: &name,
                interval_secs: interval,
                keep,
                out: &out,
                exec: &exec,
                config,
                data,
                remote,
            })?;
            for path in written {
                println!("wrote {}", path.display());
            }
            match kind {
                crate::service::ServiceKind::Systemd => {
                    println!(
                        "next: systemctl daemon-reload && systemctl enable --now {name}.timer"
                    );
                }
                crate::service::ServiceKind::Launchd => {
                    println!(
                        "next: cp {out}/com.graft.{name}.plist ~/Library/LaunchAgents/ && launchctl load ~/Library/LaunchAgents/com.graft.{name}.plist",
                        out = out.display()
                    );
                }
            }
        }
    }
    Ok(())
}

fn backup(
    workspace: &Workspace,
    config: &WorkspaceConfig,
    prefix: Option<&str>,
    every: Option<u64>,
    keep: Option<usize>,
) -> Result<()> {
    let Some(every) = every else {
        backup_once(workspace, config, prefix, keep)?;
        return Ok(());
    };
    if every == 0 {
        bail!("--every must be at least 1 second");
    }
    let keep = keep.unwrap_or(7);
    if keep == 0 {
        bail!("--keep must be at least 1");
    }
    if config.backup.is_none() {
        bail!("no backup target configured; add a [backup] section to workspace.toml");
    }

    loop {
        if let Err(err) = backup_once(workspace, config, prefix, Some(keep)) {
            eprintln!("workspace: scheduled backup failed: {err:#}");
        }
        std::thread::sleep(Duration::from_secs(every));
    }
}

fn backup_once(
    workspace: &Workspace,
    config: &WorkspaceConfig,
    prefix: Option<&str>,
    keep: Option<usize>,
) -> Result<()> {
    if keep == Some(0) {
        bail!("--keep must be at least 1");
    }
    let report = crate::backup::run(workspace, config, prefix)?;
    println!("backup: {}", report.prefix);
    println!(
        "catalog: {} {} bytes {}",
        report.catalog.key, report.catalog.size, report.catalog.hash
    );
    println!("code bundles: {}", report.code_bundles.len());
    for bundle in report.code_bundles {
        println!("{} {} bytes {}", bundle.key, bundle.size, bundle.hash);
    }
    println!("blobs: {}", report.blobs.len());
    if let Some(archive) = report.archive {
        println!(
            "encrypted archive: {} blob(s), volume {} log {}",
            archive.blobs, archive.volume, archive.log
        );
    }
    if report.skipped_cdc != 0 {
        println!(
            "skipped cdc blobs: {} (encrypted archive bytes stay in the archive volume)",
            report.skipped_cdc
        );
    }
    println!("document logs: {}", report.doc_logs.len());
    for doc in report.doc_logs {
        println!(
            "{} ({} frames{}) {} bytes {}",
            doc.doc_id,
            doc.frames,
            if doc.complete { "" } else { ", partial" },
            doc.object.size,
            doc.object.hash
        );
        if let Some(snapshot) = doc.snapshot {
            println!(
                "  snapshot: {} {} bytes {}",
                snapshot.key, snapshot.size, snapshot.hash
            );
        }
    }
    println!(
        "manifest: {} {} bytes {}",
        report.manifest.key, report.manifest.size, report.manifest.hash
    );
    if let Some(keep) = keep {
        let retention = crate::backup::prune(workspace, config, prefix, keep)?;
        println!(
            "retention: kept {} backup(s), pruned {}",
            retention.kept.len(),
            retention.pruned.len()
        );
        for prefix in retention.pruned {
            println!("  pruned {prefix}");
        }
        if let Some(gc) = retention.archive_gc {
            println!(
                "archive gc: removed {} cdc file(s), {} -> {} pages ({} reclaimed)",
                gc.files_removed,
                gc.stats.before.pages,
                gc.stats.after.pages,
                gc.stats.pages_reclaimed()
            );
            println!("old archive backup volume kept; the reference points at the new one");
        }
    }
    Ok(())
}

fn restore(workspace: &mut Workspace, config: &WorkspaceConfig, prefix: &str) -> Result<()> {
    let report = crate::backup::restore(workspace, config, prefix)?;
    println!("restored: {}", report.prefix);
    println!("workspace: {}", report.workspace);
    println!("entries: {}", report.entries);
    println!("blobs: {}", report.blobs);
    println!("documents: {}", report.docs);
    if report.missing_blobs != 0 {
        println!(
            "warning: {} catalog blob row(s) had no backup object",
            report.missing_blobs
        );
    }
    if report.skipped_cdc != 0 {
        println!(
            "warning: {} cdc blob(s) stayed encrypted in the archive volume",
            report.skipped_cdc
        );
    }
    if report.partial_docs != 0 {
        println!(
            "warning: {} compacted document(s) were not replayed",
            report.partial_docs
        );
    }
    Ok(())
}

fn update(
    repo_spec: Option<&str>,
    prefix: &str,
    ca_file: Option<&std::path::Path>,
    config: &crate::config::WorkspaceConfig,
    catalog_enabled: bool,
    command: UpdateCommand,
) -> Result<()> {
    match command {
        UpdateCommand::Publish {
            name,
            release,
            root,
            signing_key,
            compress,
            compress_level,
        } => {
            let compression_level = compress_level
                .or_else(|| compress.then_some(crate::update::DEFAULT_COMPRESSION_LEVEL));
            let repo = open_update_repo(repo_spec, prefix, ca_file, &name)?;
            let signing = signing_key
                .as_deref()
                .map(crate::update::ManifestSigningKey::load)
                .transpose()?;
            let report = repo.publish_with_compression(
                &release,
                &root,
                signing.as_ref(),
                compression_level,
            )?;
            if let Some(workspace) = update_workspace(config, catalog_enabled)? {
                workspace
                    .catalog()
                    .update_release_put(&crate::catalog::UpdateRelease {
                        store: repo.store_label().to_string(),
                        prefix: repo.prefix().to_string(),
                        name: report.name.clone(),
                        release: report.release.clone(),
                        manifest_hash: report.manifest.clone(),
                        files: report.files as i64,
                        chunks: report.chunks as i64,
                        bytes: report.bytes as i64,
                        pinned: true,
                        created_at: 0,
                    })?;
            }
            println!(
                "published {}/{}: {}",
                report.name, report.release, report.manifest
            );
            if signing.is_some() {
                println!("signed: ed25519");
            }
            println!(
                "files: {}, chunks: {}, bytes: {}",
                report.files, report.chunks, report.bytes
            );
            println!(
                "uploaded chunks: {} ({} bytes), reused: {}",
                report.uploaded_chunks, report.uploaded_bytes, report.reused_chunks
            );
            if report.compressed_chunks != 0 {
                println!(
                    "compressed chunks: {} (zstd level {})",
                    report.compressed_chunks,
                    report
                        .compression_level
                        .unwrap_or(crate::update::DEFAULT_COMPRESSION_LEVEL)
                );
            }
        }
        UpdateCommand::Plan { name, release, state, public_key } => {
            let repo = open_update_repo(repo_spec, prefix, ca_file, &name)?;
            let trust = public_key
                .as_deref()
                .map(crate::update::ManifestPublicKey::load)
                .transpose()?;
            let installed = match state {
                Some(path) if path.exists() => Some(crate::update::read_state(&path, &name)?),
                Some(path) => bail!("installed state {} does not exist", path.display()),
                None => None,
            };
            let plan = repo.plan(&release, installed.as_ref(), trust.as_ref())?;
            println!("release: {name}/{release}");
            println!(
                "files: {} total, {} new, {} changed, {} unchanged, {} removed",
                plan.files_total,
                plan.files_new,
                plan.files_changed,
                plan.files_unchanged,
                plan.files_removed
            );
            println!(
                "chunks: {} total, {} missing; bytes: {} total, {} missing",
                plan.chunks_total, plan.chunks_missing, plan.bytes_total, plan.bytes_missing
            );
        }
        UpdateCommand::State { name, release, root, out } => {
            let manifest = crate::update::build_state(&root, &name, &release)?;
            crate::update::write_state(&out, &manifest)?;
            println!(
                "state {name}/{release}: {} files, {} chunks, {} bytes",
                manifest.files.len(),
                manifest.total_chunks(),
                manifest.total_bytes()
            );
            println!("wrote {}", out.display());
        }
        UpdateCommand::Apply {
            name,
            release,
            root,
            state,
            delete,
            public_key,
        } => {
            let repo = open_update_repo(repo_spec, prefix, ca_file, &name)?;
            let trust = public_key
                .as_deref()
                .map(crate::update::ManifestPublicKey::load)
                .transpose()?;
            let installed = if state.exists() {
                Some(crate::update::read_state(&state, &name)?)
            } else {
                None
            };
            let report = repo.apply(
                &release,
                &root,
                &state,
                installed.as_ref(),
                delete,
                trust.as_ref(),
                None,
            )?;
            println!(
                "applied {}/{} to {}",
                report.name,
                report.release,
                root.display()
            );
            println!(
                "files: {} written, {} unchanged, {} deleted",
                report.files_written, report.files_unchanged, report.files_deleted
            );
            println!(
                "chunks: {} downloaded ({} bytes), {} reused ({} bytes)",
                report.chunks_downloaded,
                report.bytes_downloaded,
                report.chunks_reused,
                report.bytes_reused
            );
            if report.chunks_resumed != 0 {
                println!(
                    "resumed: {} chunks ({} bytes) from interrupted staging",
                    report.chunks_resumed, report.bytes_resumed
                );
            }
            println!("state: {}", state.display());
        }
        UpdateCommand::Ls { name } => {
            let repo = open_update_repo(repo_spec, prefix, ca_file, &name)?;
            for release in repo.releases()? {
                let signed = if release.signatures == 0 {
                    "unsigned"
                } else {
                    "signed"
                };
                println!(
                    "{}\t{}\t{}\t{}",
                    release.release, release.manifest, signed, release.signatures
                );
            }
        }
        UpdateCommand::Keygen { private_key, public_key } => {
            crate::update::generate_keypair(&private_key, &public_key)?;
            println!("private key: {}", private_key.display());
            println!("public key: {}", public_key.display());
        }
        UpdateCommand::Gc { name, grace_secs, dry_run } => {
            let repo = open_update_repo(repo_spec, prefix, ca_file, &name)?;
            let pins = match update_workspace(config, catalog_enabled)? {
                Some(workspace) => workspace.catalog().update_release_pins(
                    repo.store_label(),
                    repo.prefix(),
                    repo.name(),
                )?,
                None => Vec::new(),
            };
            let report = repo.gc_with_pins(grace_secs, dry_run, &pins)?;
            let action = if report.dry_run {
                "would remove"
            } else {
                "removed"
            };
            println!(
                "{action} {} manifest(s) and {} chunk(s), {} bytes",
                report.manifests_removed, report.chunks_removed, report.bytes_removed
            );
        }
        UpdateCommand::PruneRefs { name, keep, grace_secs, dry_run } => {
            let repo = open_update_repo(repo_spec, prefix, ca_file, &name)?;
            let report = repo.prune_refs(keep, grace_secs, dry_run)?;
            if !report.dry_run
                && let Some(workspace) = update_workspace(config, catalog_enabled)?
            {
                for release in &report.removed {
                    workspace.catalog().update_release_remove(
                        repo.store_label(),
                        repo.prefix(),
                        repo.name(),
                        release,
                    )?;
                }
            }
            let action = if report.dry_run {
                "would remove"
            } else {
                "removed"
            };
            println!(
                "{action} {} ref(s); kept {}",
                report.removed.len(),
                report.kept.len()
            );
            for release in report.removed {
                println!("  {release}");
            }
        }
        UpdateCommand::Deploy { name, release, root, public_key } => {
            #[cfg(unix)]
            {
                let repo = open_update_repo(repo_spec, prefix, ca_file, &name)?;
                let trust = public_key
                    .as_deref()
                    .map(crate::update::ManifestPublicKey::load)
                    .transpose()?;
                let report = crate::deployment::deploy(&repo, &release, &root, trust.as_ref())?;
                println!("deployment: {}", report.deployment);
                println!("current: {}", root.join("current").display());
                if let Some(previous) = &report.previous {
                    println!("previous: {previous}");
                }
                if report.reused {
                    println!("reused existing deployment");
                } else if let Some(applied) = &report.applied {
                    println!(
                        "files: {} written, {} unchanged; chunks: {} downloaded ({} bytes)",
                        applied.files_written,
                        applied.files_unchanged,
                        applied.chunks_downloaded,
                        applied.bytes_downloaded
                    );
                }
            }
            #[cfg(not(unix))]
            {
                let _ = (name, release, root, public_key);
                bail!("atomic deployments need Unix symlinks");
            }
        }
        UpdateCommand::Rollback { root } => {
            #[cfg(unix)]
            {
                let report = crate::deployment::rollback(&root)?;
                println!("rolled back from {} to {}", report.from, report.to);
                println!("current: {}", root.join("current").display());
            }
            #[cfg(not(unix))]
            {
                let _ = root;
                bail!("atomic deployments need Unix symlinks");
            }
        }
        UpdateCommand::Deployments { root } => {
            #[cfg(unix)]
            {
                for deployment in crate::deployment::list(&root)? {
                    let marker = if deployment.current { "*" } else { " " };
                    println!(
                        "{marker}\t{}\t{}\t{}",
                        deployment.id, deployment.release, deployment.manifest
                    );
                }
            }
            #[cfg(not(unix))]
            {
                let _ = root;
                bail!("atomic deployments need Unix symlinks");
            }
        }
        UpdateCommand::PruneDeployments { root, keep, dry_run } => {
            #[cfg(unix)]
            {
                let report = crate::deployment::prune(&root, keep, dry_run)?;
                let action = if report.dry_run {
                    "would remove"
                } else {
                    "removed"
                };
                println!(
                    "{action} {} deployment(s), {} bytes; kept {}",
                    report.removed.len(),
                    report.bytes_removed,
                    report.kept.len()
                );
                for deployment in report.removed {
                    println!("  {deployment}");
                }
            }
            #[cfg(not(unix))]
            {
                let _ = (root, keep, dry_run);
                bail!("atomic deployments need Unix symlinks");
            }
        }
    }
    Ok(())
}

fn update_workspace(
    config: &crate::config::WorkspaceConfig,
    enabled: bool,
) -> Result<Option<Workspace>> {
    if !enabled {
        return Ok(None);
    }
    let workspace = Workspace::open(config)?;
    if workspace.catalog().workspace_id().is_none() {
        return Ok(None);
    }
    Ok(Some(workspace))
}

fn open_update_repo(
    spec: Option<&str>,
    prefix: &str,
    ca_file: Option<&std::path::Path>,
    name: &str,
) -> Result<crate::update::UpdateRepo> {
    let spec = spec.ok_or_else(|| {
        anyhow::anyhow!("--repo is required (for example `--repo fs:/srv/updates`)")
    })?;
    Ok(crate::update::UpdateRepo::from_locator_with_ca(
        spec, prefix, name, ca_file,
    )?)
}

fn release(workspace: &Workspace, command: ReleaseCommand) -> Result<()> {
    match command {
        ReleaseCommand::Stage { entries, out } => {
            let started = Instant::now();
            let staged = crate::release::stage(workspace, &entries, &out)?;
            let millis = started.elapsed().as_millis() as i64;
            workspace
                .catalog()
                .metric_add("metric.release.stage.millis", millis)?;
            workspace
                .catalog()
                .metric_add("metric.release.stage.count", 1)?;
            println!(
                "staged {} resources into {}",
                staged.resources.len(),
                staged.dir.display()
            );
            println!("manifest: {}", staged.manifest_hash);
            for resource in staged.resources {
                println!(
                    "{}\t{}\t{} bytes",
                    resource.hash, resource.path, resource.bytes
                );
            }
        }
    }
    Ok(())
}

fn code(workspace: &Workspace, command: CodeCommand) -> Result<()> {
    match command {
        CodeCommand::Snapshot { dir, path } => {
            let snapshot = workspace.snapshot_code(&dir, &path)?;
            println!(
                "snapshot {path}: tree {}, {} files, {} bytes",
                snapshot.tree, snapshot.files, snapshot.bytes
            );
            if let Some(bundle) = snapshot.git_bundle {
                println!("git bundle: {bundle}");
            }
        }
        CodeCommand::Restore { tree, out } => {
            let blobs = workspace.blobs()?;
            let stats = crate::code::restore(&blobs, &tree, &out)?;
            println!(
                "restored {} files, {} bytes to {}",
                stats.files,
                stats.bytes,
                out.display()
            );
            if let Some(bundle) = stats.git_bundle {
                println!("git bundle blob: {bundle}");
            }
        }
    }
    Ok(())
}

fn doc(workspace: &Workspace, command: DocCommand) -> Result<()> {
    match command {
        DocCommand::Ls => {
            for summary in workspace.oplog().docs()? {
                println!(
                    "{}\t{}\t{}\t{}",
                    summary.doc_id, summary.peers, summary.frames, summary.bytes
                );
            }
        }
        DocCommand::Stats { doc } => {
            let summary = workspace
                .oplog()
                .doc_summary(&doc)?
                .ok_or_else(|| anyhow::anyhow!("no oplog for document {doc:?}"))?;
            println!("doc: {}", summary.doc_id);
            println!("peers: {}", summary.peers);
            println!("frames: {}", summary.frames);
            println!("bytes: {}", summary.bytes);
            let frontier = workspace.oplog().frontier(&doc)?;
            let frontier = frontier
                .iter()
                .map(|(peer, seq)| format!("{peer}={seq}"))
                .collect::<Vec<_>>()
                .join(",");
            println!("frontier: {frontier}");
        }
        DocCommand::Append { doc, peer, seq, file } => {
            let data = fs::read(&file)
                .with_context(|| format!("read frame payload {}", file.display()))?;
            match workspace.submit(&doc, &peer, seq, &data)? {
                AppendOutcome::Applied { peer, seq } => println!("applied {peer}:{seq}"),
                AppendOutcome::Duplicate { peer, seq } => {
                    println!("duplicate {peer}:{seq} (already applied)")
                }
                AppendOutcome::Gap { peer, expected } => {
                    println!("gap {peer}: expected {expected}")
                }
            }
        }
        DocCommand::Frames { doc, cursors, out_dir, snapshot_out } => {
            let cursors = parse_cursors(&cursors)?;
            let batch = workspace.join(&doc, &cursors)?;
            if let Some(snapshot) = &batch.snapshot {
                match snapshot_out {
                    Some(path) => {
                        fs::write(&path, &snapshot.data)
                            .with_context(|| format!("write {}", path.display()))?;
                        println!("snapshot {} {}", snapshot.data.len(), path.display());
                    }
                    None => println!("snapshot {} bytes", snapshot.data.len()),
                }
            }
            if let Some(dir) = out_dir {
                fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
                for frame in &batch.frames {
                    let path = dir.join(format!("{}-{}.bin", frame.peer, frame.seq));
                    fs::write(&path, &frame.data)
                        .with_context(|| format!("write {}", path.display()))?;
                    println!("{} {} {}", frame.peer, frame.seq, path.display());
                }
            } else {
                for frame in &batch.frames {
                    println!("{} {} {}", frame.peer, frame.seq, frame.data.len());
                }
            }
        }
        DocCommand::Ack { doc, producer, consumer, seq } => {
            let acked = workspace.ack(&doc, &producer, &consumer, seq)?;
            println!("acked {producer} through {acked} for {consumer}");
        }
        DocCommand::Compact { doc, producer, through } => {
            match workspace.compact_document(&doc, &producer, through)? {
                CompactOutcome::Compacted { through, frames } => {
                    println!("compacted {producer} through {through}: {frames} frames deleted")
                }
                CompactOutcome::Blocked { consumer, acked, through } => println!(
                    "blocked: {consumer} has acked {producer} through {acked}, needs {through}"
                ),
                CompactOutcome::Nothing => println!("nothing to compact"),
            }
        }
        DocCommand::Snapshot { doc, file } => {
            let frontier =
                fs::read(&file).with_context(|| format!("read frontier {}", file.display()))?;
            let updated = workspace.note_snapshot(&doc, &frontier)?;
            println!("snapshot {doc}: updated {updated} entries");
        }
    }
    Ok(())
}

/// Parse repeated `PEER=SEQ` cursors into `(peer, seq)` pairs.
fn parse_cursors(cursors: &[String]) -> Result<Vec<(String, i64)>> {
    let mut parsed = Vec::with_capacity(cursors.len());
    for cursor in cursors {
        let (peer, seq) = cursor
            .split_once('=')
            .with_context(|| format!("cursor {cursor:?}: expected PEER=SEQ"))?;
        let seq: i64 = seq
            .parse()
            .with_context(|| format!("cursor {cursor:?}: invalid sequence"))?;
        parsed.push((peer.to_string(), seq));
    }
    Ok(parsed)
}

fn print_entry(entry: &Entry) {
    println!("path: {}", entry.path);
    println!("lane: {}", entry.lane.as_str());
    println!("kind: {}", entry.kind.as_str());
    println!("revision: {}", entry.revision);
    println!("updated_at: {}", entry.updated_at);
    println!("blob: {}", entry.blob_hash.as_deref().unwrap_or("-"));
    println!("loro_doc: {}", entry.loro_doc_id.as_deref().unwrap_or("-"));
    println!(
        "loro_frontier: {}",
        entry
            .loro_frontier
            .as_deref()
            .map_or_else(|| "-".to_string(), hex)
    );
    println!("code_tree: {}", entry.code_tree.as_deref().unwrap_or("-"));
    println!(
        "release_doi: {}",
        entry.release_doi.as_deref().unwrap_or("-")
    );
    println!("mime: {}", entry.mime.as_deref().unwrap_or("-"));
    println!(
        "size: {}",
        entry
            .size
            .map_or_else(|| "-".to_string(), |size| size.to_string())
    );
}

fn print_blob(blob: &Blob) {
    println!("hash: {}", blob.hash);
    println!("sha256: {}", blob.sha256.as_deref().unwrap_or("-"));
    println!("size: {}", blob.size);
    println!("mime: {}", blob.mime.as_deref().unwrap_or("-"));
    println!("storage: {}", blob.storage.as_str());
    println!("locator: {}", blob.locator);
    println!("refcount: {}", blob.refcount);
    println!("pinned: {}", blob.pinned);
    println!("created_at: {}", blob.created_at);
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
