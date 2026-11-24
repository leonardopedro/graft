//! Loading `workspace.toml` and the environment into a [`WorkspaceConfig`].
//!
//! Precedence is command line, then environment (`WORKSPACE_*`, with `__` for
//! nesting), then the config file, then defaults. The file is `workspace.toml`
//! in the working directory unless `--config` names another path.

use std::{
    num::NonZero,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use config::{Config, Environment, File};
use graft::{remote::RemoteConfig, setup::GraftConfig};
use serde::Deserialize;

fn default_data_dir() -> PathBuf {
    PathBuf::from("./workspace-data")
}

fn default_catalog_tag() -> String {
    "ws-catalog".to_string()
}

fn default_backup_prefix() -> String {
    "backup".to_string()
}

/// Object storage for raw media. Deliberately separate from the Graft remote:
/// Graft owns its bucket prefix (`logs/`, `segments/`), so media objects must
/// not share it, and lifecycle policies differ.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaConfig {
    /// On-disk object store, useful for tests and single-machine setups.
    Fs { root: String },

    /// S3 compatible object store. Credentials come from the `AWS_*` env vars.
    S3Compatible {
        bucket: String,
        prefix: Option<String>,
    },
}

/// Where `workspace backup` writes catalog exports and code bundles. A
/// different bucket or root from media is the off-site target; the prefix keeps
/// backups from colliding with live objects when they share one.
#[derive(Debug, Clone, Deserialize)]
pub struct BackupConfig {
    pub store: MediaConfig,
    #[serde(default = "default_backup_prefix")]
    pub prefix: String,
    /// Optional Graft remote for encrypted CDC pages. The workspace archive
    /// key is reused, so this copy stays encrypted and code checkpoints can be
    /// restored. Keep it separate from the live workspace remote.
    #[serde(default)]
    pub archive: Option<ArchiveBackupConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveBackupConfig {
    pub remote: RemoteConfig,
    pub data_dir: PathBuf,
}

/// Encrypted, chunk-deduplicating storage for archive-class blobs.
///
/// The key is read from a file rather than the config so it does not end up in
/// `workspace.toml`, command lines, or environment dumps. The file holds one
/// 32-byte key as 64 hex characters.
#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveConfig {
    pub key_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceConfig {
    /// Graft remote. Defaults to an in-memory store, which does not survive
    /// the process; set this for anything real.
    #[serde(default)]
    pub remote: RemoteConfig,

    /// Local Graft data directory (the Fjall index and cache).
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Graft autosync interval in seconds.
    #[serde(default)]
    pub autosync: Option<NonZero<u64>>,

    /// Volume tag naming the catalog database inside the Graft volume.
    #[serde(default = "default_catalog_tag")]
    pub catalog_tag: String,

    /// Media object storage. Without it raw blob operations are unavailable.
    #[serde(default)]
    pub media: Option<MediaConfig>,

    /// Archive storage through `cdc-volume`. Without it the `cdc` storage
    /// class is unavailable.
    #[serde(default)]
    pub archive: Option<ArchiveConfig>,

    /// Off-site backup target. Without it `workspace backup` is unavailable.
    #[serde(default)]
    pub backup: Option<BackupConfig>,
}

impl WorkspaceConfig {
    pub fn load(
        config: Option<&Path>,
        data: Option<PathBuf>,
        remote: Option<&str>,
    ) -> Result<Self> {
        let mut builder = Config::builder();

        let path = match config {
            Some(path) => Some(path.to_path_buf()),
            None => {
                let default = PathBuf::from("workspace.toml");
                default.is_file().then_some(default)
            }
        };
        if let Some(path) = path {
            builder = builder.add_source(File::from(path).required(true));
        }
        builder = builder.add_source(
            Environment::with_prefix("WORKSPACE")
                .prefix_separator("_")
                .separator("__"),
        );

        let mut config: WorkspaceConfig = builder
            .build()
            .context("read workspace config")?
            .try_deserialize()
            .context("deserialize workspace config")?;

        if let Some(data) = data {
            config.data_dir = data;
        }
        if let Some(remote) = remote {
            config.remote = parse_remote(remote)?;
        }
        if config.catalog_tag.is_empty() {
            bail!("catalog_tag must not be empty");
        }

        Ok(config)
    }

    /// Volume tag for the oplog lane, derived from the catalog tag so one
    /// workspace never shares an oplog with another.
    pub fn oplog_tag(&self) -> String {
        format!("{}-oplog", self.catalog_tag)
    }

    /// Volume tag for the encrypted archive lane.
    pub fn archive_tag(&self) -> String {
        format!("{}-archive", self.catalog_tag)
    }

    pub fn graft_config(&self) -> GraftConfig {
        GraftConfig {
            remote: self.remote.clone(),
            data_dir: self.data_dir.clone(),
            autosync: self.autosync,
        }
    }

    /// A short, shell-friendly description of the remote.
    pub fn remote_label(&self) -> String {
        match &self.remote {
            RemoteConfig::Memory => "memory".to_string(),
            RemoteConfig::Fs { root } => format!("fs:{root}"),
            RemoteConfig::S3Compatible { bucket, prefix } => match prefix {
                Some(prefix) => format!("s3:{bucket}/{prefix}"),
                None => format!("s3:{bucket}"),
            },
        }
    }
}

/// Parse `memory`, `fs:DIR`, or `s3:BUCKET[/PREFIX]`.
///
/// S3 credentials, region and endpoint come from the `AWS_*` environment
/// variables that Graft's S3-compatible remote already reads.
pub fn parse_remote(spec: &str) -> Result<RemoteConfig> {
    if spec == "memory" {
        return Ok(RemoteConfig::Memory);
    }
    if let Some(root) = spec.strip_prefix("fs:") {
        if root.is_empty() {
            bail!("`fs:` needs a directory, e.g. fs:/var/lib/workspace-remote");
        }
        return Ok(RemoteConfig::Fs { root: root.to_string() });
    }
    if let Some(rest) = spec.strip_prefix("s3:") {
        if rest.is_empty() {
            bail!("`s3:` needs a bucket, e.g. s3:my-bucket/workspaces");
        }
        let (bucket, prefix) = match rest.split_once('/') {
            Some((bucket, prefix)) if !bucket.is_empty() => {
                (bucket.to_string(), Some(prefix.to_string()))
            }
            Some(_) => bail!("`s3:` needs a bucket before the prefix"),
            None => (rest.to_string(), None),
        };
        return Ok(RemoteConfig::S3Compatible { bucket, prefix });
    }
    bail!("unknown remote {spec:?}: expected memory, fs:DIR, or s3:BUCKET[/PREFIX]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_specs() {
        assert!(matches!(
            parse_remote("memory").unwrap(),
            RemoteConfig::Memory
        ));
        assert!(matches!(
            parse_remote("fs:/tmp/remote").unwrap(),
            RemoteConfig::Fs { root } if root == "/tmp/remote"
        ));
        assert!(matches!(
            parse_remote("s3:bucket/prefix").unwrap(),
            RemoteConfig::S3Compatible { bucket, prefix }
                if bucket == "bucket" && prefix.as_deref() == Some("prefix")
        ));
        assert!(matches!(
            parse_remote("s3:bucket").unwrap(),
            RemoteConfig::S3Compatible { prefix: None, .. }
        ));
        for bad in ["", "fs:", "s3:", "s3:/prefix", "http://example.com"] {
            assert!(parse_remote(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
