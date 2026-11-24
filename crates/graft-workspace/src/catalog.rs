//! The workspace catalog: a SQLite database stored in a Graft volume.
//!
//! The catalog is an index, not a peer lane. It records where each workspace
//! object lives (a blob hash, a Loro document, a code tree, a release DOI) and
//! nothing else. Object bytes are content addressed and written before the row
//! that references them, so a crash can leave an orphaned blob but never a row
//! without bytes.
//!
//! [`Catalog::open`] applies the canonical schema in `sql/catalog.sql`, which
//! the SQL fixtures under `tests/sql/` also read, so there is a single source
//! of truth for the DDL.

use std::{
    path::PathBuf,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::ValueEnum;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use thiserror::Error;

/// Schema version written to `PRAGMA user_version`.
pub const SCHEMA_VERSION: i64 = 1;

/// The canonical DDL, also read by `tests/sql/test_workspace_catalog.sql`.
pub const SCHEMA_SQL: &str = include_str!("../sql/catalog.sql");

/// Additive DDL applied to catalogs created before update releases were
/// catalogued. `PRAGMA user_version` stays at 1 because the change only adds a
/// table and does not rewrite existing rows.
const UPDATE_RELEASES_SQL: &str = "CREATE TABLE IF NOT EXISTS update_releases (
    workspace_id  TEXT NOT NULL,
    store         TEXT NOT NULL,
    prefix        TEXT NOT NULL,
    name          TEXT NOT NULL,
    release       TEXT NOT NULL,
    manifest_hash TEXT NOT NULL,
    files         INTEGER NOT NULL,
    chunks        INTEGER NOT NULL,
    bytes         INTEGER NOT NULL,
    pinned        INTEGER NOT NULL DEFAULT 1 CHECK (pinned IN (0, 1)),
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, store, prefix, name, release)
) STRICT;";

const ENTRY_COLUMNS: &str = "path, lane, kind, blob_hash, loro_doc_id, loro_frontier, \
                             code_tree, release_doi, mime, size, revision, updated_at";

const BLOB_COLUMNS: &str =
    "hash, sha256, size, mime, storage, locator, refcount, pinned, created_at";

/// A storage lane. The catalog is not one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Lane {
    /// Live documents: notes, specs, whiteboards (Loro).
    Loro,
    /// Versioned code: tree manifests, git bundles, later jj objects.
    Code,
    /// Photos, video, audio, datasets.
    Media,
    /// Frozen releases published to Zenodo.
    Archive,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Loro => "loro",
            Lane::Code => "code",
            Lane::Media => "media",
            Lane::Archive => "archive",
        }
    }

    pub fn parse(value: &str) -> Option<Lane> {
        match value {
            "loro" => Some(Lane::Loro),
            "code" => Some(Lane::Code),
            "media" => Some(Lane::Media),
            "archive" => Some(Lane::Archive),
            _ => None,
        }
    }
}

impl FromStr for Lane {
    type Err = UnknownValue;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Lane::parse(value).ok_or_else(|| UnknownValue(value.to_owned()))
    }
}

/// What an entry points at. `dir` entries have no blob of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    Doc,
    File,
    Dir,
    Dataset,
    Release,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Doc => "doc",
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::Dataset => "dataset",
            Kind::Release => "release",
        }
    }

    pub fn parse(value: &str) -> Option<Kind> {
        match value {
            "doc" => Some(Kind::Doc),
            "file" => Some(Kind::File),
            "dir" => Some(Kind::Dir),
            "dataset" => Some(Kind::Dataset),
            "release" => Some(Kind::Release),
            _ => None,
        }
    }
}

impl FromStr for Kind {
    type Err = UnknownValue;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Kind::parse(value).ok_or_else(|| UnknownValue(value.to_owned()))
    }
}

/// How a blob's bytes are stored: as a raw object (range readable) or through
/// the chunked, deduplicating `cdc-volume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BlobStorage {
    Raw,
    Cdc,
}

impl BlobStorage {
    pub fn as_str(self) -> &'static str {
        match self {
            BlobStorage::Raw => "raw",
            BlobStorage::Cdc => "cdc",
        }
    }

    pub fn parse(value: &str) -> Option<BlobStorage> {
        match value {
            "raw" => Some(BlobStorage::Raw),
            "cdc" => Some(BlobStorage::Cdc),
            _ => None,
        }
    }
}

/// A catalog row. `revision` and `updated_at` are assigned by the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub lane: Lane,
    pub kind: Kind,
    pub blob_hash: Option<String>,
    pub loro_doc_id: Option<String>,
    /// Frontier of the last compacted Loro snapshot for this entry. Owned by
    /// [`Catalog::note_snapshot`]; upserts never clear it.
    pub loro_frontier: Option<Vec<u8>>,
    pub code_tree: Option<String>,
    pub release_doi: Option<String>,
    pub mime: Option<String>,
    pub size: Option<i64>,
    pub revision: i64,
    pub updated_at: i64,
}

impl Entry {
    pub fn new(path: impl Into<String>, lane: Lane, kind: Kind) -> Entry {
        Entry {
            path: path.into(),
            lane,
            kind,
            blob_hash: None,
            loro_doc_id: None,
            loro_frontier: None,
            code_tree: None,
            release_doi: None,
            mime: None,
            size: None,
            revision: 0,
            updated_at: 0,
        }
    }
}

/// A blob row about to be inserted. Contents live in object storage; this is
/// only the index record.
#[derive(Debug, Clone)]
pub struct NewBlob {
    pub hash: String,
    pub sha256: Option<String>,
    pub size: i64,
    pub mime: Option<String>,
    pub storage: BlobStorage,
    pub locator: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub hash: String,
    pub sha256: Option<String>,
    pub size: i64,
    pub mime: Option<String>,
    pub storage: BlobStorage,
    pub locator: String,
    pub refcount: i64,
    pub pinned: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRelease {
    pub store: String,
    pub prefix: String,
    pub name: String,
    pub release: String,
    pub manifest_hash: String,
    pub files: i64,
    pub chunks: i64,
    pub bytes: i64,
    pub pinned: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogStatus {
    pub workspace_id: Option<String>,
    pub schema_version: i64,
    pub entries: i64,
    pub blobs: i64,
    pub pinned_blobs: i64,
    pub releases: i64,
}

#[derive(Debug, Error)]
pub enum CatalogErr {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("catalog schema version {found} is not supported (expected {supported})")]
    UnsupportedSchema { found: i64, supported: i64 },

    #[error("invalid entry path {path:?}: {reason}")]
    InvalidPath { path: String, reason: &'static str },

    #[error("invalid blob hash {0:?}: expected blake3:<hex>")]
    InvalidHash(String),

    #[error("catalog export path {0:?} is not valid UTF-8 or contains a quote")]
    ExportPath(PathBuf),

    #[error("refusing to restore over a non-empty catalog")]
    RestoreNotEmpty,

    #[error("backup catalog schema {found} is not supported (expected {supported})")]
    BackupSchema { found: i64, supported: i64 },

    #[error("catalog has no workspace id; run `workspace init` first")]
    NotInitialized,

    #[error("invalid Loro document id")]
    InvalidLoroDocId,

    #[error("invalid update release identity {0:?}")]
    InvalidUpdateRelease(String),

    #[error("catalog row has an unknown {0}")]
    CorruptRow(&'static str),
}

/// An open catalog. Exactly one process should hold the writer for a given
/// catalog volume; Graft enforces that through its volume snapshot check.
pub struct Catalog {
    conn: Connection,
    workspace_id: Option<String>,
}

impl Catalog {
    /// Open a connection to a catalog database, applying the schema if the
    /// database is new.
    pub fn open(conn: Connection) -> Result<Catalog, CatalogErr> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;

        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 0 {
            conn.execute_batch(SCHEMA_SQL)?;
        } else {
            conn.execute_batch(UPDATE_RELEASES_SQL)?;
        }

        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(CatalogErr::UnsupportedSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }

        let workspace_id = conn
            .query_row(
                "SELECT value FROM catalog_meta WHERE key = 'workspace_id'",
                [],
                |row| row.get(0),
            )
            .optional()?;

        Ok(Catalog { conn, workspace_id })
    }

    /// Assign a workspace id if the catalog does not have one yet, and return
    /// it. Idempotent: re-running `init` keeps the existing id.
    pub fn init(&mut self) -> Result<String, CatalogErr> {
        if let Some(id) = &self.workspace_id {
            return Ok(id.clone());
        }

        let id = new_workspace_id();
        self.conn.execute(
            "INSERT INTO catalog_meta (key, value) VALUES ('workspace_id', ?1)",
            [&id],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO catalog_meta (key, value) VALUES ('created_at', ?1)",
            [now_unix().to_string()],
        )?;
        self.workspace_id = Some(id.clone());
        Ok(id)
    }

    pub fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }

    /// Insert or update an entry. Returns the new revision, which starts at 1
    /// and increases on every upsert.
    pub fn entry_put(&self, entry: &Entry) -> Result<i64, CatalogErr> {
        validate_path(&entry.path)?;
        let workspace = self.workspace()?;
        let now = now_unix();

        let revision = self.conn.query_row(
            "INSERT INTO entries (
                 workspace_id, path, lane, kind, blob_hash, loro_doc_id, code_tree,
                 release_doi, mime, size, revision, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11)
             ON CONFLICT (workspace_id, path) DO UPDATE SET
                 lane = excluded.lane,
                 kind = excluded.kind,
                 blob_hash = excluded.blob_hash,
                 loro_doc_id = excluded.loro_doc_id,
                 code_tree = excluded.code_tree,
                 release_doi = excluded.release_doi,
                 mime = excluded.mime,
                 size = excluded.size,
                 revision = entries.revision + 1,
                 deleted_at = NULL,
                 updated_at = excluded.updated_at
             RETURNING revision",
            params![
                workspace,
                entry.path,
                entry.lane.as_str(),
                entry.kind.as_str(),
                entry.blob_hash,
                entry.loro_doc_id,
                entry.code_tree,
                entry.release_doi,
                entry.mime,
                entry.size,
                now,
            ],
            |row| row.get(0),
        )?;

        Ok(revision)
    }

    pub fn entry_get(&self, path: &str) -> Result<Option<Entry>, CatalogErr> {
        validate_path(path)?;
        let workspace = self.workspace()?;
        let sql =
            format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE workspace_id = ?1 AND path = ?2");
        let raw = self
            .conn
            .query_row(&sql, params![workspace, path], raw_entry)
            .optional()?;
        raw.map(RawEntry::into_entry).transpose()
    }

    /// List live entries under `prefix` in path order.
    pub fn entry_list(
        &self,
        prefix: &str,
        include_deleted: bool,
    ) -> Result<Vec<Entry>, CatalogErr> {
        let workspace = self.workspace()?;
        let sql = format!(
            "SELECT {ENTRY_COLUMNS} FROM entries
             WHERE workspace_id = ?1
               AND (?2 OR deleted_at IS NULL)
               AND path LIKE ?3 ESCAPE '\\'
             ORDER BY path"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![workspace, include_deleted, like_prefix(prefix)],
            raw_entry,
        )?;

        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?.into_entry()?);
        }
        Ok(entries)
    }

    /// Soft delete an entry. Returns false if there was no live entry.
    pub fn entry_remove(&self, path: &str) -> Result<bool, CatalogErr> {
        validate_path(path)?;
        let workspace = self.workspace()?;
        let now = now_unix();
        let changed = self.conn.execute(
            "UPDATE entries
                SET deleted_at = ?1, revision = revision + 1, updated_at = ?1
              WHERE workspace_id = ?2 AND path = ?3 AND deleted_at IS NULL",
            params![now, workspace, path],
        )?;
        Ok(changed > 0)
    }

    /// Record a blob. Content addressing makes this an idempotent upsert;
    /// `refcount` and `pinned` are owned by the catalog, not by callers.
    pub fn blob_upsert(&self, blob: &NewBlob) -> Result<(), CatalogErr> {
        validate_hash(&blob.hash)?;
        self.conn.execute(
            "INSERT INTO blobs (hash, sha256, size, mime, storage, locator, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (hash) DO UPDATE SET
                 sha256 = COALESCE(excluded.sha256, blobs.sha256),
                 size = excluded.size,
                 mime = COALESCE(excluded.mime, blobs.mime),
                 storage = excluded.storage,
                 locator = excluded.locator",
            params![
                blob.hash,
                blob.sha256,
                blob.size,
                blob.mime,
                blob.storage.as_str(),
                blob.locator,
                now_unix(),
            ],
        )?;
        Ok(())
    }

    pub fn blob_get(&self, hash: &str) -> Result<Option<Blob>, CatalogErr> {
        let sql = format!("SELECT {BLOB_COLUMNS} FROM blobs WHERE hash = ?1");
        let raw = self.conn.query_row(&sql, [hash], raw_blob).optional()?;
        raw.map(RawBlob::into_blob).transpose()
    }

    pub fn blob_list(&self) -> Result<Vec<Blob>, CatalogErr> {
        let sql = format!("SELECT {BLOB_COLUMNS} FROM blobs ORDER BY created_at, hash");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], raw_blob)?;
        let mut blobs = Vec::new();
        for row in rows {
            blobs.push(row?.into_blob()?);
        }
        Ok(blobs)
    }

    /// Raw blobs that no entry references and that no release has pinned.
    /// Soft-deleted entries still count as references, so a release can keep
    /// pointing at a removed path.
    pub fn unreferenced_blobs(&self) -> Result<Vec<Blob>, CatalogErr> {
        let sql = format!(
            "SELECT {BLOB_COLUMNS} FROM blobs
              WHERE pinned = 0
                AND storage = 'raw'
                AND NOT EXISTS (
                    SELECT 1 FROM entries WHERE entries.blob_hash = blobs.hash
                )
              ORDER BY created_at, hash"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], raw_blob)?;
        let mut blobs = Vec::new();
        for row in rows {
            blobs.push(row?.into_blob()?);
        }
        Ok(blobs)
    }

    /// CDC blobs that no entry references and that no release has pinned.
    pub fn unreferenced_cdc_blobs(&self) -> Result<Vec<Blob>, CatalogErr> {
        let sql = format!(
            "SELECT {BLOB_COLUMNS} FROM blobs
              WHERE pinned = 0
                AND storage = 'cdc'
                AND NOT EXISTS (
                    SELECT 1 FROM entries WHERE entries.blob_hash = blobs.hash
                )
              ORDER BY created_at, hash"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], raw_blob)?;
        let mut blobs = Vec::new();
        for row in rows {
            blobs.push(row?.into_blob()?);
        }
        Ok(blobs)
    }

    /// Pin a blob so garbage collection cannot remove it. Releases and code
    /// checkpoints pin everything they still name.
    pub fn blob_pin(&self, hash: &str) -> Result<(), CatalogErr> {
        self.conn
            .execute("UPDATE blobs SET pinned = 1 WHERE hash = ?1", [hash])?;
        Ok(())
    }

    /// Forget a blob row. Callers must have removed the object first.
    pub fn blob_delete(&self, hash: &str) -> Result<bool, CatalogErr> {
        let changed = self
            .conn
            .execute("DELETE FROM blobs WHERE hash = ?1", [hash])?;
        Ok(changed > 0)
    }

    /// Add `delta` to a cumulative counter in `catalog_meta`.
    pub fn metric_add(&self, name: &str, delta: i64) -> Result<(), CatalogErr> {
        self.conn.execute(
            "INSERT INTO catalog_meta (key, value) VALUES (?1, CAST(?2 AS TEXT))
             ON CONFLICT (key) DO UPDATE SET
                 value = CAST(CAST(catalog_meta.value AS INTEGER) + ?2 AS TEXT)",
            params![name, delta],
        )?;
        Ok(())
    }

    pub fn metric(&self, name: &str) -> Result<i64, CatalogErr> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM catalog_meta WHERE key = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.and_then(|value| value.parse().ok()).unwrap_or(0))
    }

    /// Count and total bytes for each storage class.
    pub fn storage_stats(&self) -> Result<Vec<(String, i64, i64)>, CatalogErr> {
        let mut stmt = self.conn.prepare(
            "SELECT storage, COUNT(*), COALESCE(SUM(size), 0)
               FROM blobs GROUP BY storage ORDER BY storage",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        let mut stats = Vec::new();
        for row in rows {
            stats.push(row?);
        }
        Ok(stats)
    }

    /// Replace this empty catalog with the rows from an exported backup.
    ///
    /// The backup is attached, checked for the supported schema, and copied in
    /// one transaction. A non-empty catalog is refused so a restore cannot
    /// silently discard a live workspace.
    pub fn restore_from(&mut self, path: &std::path::Path) -> Result<(), CatalogErr> {
        let (entries, blobs, releases) = self.counts()?;
        if entries != 0 || blobs != 0 || releases != 0 {
            return Err(CatalogErr::RestoreNotEmpty);
        }
        // Open the export with a plain connection. Attaching it to this
        // connection would reuse the Graft VFS, which expects a volume rather
        // than an ordinary SQLite file.
        let backup = Connection::open(path)?;
        let version: i64 = backup.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 0 && version != SCHEMA_VERSION {
            return Err(CatalogErr::BackupSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }

        let has_update_releases: bool = backup.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
                            WHERE type = 'table' AND name = 'update_releases')",
            [],
            |row| row.get(0),
        )?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM entries", [])?;
        tx.execute("DELETE FROM releases", [])?;
        tx.execute("DELETE FROM update_releases", [])?;
        tx.execute("DELETE FROM blobs", [])?;
        tx.execute("DELETE FROM catalog_meta", [])?;
        copy_table(&backup, &tx, "blobs", 9)?;
        copy_table(&backup, &tx, "entries", 14)?;
        copy_table(&backup, &tx, "releases", 6)?;
        if has_update_releases {
            copy_table(&backup, &tx, "update_releases", 11)?;
        }
        copy_table(&backup, &tx, "catalog_meta", 2)?;
        tx.commit()?;
        self.workspace_id = self
            .conn
            .query_row(
                "SELECT value FROM catalog_meta WHERE key = 'workspace_id'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(())
    }

    fn counts(&self) -> Result<(i64, i64, i64), CatalogErr> {
        Ok(self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM entries),
                    (SELECT COUNT(*) FROM blobs),
                    (SELECT COUNT(*) FROM releases)
                      + (SELECT COUNT(*) FROM update_releases)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    }

    /// Write a consistent snapshot of the catalog database to `path` through
    /// Graft's `graft_export` pragma. The path is embedded in the pragma, so it
    /// must not contain a single quote that needs escaping in SQL.
    pub fn export_to(&self, path: &std::path::Path) -> Result<(), CatalogErr> {
        let text = path
            .to_str()
            .ok_or_else(|| CatalogErr::ExportPath(path.to_path_buf()))?;
        if text.contains('\'') {
            return Err(CatalogErr::ExportPath(path.to_path_buf()));
        }
        self.conn
            .execute_batch(&format!("PRAGMA graft_export = '{text}';"))?;
        Ok(())
    }

    /// Record or refresh an update release. The row pins its manifest until
    /// [`Self::update_release_remove`] forgets it.
    pub fn update_release_put(&self, release: &UpdateRelease) -> Result<(), CatalogErr> {
        validate_update_release(release)?;
        let workspace = self.workspace()?;
        self.conn.execute(
            "INSERT INTO update_releases (
                 workspace_id, store, prefix, name, release, manifest_hash,
                 files, chunks, bytes, pinned, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT (workspace_id, store, prefix, name, release) DO UPDATE SET
                 manifest_hash = excluded.manifest_hash,
                 files = excluded.files,
                 chunks = excluded.chunks,
                 bytes = excluded.bytes,
                 pinned = excluded.pinned",
            params![
                workspace,
                release.store,
                release.prefix,
                release.name,
                release.release,
                release.manifest_hash,
                release.files,
                release.chunks,
                release.bytes,
                release.pinned,
                if release.created_at == 0 {
                    now_unix()
                } else {
                    release.created_at
                },
            ],
        )?;
        Ok(())
    }

    pub fn update_release_get(
        &self,
        store: &str,
        prefix: &str,
        name: &str,
        release: &str,
    ) -> Result<Option<UpdateRelease>, CatalogErr> {
        let workspace = self.workspace()?;
        Ok(self
            .conn
            .query_row(
                "SELECT store, prefix, name, release, manifest_hash, files, chunks,
                        bytes, pinned, created_at
                   FROM update_releases
                  WHERE workspace_id = ?1 AND store = ?2 AND prefix = ?3
                    AND name = ?4 AND release = ?5",
                params![workspace, store, prefix, name, release],
                raw_update_release,
            )
            .optional()?
            .map(RawUpdateRelease::into_update_release))
    }

    pub fn update_release_list(
        &self,
        store: &str,
        prefix: &str,
        name: &str,
    ) -> Result<Vec<UpdateRelease>, CatalogErr> {
        let workspace = self.workspace()?;
        let mut stmt = self.conn.prepare(
            "SELECT store, prefix, name, release, manifest_hash, files, chunks,
                    bytes, pinned, created_at
               FROM update_releases
              WHERE workspace_id = ?1 AND store = ?2 AND prefix = ?3 AND name = ?4
              ORDER BY created_at, release",
        )?;
        let rows = stmt.query_map(params![workspace, store, prefix, name], raw_update_release)?;
        let mut releases = Vec::new();
        for row in rows {
            releases.push(row?.into_update_release());
        }
        Ok(releases)
    }

    /// Manifest hashes pinned for one repository.
    pub fn update_release_pins(
        &self,
        store: &str,
        prefix: &str,
        name: &str,
    ) -> Result<Vec<String>, CatalogErr> {
        let workspace = self.workspace()?;
        let mut stmt = self.conn.prepare(
            "SELECT manifest_hash FROM update_releases
              WHERE workspace_id = ?1 AND store = ?2 AND prefix = ?3
                AND name = ?4 AND pinned = 1
              ORDER BY manifest_hash",
        )?;
        let rows = stmt.query_map(params![workspace, store, prefix, name], |row| row.get(0))?;
        let mut hashes = Vec::new();
        for row in rows {
            hashes.push(row?);
        }
        Ok(hashes)
    }

    /// Forget one update release, unpinning its manifest.
    pub fn update_release_remove(
        &self,
        store: &str,
        prefix: &str,
        name: &str,
        release: &str,
    ) -> Result<bool, CatalogErr> {
        let workspace = self.workspace()?;
        let changed = self.conn.execute(
            "DELETE FROM update_releases
              WHERE workspace_id = ?1 AND store = ?2 AND prefix = ?3
                AND name = ?4 AND release = ?5",
            params![workspace, store, prefix, name, release],
        )?;
        Ok(changed > 0)
    }

    pub fn status(&self) -> Result<CatalogStatus, CatalogErr> {
        let workspace = self.workspace_id.as_deref().unwrap_or("");
        let (entries, blobs, pinned_blobs, releases) = self.conn.query_row(
            "SELECT
                 (SELECT COUNT(*) FROM entries WHERE workspace_id = ?1 AND deleted_at IS NULL),
                 (SELECT COUNT(*) FROM blobs),
                 (SELECT COUNT(*) FROM blobs WHERE pinned = 1),
                 (SELECT COUNT(*) FROM releases WHERE workspace_id = ?1)
                   + (SELECT COUNT(*) FROM update_releases WHERE workspace_id = ?1)",
            [workspace],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;

        Ok(CatalogStatus {
            workspace_id: self.workspace_id.clone(),
            schema_version: SCHEMA_VERSION,
            entries,
            blobs,
            pinned_blobs,
            releases,
        })
    }

    /// Record the frontier of a document's last compacted Loro snapshot on
    /// every entry that points at `loro_doc_id`. Called at snapshot
    /// boundaries, not per edit. Returns the number of entries updated.
    pub fn note_snapshot(&self, loro_doc_id: &str, frontier: &[u8]) -> Result<usize, CatalogErr> {
        if loro_doc_id.is_empty() {
            return Err(CatalogErr::InvalidLoroDocId);
        }
        let workspace = self.workspace()?;
        let changed = self.conn.execute(
            "UPDATE entries
                SET loro_frontier = ?1, updated_at = ?2
              WHERE workspace_id = ?3 AND loro_doc_id = ?4",
            params![frontier, now_unix(), workspace, loro_doc_id],
        )?;
        Ok(changed)
    }

    fn workspace(&self) -> Result<&str, CatalogErr> {
        self.workspace_id
            .as_deref()
            .ok_or(CatalogErr::NotInitialized)
    }
}

struct RawUpdateRelease {
    store: String,
    prefix: String,
    name: String,
    release: String,
    manifest_hash: String,
    files: i64,
    chunks: i64,
    bytes: i64,
    pinned: i64,
    created_at: i64,
}

impl RawUpdateRelease {
    fn into_update_release(self) -> UpdateRelease {
        UpdateRelease {
            store: self.store,
            prefix: self.prefix,
            name: self.name,
            release: self.release,
            manifest_hash: self.manifest_hash,
            files: self.files,
            chunks: self.chunks,
            bytes: self.bytes,
            pinned: self.pinned != 0,
            created_at: self.created_at,
        }
    }
}

fn raw_update_release(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawUpdateRelease> {
    Ok(RawUpdateRelease {
        store: row.get(0)?,
        prefix: row.get(1)?,
        name: row.get(2)?,
        release: row.get(3)?,
        manifest_hash: row.get(4)?,
        files: row.get(5)?,
        chunks: row.get(6)?,
        bytes: row.get(7)?,
        pinned: row.get(8)?,
        created_at: row.get(9)?,
    })
}

fn validate_update_release(release: &UpdateRelease) -> Result<(), CatalogErr> {
    for (label, value) in [
        ("store", &release.store),
        ("prefix", &release.prefix),
        ("name", &release.name),
        ("release", &release.release),
    ] {
        if value.is_empty() || value.contains('\0') {
            return Err(CatalogErr::InvalidUpdateRelease(label.to_string()));
        }
    }
    validate_hash(&release.manifest_hash)?;
    if release.files < 0 || release.chunks < 0 || release.bytes < 0 {
        return Err(CatalogErr::InvalidUpdateRelease(
            "negative size or count".to_string(),
        ));
    }
    Ok(())
}

struct RawEntry {
    path: String,
    lane: String,
    kind: String,
    blob_hash: Option<String>,
    loro_doc_id: Option<String>,
    loro_frontier: Option<Vec<u8>>,
    code_tree: Option<String>,
    release_doi: Option<String>,
    mime: Option<String>,
    size: Option<i64>,
    revision: i64,
    updated_at: i64,
}

impl RawEntry {
    fn into_entry(self) -> Result<Entry, CatalogErr> {
        Ok(Entry {
            path: self.path,
            lane: Lane::parse(&self.lane).ok_or(CatalogErr::CorruptRow("lane"))?,
            kind: Kind::parse(&self.kind).ok_or(CatalogErr::CorruptRow("kind"))?,
            blob_hash: self.blob_hash,
            loro_doc_id: self.loro_doc_id,
            loro_frontier: self.loro_frontier,
            code_tree: self.code_tree,
            release_doi: self.release_doi,
            mime: self.mime,
            size: self.size,
            revision: self.revision,
            updated_at: self.updated_at,
        })
    }
}

fn raw_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok(RawEntry {
        path: row.get(0)?,
        lane: row.get(1)?,
        kind: row.get(2)?,
        blob_hash: row.get(3)?,
        loro_doc_id: row.get(4)?,
        loro_frontier: row.get(5)?,
        code_tree: row.get(6)?,
        release_doi: row.get(7)?,
        mime: row.get(8)?,
        size: row.get(9)?,
        revision: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

struct RawBlob {
    hash: String,
    sha256: Option<String>,
    size: i64,
    mime: Option<String>,
    storage: String,
    locator: String,
    refcount: i64,
    pinned: i64,
    created_at: i64,
}

impl RawBlob {
    fn into_blob(self) -> Result<Blob, CatalogErr> {
        Ok(Blob {
            hash: self.hash,
            sha256: self.sha256,
            size: self.size,
            mime: self.mime,
            storage: BlobStorage::parse(&self.storage)
                .ok_or(CatalogErr::CorruptRow("blob storage"))?,
            locator: self.locator,
            refcount: self.refcount,
            pinned: self.pinned != 0,
            created_at: self.created_at,
        })
    }
}

fn raw_blob(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawBlob> {
    Ok(RawBlob {
        hash: row.get(0)?,
        sha256: row.get(1)?,
        size: row.get(2)?,
        mime: row.get(3)?,
        storage: row.get(4)?,
        locator: row.get(5)?,
        refcount: row.get(6)?,
        pinned: row.get(7)?,
        created_at: row.get(8)?,
    })
}

/// Copy every row of a trusted table name from one connection to another.
/// Values are passed as parameters, never spliced into SQL.
fn copy_table(
    source: &Connection,
    target: &Connection,
    table: &str,
    columns: usize,
) -> Result<(), CatalogErr> {
    let mut select = source.prepare(&format!("SELECT * FROM {table}"))?;
    let placeholders = (1..=columns)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut insert = target.prepare(&format!("INSERT INTO {table} VALUES ({placeholders})"))?;
    let mut rows = select.query([])?;
    while let Some(row) = rows.next()? {
        let values = (0..columns)
            .map(|index| row.get::<_, Value>(index))
            .collect::<Result<Vec<_>, _>>()?;
        insert.execute(params_from_iter(values))?;
    }
    Ok(())
}

/// Entry paths are workspace-relative, slash separated, and may not escape the
/// workspace or name a synthetic intermediate.
fn validate_path(path: &str) -> Result<(), CatalogErr> {
    let invalid = |reason| CatalogErr::InvalidPath { path: path.to_owned(), reason };

    if path.is_empty() {
        return Err(invalid("path is empty"));
    }
    if path.contains('\0') {
        return Err(invalid("path contains a NUL byte"));
    }
    if path.contains('\\') {
        return Err(invalid("use '/' as the separator"));
    }
    if path.starts_with('/') {
        return Err(invalid("path must be relative"));
    }
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(invalid("path has an empty segment"));
        }
        if segment == "." || segment == ".." {
            return Err(invalid("path may not contain '.' or '..'"));
        }
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<(), CatalogErr> {
    let Some(hex) = hash.strip_prefix("blake3:") else {
        return Err(CatalogErr::InvalidHash(hash.to_owned()));
    };
    if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CatalogErr::InvalidHash(hash.to_owned()));
    }
    Ok(())
}

/// Escape LIKE metacharacters and append the wildcard, so a prefix such as
/// `a_b` only matches literal paths.
fn like_prefix(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len() + 1);
    for ch in prefix.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// A fresh workspace id: `w-` plus 12 random bytes in base58.
fn new_workspace_id() -> String {
    let hi = rand::random::<u64>().to_le_bytes();
    let lo = rand::random::<u64>().to_le_bytes();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi);
    bytes[8..].copy_from_slice(&lo);
    format!("w-{}", bs58::encode(&bytes[..12]).into_string())
}

pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Debug, Error)]
#[error("unknown value {0:?}")]
pub struct UnknownValue(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        let conn = Connection::open_in_memory().unwrap();
        Catalog::open(conn).unwrap()
    }

    fn initialized() -> Catalog {
        let mut catalog = catalog();
        catalog.init().unwrap();
        catalog
    }

    fn entry(path: &str, lane: Lane, kind: Kind) -> Entry {
        Entry::new(path, lane, kind)
    }

    #[test]
    fn migrations_apply_once_and_record_version() {
        let catalog = catalog();
        assert_eq!(catalog.status().unwrap().entries, 0);
        // Opening an already-migrated catalog is not an error.
        let conn = catalog.conn;
        let mut reopened = Catalog::open(conn).unwrap();
        reopened.init().unwrap();
        assert!(reopened.workspace_id().is_some());
    }

    #[test]
    fn older_catalogs_gain_update_releases_on_open() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute("DROP TABLE update_releases", []).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let catalog = Catalog::open(conn).unwrap();
        let exists: bool = catalog
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master
                                WHERE type = 'table' AND name = 'update_releases')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists);
    }

    #[test]
    fn init_is_idempotent() {
        let mut catalog = catalog();
        let first = catalog.init().unwrap();
        let second = catalog.init().unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("w-"));
        assert!(first.len() > 4);
    }

    #[test]
    fn entries_round_trip_and_revisions_increase() {
        let catalog = initialized();
        let mut new = entry("notes/intro", Lane::Loro, Kind::Doc);
        new.loro_doc_id = Some("doc-1".into());

        assert_eq!(catalog.entry_put(&new).unwrap(), 1);
        let fetched = catalog.entry_get("notes/intro").unwrap().unwrap();
        assert_eq!(fetched.lane, Lane::Loro);
        assert_eq!(fetched.loro_doc_id.as_deref(), Some("doc-1"));
        assert!(fetched.updated_at > 0);

        new.mime = Some("text/markdown".into());
        assert_eq!(catalog.entry_put(&new).unwrap(), 2);
        let fetched = catalog.entry_get("notes/intro").unwrap().unwrap();
        assert_eq!(fetched.revision, 2);
        assert_eq!(fetched.mime.as_deref(), Some("text/markdown"));
    }

    #[test]
    fn list_filters_by_prefix_and_soft_delete() {
        let catalog = initialized();
        catalog
            .entry_put(&entry("notes/a", Lane::Loro, Kind::Doc))
            .unwrap();
        catalog
            .entry_put(&entry("notes/b", Lane::Loro, Kind::Doc))
            .unwrap();
        catalog
            .entry_put(&entry("assets/video", Lane::Media, Kind::File))
            .unwrap();

        let notes = catalog.entry_list("notes/", false).unwrap();
        assert_eq!(notes.len(), 2);
        assert!(notes.iter().all(|e| e.path.starts_with("notes/")));

        assert!(catalog.entry_remove("notes/a").unwrap());
        assert!(!catalog.entry_remove("notes/a").unwrap());
        assert_eq!(catalog.entry_list("notes/", false).unwrap().len(), 1);
        assert_eq!(catalog.entry_list("notes/", true).unwrap().len(), 2);

        // A removed path can be restored by putting it again.
        catalog
            .entry_put(&entry("notes/a", Lane::Loro, Kind::Doc))
            .unwrap();
        let restored = catalog.entry_get("notes/a").unwrap().unwrap();
        // put=1, soft delete=2, restored put=3.
        assert_eq!(restored.revision, 3);
        assert_eq!(catalog.entry_list("notes/", false).unwrap().len(), 2);
    }

    #[test]
    fn like_metacharacters_in_prefix_are_literal() {
        let catalog = initialized();
        catalog
            .entry_put(&entry("a_b/x", Lane::Loro, Kind::Doc))
            .unwrap();
        catalog
            .entry_put(&entry("axb/y", Lane::Loro, Kind::Doc))
            .unwrap();
        let hits = catalog.entry_list("a_b/", false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a_b/x");
    }

    #[test]
    fn rejects_paths_that_escape_the_workspace() {
        let catalog = initialized();
        for path in ["", "/abs", "a//b", "a/./b", "a/../b", "a\\b", "a/"] {
            let err = catalog
                .entry_put(&entry(path, Lane::Loro, Kind::Doc))
                .unwrap_err();
            assert!(
                matches!(err, CatalogErr::InvalidPath { .. }),
                "{path:?} produced {err:?}"
            );
        }
    }

    #[test]
    fn rejects_unknown_lane_at_the_database() {
        let catalog = initialized();
        let err = catalog
            .conn
            .execute(
                "INSERT INTO entries (workspace_id, path, lane, kind, updated_at)
                 VALUES ('ws', 'x', 'bogus', 'doc', 1)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
    }

    #[test]
    fn entry_paths_are_unique_per_workspace() {
        let mut catalog = initialized();
        let original = catalog.workspace_id().unwrap().to_owned();
        catalog
            .entry_put(&entry("shared/path", Lane::Loro, Kind::Doc))
            .unwrap();

        catalog.workspace_id = Some("w-other".into());
        catalog
            .entry_put(&entry("shared/path", Lane::Media, Kind::File))
            .unwrap();
        assert_eq!(catalog.entry_list("", false).unwrap().len(), 1);

        catalog.workspace_id = Some(original);
        assert_eq!(catalog.entry_list("", false).unwrap().len(), 1);
        let rows: i64 = catalog
            .conn
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2);
    }

    #[test]
    fn blob_lookup_and_pinning_are_reported() {
        let catalog = initialized();
        let blob = NewBlob {
            hash: "blake3:00ff".into(),
            sha256: None,
            size: 42,
            mime: Some("image/png".into()),
            storage: BlobStorage::Raw,
            locator: "media/raw/w-x/00ff".into(),
        };
        catalog.blob_upsert(&blob).unwrap();
        // Idempotent: re-recording the same content keeps one row.
        catalog.blob_upsert(&blob).unwrap();

        let fetched = catalog.blob_get("blake3:00ff").unwrap().unwrap();
        assert_eq!(fetched.size, 42);
        assert!(!fetched.pinned);

        catalog
            .conn
            .execute("UPDATE blobs SET pinned = 1", [])
            .unwrap();
        let status = catalog.status().unwrap();
        assert_eq!(status.blobs, 1);
        assert_eq!(status.pinned_blobs, 1);
    }

    #[test]
    fn entries_require_a_known_blob() {
        let catalog = initialized();
        let mut with_blob = entry("media/clip", Lane::Media, Kind::File);
        with_blob.blob_hash = Some("blake3:beef".into());
        let err = catalog.entry_put(&with_blob).unwrap_err();
        assert!(
            matches!(err, CatalogErr::Sqlite(ref e) if e.to_string().contains("FOREIGN KEY")),
            "{err}"
        );

        catalog
            .blob_upsert(&NewBlob {
                hash: "blake3:beef".into(),
                sha256: None,
                size: 1,
                mime: None,
                storage: BlobStorage::Raw,
                locator: "media/raw/beef".into(),
            })
            .unwrap();
        catalog.entry_put(&with_blob).unwrap();
    }

    #[test]
    fn rejects_malformed_blob_hashes() {
        let catalog = initialized();
        for hash in ["", "sha256:aa", "blake3:", "blake3:xyz", "blake3:abc"] {
            let err = catalog
                .blob_upsert(&NewBlob {
                    hash: hash.into(),
                    sha256: None,
                    size: 1,
                    mime: None,
                    storage: BlobStorage::Raw,
                    locator: "x".into(),
                })
                .unwrap_err();
            assert!(
                matches!(err, CatalogErr::InvalidHash(_)),
                "{hash:?} produced {err:?}"
            );
        }
    }

    #[test]
    fn snapshot_frontier_updates_matching_entries_only() {
        let catalog = initialized();
        let mut live = entry("notes/a", Lane::Loro, Kind::Doc);
        live.loro_doc_id = Some("doc-1".into());
        catalog.entry_put(&live).unwrap();
        catalog
            .entry_put(&entry("notes/b", Lane::Loro, Kind::Doc))
            .unwrap();

        assert_eq!(catalog.note_snapshot("doc-1", b"frontier").unwrap(), 1);
        let fetched = catalog.entry_get("notes/a").unwrap().unwrap();
        assert_eq!(fetched.loro_frontier.as_deref(), Some(&b"frontier"[..]));

        // Metadata edits must not drop the frontier.
        live.mime = Some("text/markdown".into());
        catalog.entry_put(&live).unwrap();
        let fetched = catalog.entry_get("notes/a").unwrap().unwrap();
        assert_eq!(fetched.loro_frontier.as_deref(), Some(&b"frontier"[..]));

        let untouched = catalog.entry_get("notes/b").unwrap().unwrap();
        assert_eq!(untouched.loro_frontier, None);
        assert!(matches!(
            catalog.note_snapshot("", b"x"),
            Err(CatalogErr::InvalidLoroDocId)
        ));
    }

    #[test]
    fn update_releases_pin_manifests_and_count_as_releases() {
        let catalog = initialized();
        let release = UpdateRelease {
            store: "fs:./repo".into(),
            prefix: "updates".into(),
            name: "app".into(),
            release: "1.0.0".into(),
            manifest_hash: format!("blake3:{}", "aa".repeat(32)),
            files: 2,
            chunks: 3,
            bytes: 40,
            pinned: true,
            created_at: 0,
        };
        catalog.update_release_put(&release).unwrap();
        catalog.update_release_put(&release).unwrap();
        let fetched = catalog
            .update_release_get("fs:./repo", "updates", "app", "1.0.0")
            .unwrap()
            .unwrap();
        assert_eq!(fetched.manifest_hash, release.manifest_hash);
        assert!(fetched.pinned);
        assert!(fetched.created_at > 0);
        assert_eq!(
            catalog
                .update_release_pins("fs:./repo", "updates", "app")
                .unwrap(),
            vec![release.manifest_hash]
        );
        assert_eq!(catalog.status().unwrap().releases, 1);
        assert_eq!(
            catalog
                .update_release_list("fs:./repo", "updates", "app")
                .unwrap()
                .len(),
            1
        );
        assert!(
            catalog
                .update_release_remove("fs:./repo", "updates", "app", "1.0.0")
                .unwrap()
        );
        assert_eq!(catalog.status().unwrap().releases, 0);
    }

    #[test]
    fn writes_require_init() {
        let catalog = catalog();
        let err = catalog
            .entry_put(&entry("a", Lane::Loro, Kind::Doc))
            .unwrap_err();
        assert!(matches!(err, CatalogErr::NotInitialized));
        assert_eq!(catalog.status().unwrap().entries, 0);
    }
}
