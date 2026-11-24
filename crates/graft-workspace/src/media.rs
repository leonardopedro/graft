//! The media lane: raw objects in a separate object store, ingested through
//! the coordinator and read back as ranges.
//!
//! Media deliberately lives outside the Graft remote. Graft owns its configured
//! bucket prefix (`logs/`, `segments/`); putting media keys beside them would
//! mix namespaces and lifecycle policies, and a rule aimed at media could
//! expire Graft's own segments.
//!
//! Ingest is streaming: bytes are hashed while they are written to a staging
//! object, so committing never re-reads the payload. The canonical object is
//! content addressed (`media/raw/<workspace>/<blake3 hex>`), which makes a
//! re-upload of identical bytes a catalog lookup instead of a copy.

use std::{
    collections::HashSet,
    env,
    io::{self, Read},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use blake3::Hasher as Blake3;
use opendal::{
    blocking::{Operator, Writer},
    options::ReadOptions,
    raw::BytesRange,
    services,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::archive::ArchiveStore;
use crate::catalog::{Blob, BlobStorage, Catalog, CatalogErr, NewBlob};

use crate::config::MediaConfig;

/// Read/write chunk for fallback copies and ingestion.
const CHUNK: usize = 64 * 1024;

const METRIC_INGESTED_BYTES: &str = "metric.blob.ingested.bytes";
const METRIC_INGESTED_COUNT: &str = "metric.blob.ingested.count";
const METRIC_SWEPT_BYTES: &str = "metric.blob.swept.bytes";
const METRIC_SWEPT_COUNT: &str = "metric.blob.swept.count";
const COPY_CHUNK: u64 = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum BlobErr {
    #[error("no media store configured; add a [media] section to workspace.toml")]
    NoMediaStore,

    #[error("no archive store configured; add an [archive] section to workspace.toml")]
    NoArchiveStore,

    #[error("workspace is not initialized; run `workspace init` first")]
    NotInitialized,

    #[error("blob {0:?} is not recorded in the catalog")]
    NotFound(String),

    #[error("archive key {path}: {reason}")]
    ArchiveKey { path: String, reason: String },

    #[error("archive: {0}")]
    Archive(String),

    #[error("cdc volume: {0}")]
    Cdc(#[from] cdc_volume::CdcErr),

    #[error("blob {hash:?} is {size} bytes; range {offset}+{len} is out of bounds")]
    RangeOutOfBounds {
        hash: String,
        offset: u64,
        len: u64,
        size: u64,
    },

    #[error("size mismatch: expected {expected} bytes, read {actual}")]
    SizeMismatch { expected: u64, actual: u64 },

    #[error("hash mismatch: expected {expected}, computed {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("invalid blob hash {0:?}: expected blake3:<hex>")]
    InvalidHash(String),

    #[error("object storage: {0}")]
    Store(#[from] opendal::Error),

    #[error("this media backend cannot presign uploads; use a streaming ingest")]
    PresignUnsupported,

    #[error("presign: {0}")]
    Presign(String),

    #[error("upload ticket {0:?} is not a staging key for this workspace")]
    BadTicket(String),

    #[error("upload ticket {0:?} has no object; the upload did not finish")]
    MissingUpload(String),

    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("catalog: {0}")]
    Catalog(#[from] CatalogErr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    /// Key relative to the media store root.
    pub key: String,
    pub size: u64,
    /// Seconds since the Unix epoch, when the backend reports it.
    pub modified: Option<i64>,
}

/// A presigned direct-upload request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresignedWrite {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
}

/// A blocking object store for media.
pub struct MediaStore {
    op: Operator,
    // The blocking operator borrows this runtime's handle, so the runtime has
    // to outlive every operation.
    _runtime: tokio::runtime::Runtime,
}

impl MediaStore {
    pub fn open(config: &MediaConfig) -> Result<MediaStore, BlobErr> {
        let async_op = match config {
            MediaConfig::Fs { root } => {
                opendal::Operator::new(services::Fs::default().root(root))?.finish()
            }
            MediaConfig::S3Compatible { bucket, prefix } => {
                let mut builder = services::S3::default().bucket(bucket);
                if let Some(prefix) = prefix {
                    builder = builder.root(prefix);
                }
                if let Ok(endpoint) = env::var("AWS_ENDPOINT_URL") {
                    builder = builder.endpoint(&endpoint);
                }
                if let Ok(key) = env::var("AWS_ACCESS_KEY_ID") {
                    builder = builder.access_key_id(&key);
                }
                if let Ok(secret) = env::var("AWS_SECRET_ACCESS_KEY") {
                    builder = builder.secret_access_key(&secret);
                }
                if let Ok(region) = env::var("AWS_REGION") {
                    builder = builder.region(&region);
                }
                opendal::Operator::new(builder)?.finish()
            }
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let op = {
            let _guard = runtime.enter();
            Operator::new(async_op)?
        };

        Ok(MediaStore { op, _runtime: runtime })
    }

    pub(crate) fn writer(&self, key: &str) -> Result<Writer, BlobErr> {
        Ok(self.op.writer(key)?)
    }

    /// Read `len` bytes at `offset` (`None` reads to the end).
    pub fn read_range(&self, key: &str, offset: u64, len: Option<u64>) -> Result<Vec<u8>, BlobErr> {
        let opts = ReadOptions {
            range: BytesRange::new(offset, len),
            ..Default::default()
        };
        Ok(self.op.read_options(key, opts)?.to_vec())
    }

    pub fn stat(&self, key: &str) -> Result<Option<ObjectMeta>, BlobErr> {
        match self.op.stat(key) {
            Ok(meta) => Ok(Some(ObjectMeta {
                key: key.to_string(),
                size: meta.content_length(),
                modified: meta
                    .last_modified()
                    .map(|stamp| stamp.into_inner().as_second()),
            })),
            Err(err) if err.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, BlobErr> {
        let opts = opendal::options::ListOptions { recursive: true, ..Default::default() };
        let mut objects = Vec::new();
        for entry in self.op.list_options(prefix, opts)? {
            let meta = entry.metadata();
            if meta.mode().is_dir() {
                continue;
            }
            objects.push(ObjectMeta {
                key: entry.path().to_string(),
                size: meta.content_length(),
                modified: meta
                    .last_modified()
                    .map(|stamp| stamp.into_inner().as_second()),
            });
        }
        Ok(objects)
    }

    /// Copy an object to a local file without loading it into memory.
    pub fn download(&self, key: &str, path: &std::path::Path) -> Result<u64, BlobErr> {
        let size = self
            .stat(key)?
            .ok_or_else(|| BlobErr::NotFound(key.to_string()))?
            .size;
        let reader = self.op.reader(key)?;
        let mut file = std::fs::File::create(path)?;
        let mut offset = 0u64;
        while offset < size {
            let len = COPY_CHUNK.min(size - offset);
            let buf = reader.read(offset..offset + len)?;
            if buf.is_empty() {
                break;
            }
            std::io::Write::write_all(&mut file, &buf.to_vec())?;
            offset += buf.len() as u64;
        }
        std::io::Write::flush(&mut file)?;
        if offset != size {
            return Err(BlobErr::SizeMismatch { expected: size, actual: offset });
        }
        Ok(size)
    }

    /// Stream a local file into an object key.
    pub fn put_file(&self, key: &str, path: &std::path::Path) -> Result<u64, BlobErr> {
        let mut file = std::fs::File::open(path)?;
        let mut writer = self.writer(key)?;
        let mut buf = vec![0u8; CHUNK];
        let mut size = 0u64;
        loop {
            let read = file.read(&mut buf)?;
            if read == 0 {
                break;
            }
            size += read as u64;
            writer.write(buf[..read].to_vec())?;
        }
        writer.close()?;
        Ok(size)
    }

    pub fn write_bytes(&self, key: &str, data: &[u8]) -> Result<(), BlobErr> {
        let mut writer = self.writer(key)?;
        writer.write(data.to_vec())?;
        writer.close()?;
        Ok(())
    }

    /// A short-lived URL a client can PUT to without workspace credentials.
    pub fn presign_write(&self, key: &str, expire: Duration) -> Result<PresignedWrite, BlobErr> {
        if !self.op.info().full_capability().presign_write {
            return Err(BlobErr::PresignUnsupported);
        }
        let request = self.op.presign_write(key, expire)?;
        let mut headers = Vec::new();
        for (name, value) in request.header() {
            let value = value
                .to_str()
                .map_err(|err| BlobErr::Presign(err.to_string()))?;
            headers.push((name.to_string(), value.to_string()));
        }
        Ok(PresignedWrite {
            url: request.uri().to_string(),
            method: request.method().to_string(),
            headers,
        })
    }

    pub fn delete(&self, key: &str) -> Result<(), BlobErr> {
        match self.op.delete(key) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == opendal::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Stream `from` into `to`, verifying the size and blake3 hash on the way.
    /// A mismatch removes the partial target and leaves `from` in place.
    pub fn copy_verified(
        &self,
        from: &str,
        to: &str,
        expected_size: Option<u64>,
        expected_hash: &str,
    ) -> Result<(u64, String), BlobErr> {
        let size = self
            .stat(from)?
            .ok_or_else(|| BlobErr::MissingUpload(from.to_string()))?
            .size;
        let reader = self.op.reader(from)?;
        let mut writer = self.op.writer(to)?;
        let mut blake = Blake3::new();
        let mut sha = Sha256::new();
        let mut offset = 0u64;
        while offset < size {
            let len = COPY_CHUNK.min(size - offset);
            let buf = reader.read(offset..offset + len)?;
            if buf.is_empty() {
                break;
            }
            let bytes = buf.to_vec();
            blake.update(&bytes);
            sha.update(&bytes);
            offset += bytes.len() as u64;
            writer.write(bytes)?;
        }
        writer.close()?;

        let hash = format!("blake3:{}", blake.finalize().to_hex());
        let failure = match expected_size {
            Some(expected) if expected != offset => {
                Some(BlobErr::SizeMismatch { expected, actual: offset })
            }
            _ if expected_hash != hash => Some(BlobErr::HashMismatch {
                expected: expected_hash.to_string(),
                actual: hash,
            }),
            _ => None,
        };
        if let Some(err) = failure {
            let _ = self.delete(to);
            return Err(err);
        }
        Ok((offset, hex(&sha.finalize())))
    }

    /// Server-side copy when the backend supports it, otherwise a bounded
    /// chunked copy. A failed copy leaves the staging object in place.
    pub fn copy_object(&self, from: &str, to: &str) -> Result<(), BlobErr> {
        if self.op.copy(from, to).is_ok() {
            return Ok(());
        }

        let reader = self.op.reader(from)?;
        let mut writer = self.op.writer(to)?;
        let mut offset = 0u64;
        loop {
            let buf = reader.read(offset..offset + COPY_CHUNK)?;
            if buf.is_empty() {
                break;
            }
            let len = buf.len() as u64;
            writer.write(buf.to_vec())?;
            offset += len;
            if len < COPY_CHUNK {
                break;
            }
        }
        writer.close()?;
        Ok(())
    }
}

/// A blob that was ingested, or found already present.
#[derive(Debug, Clone)]
pub struct IngestedBlob {
    pub blob: Blob,
    /// True when identical bytes were already stored, so no object was copied.
    pub deduplicated: bool,
}

/// A direct-upload ticket: either an existing blob or a presigned PUT.
#[derive(Debug, Clone)]
pub enum UploadTicket {
    Existing(IngestedBlob),
    Presigned {
        /// Staging key to pass back to commit.
        key: String,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        expires_in: u64,
    },
}

/// Blob operations for one workspace: ingest, range reads, staging cleanup.
pub struct Blobs<'a> {
    catalog: &'a Catalog,
    media: Option<&'a MediaStore>,
    archive: Option<&'a ArchiveStore>,
    workspace: &'a str,
}

impl<'a> Blobs<'a> {
    pub(crate) fn new(
        catalog: &'a Catalog,
        media: Option<&'a MediaStore>,
        archive: Option<&'a ArchiveStore>,
        workspace: &'a str,
    ) -> Self {
        Blobs { catalog, media, archive, workspace }
    }

    fn media(&self) -> Result<&MediaStore, BlobErr> {
        self.media.ok_or(BlobErr::NoMediaStore)
    }

    fn archive(&self) -> Result<&ArchiveStore, BlobErr> {
        self.archive.ok_or(BlobErr::NoArchiveStore)
    }

    /// Stream `reader` into the media store.
    ///
    /// When `expected_size` or `expected_hash` are given they are enforced
    /// against what was actually read. Hashing happens during the write, so
    /// commit is a copy plus a catalog row, never a re-read of the payload.
    pub fn ingest<R: Read>(
        &self,
        mut reader: R,
        expected_size: Option<u64>,
        expected_hash: Option<&str>,
        mime: Option<&str>,
    ) -> Result<IngestedBlob, BlobErr> {
        if let Some(hash) = expected_hash {
            validate_hash(hash)?;
            if let Some(blob) = self.catalog.blob_get(hash)? {
                return Ok(IngestedBlob { blob, deduplicated: true });
            }
        }

        let media = self.media()?;
        let staging = self.staging_key();
        let mut writer = media.writer(&staging)?;
        let mut blake = Blake3::new();
        let mut sha = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        let mut written = 0u64;

        loop {
            let read = reader.read(&mut buf)?;
            if read == 0 {
                break;
            }
            blake.update(&buf[..read]);
            sha.update(&buf[..read]);
            writer.write(buf[..read].to_vec())?;
            written += read as u64;
        }
        writer.close()?;

        if let Some(expected) = expected_size
            && expected != written
        {
            media.delete(&staging)?;
            return Err(BlobErr::SizeMismatch { expected, actual: written });
        }

        let hash = format!("blake3:{}", blake.finalize().to_hex());
        if let Some(expected) = expected_hash
            && expected != hash
        {
            media.delete(&staging)?;
            return Err(BlobErr::HashMismatch {
                expected: expected.to_string(),
                actual: hash,
            });
        }

        // Identical bytes may already be stored even when the caller did not
        // know the hash up front.
        if let Some(blob) = self.catalog.blob_get(&hash)? {
            media.delete(&staging)?;
            return Ok(IngestedBlob { blob, deduplicated: true });
        }

        let key = self.canonical_key(&hash);
        media.copy_object(&staging, &key)?;
        media.delete(&staging)?;

        let sha256 = hex(&sha.finalize());
        self.catalog.blob_upsert(&NewBlob {
            hash: hash.clone(),
            sha256: Some(sha256),
            size: written as i64,
            mime: mime.map(str::to_owned),
            storage: BlobStorage::Raw,
            locator: key,
        })?;
        self.catalog
            .metric_add(METRIC_INGESTED_BYTES, written as i64)?;
        self.catalog.metric_add(METRIC_INGESTED_COUNT, 1)?;

        let blob = self
            .catalog
            .blob_get(&hash)?
            .ok_or_else(|| BlobErr::NotFound(hash.clone()))?;
        Ok(IngestedBlob { blob, deduplicated: false })
    }

    /// Prepare a direct upload. The client PUTs the bytes to `url` with the
    /// returned method and headers, then calls [`Self::commit_presigned`] with
    /// the same hash and size.
    pub fn presign_ingest(
        &self,
        expected_hash: &str,
        expire: Duration,
    ) -> Result<UploadTicket, BlobErr> {
        validate_hash(expected_hash)?;
        if let Some(blob) = self.catalog.blob_get(expected_hash)? {
            return Ok(UploadTicket::Existing(IngestedBlob {
                blob,
                deduplicated: true,
            }));
        }
        let media = self.media()?;
        let key = self.staging_key();
        let request = media.presign_write(&key, expire)?;
        Ok(UploadTicket::Presigned {
            key,
            url: request.url,
            method: request.method,
            headers: request.headers,
            expires_in: expire.as_secs(),
        })
    }

    /// Verify a direct upload and move it into canonical storage.
    pub fn commit_presigned(
        &self,
        ticket: &str,
        expected_size: Option<u64>,
        expected_hash: &str,
        mime: Option<&str>,
    ) -> Result<IngestedBlob, BlobErr> {
        validate_hash(expected_hash)?;
        let prefix = format!("staging/{}/", self.workspace);
        if !ticket.starts_with(&prefix) || ticket.contains("..") {
            return Err(BlobErr::BadTicket(ticket.to_string()));
        }
        let media = self.media()?;

        if let Some(blob) = self.catalog.blob_get(expected_hash)? {
            media.delete(ticket)?;
            return Ok(IngestedBlob { blob, deduplicated: true });
        }

        let Some(meta) = media.stat(ticket)? else {
            return Err(BlobErr::MissingUpload(ticket.to_string()));
        };
        if let Some(expected) = expected_size
            && expected != meta.size
        {
            media.delete(ticket)?;
            return Err(BlobErr::SizeMismatch { expected, actual: meta.size });
        }

        let key = self.canonical_key(expected_hash);
        let (size, sha256) = media.copy_verified(
            ticket,
            &key,
            expected_size.or(Some(meta.size)),
            expected_hash,
        )?;
        media.delete(ticket)?;

        if let Err(err) = self.catalog.blob_upsert(&NewBlob {
            hash: expected_hash.to_string(),
            sha256: Some(sha256),
            size: size as i64,
            mime: mime.map(str::to_owned),
            storage: BlobStorage::Raw,
            locator: key.clone(),
        }) {
            let _ = media.delete(&key);
            return Err(err.into());
        }
        self.catalog
            .metric_add(METRIC_INGESTED_BYTES, size as i64)?;
        self.catalog.metric_add(METRIC_INGESTED_COUNT, 1)?;

        let blob = self
            .catalog
            .blob_get(expected_hash)?
            .ok_or_else(|| BlobErr::NotFound(expected_hash.to_string()))?;
        Ok(IngestedBlob { blob, deduplicated: false })
    }

    /// Stream `reader` into the encrypted archive volume.
    ///
    /// Chunks are deduplicated by `cdc-volume`; the catalog row is keyed by the
    /// blake3 of the whole file, so a repeat upload is a catalog hit before any
    /// bytes are read. A repeat discovered only after writing deletes the new
    /// manifest and reports deduplication.
    pub fn ingest_cdc<R: Read>(
        &self,
        reader: R,
        expected_size: Option<u64>,
        expected_hash: Option<&str>,
        mime: Option<&str>,
    ) -> Result<IngestedBlob, BlobErr> {
        if let Some(hash) = expected_hash {
            validate_hash(hash)?;
            if let Some(blob) = self.catalog.blob_get(hash)? {
                return Ok(IngestedBlob { blob, deduplicated: true });
            }
        }

        let archive = self.archive()?;
        let path = format!("media/cdc/{}/{}", self.workspace, ticket_id());
        let mut hashing = HashingReader::new(reader);
        let stat = archive.put(&path, &mut hashing)?;
        let written = stat.size;

        if let Some(expected) = expected_size
            && expected != written
        {
            archive.delete(&path)?;
            return Err(BlobErr::SizeMismatch { expected, actual: written });
        }

        let (hash, sha256) = hashing.digests();
        if let Some(expected) = expected_hash
            && expected != hash
        {
            archive.delete(&path)?;
            return Err(BlobErr::HashMismatch {
                expected: expected.to_string(),
                actual: hash,
            });
        }

        if let Some(blob) = self.catalog.blob_get(&hash)? {
            archive.delete(&path)?;
            return Ok(IngestedBlob { blob, deduplicated: true });
        }

        if let Err(err) = self.catalog.blob_upsert(&NewBlob {
            hash: hash.clone(),
            sha256: Some(sha256),
            size: written as i64,
            mime: mime.map(str::to_owned),
            storage: BlobStorage::Cdc,
            locator: path.clone(),
        }) {
            let _ = archive.delete(&path);
            return Err(err.into());
        }
        self.catalog
            .metric_add(METRIC_INGESTED_BYTES, written as i64)?;
        self.catalog.metric_add(METRIC_INGESTED_COUNT, 1)?;

        let blob = self
            .catalog
            .blob_get(&hash)?
            .ok_or_else(|| BlobErr::NotFound(hash.clone()))?;
        Ok(IngestedBlob { blob, deduplicated: false })
    }

    /// Read `len` bytes at `offset` from a blob. `None` reads to the end.
    /// Raw blobs are range-read from object storage; `cdc` blobs are read
    /// through the archive volume's chunk index.
    pub fn read_range(
        &self,
        hash: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<Vec<u8>, BlobErr> {
        validate_hash(hash)?;
        let blob = self
            .catalog
            .blob_get(hash)?
            .ok_or_else(|| BlobErr::NotFound(hash.to_string()))?;
        let size = u64::try_from(blob.size).unwrap_or(0);
        if offset > size {
            return Err(BlobErr::RangeOutOfBounds {
                hash: hash.to_string(),
                offset,
                len: len.unwrap_or(0),
                size,
            });
        }
        let remaining = size - offset;
        let len = len.map_or(remaining, |len| len.min(remaining));
        if len == 0 {
            return Ok(Vec::new());
        }
        match blob.storage {
            BlobStorage::Raw => self.media()?.read_range(&blob.locator, offset, Some(len)),
            BlobStorage::Cdc => self.archive()?.read_range(&blob.locator, offset, len),
        }
    }

    /// Pin a blob so [`Self::sweep_unreferenced`] keeps it.
    pub fn pin(&self, hash: &str) -> Result<(), BlobErr> {
        validate_hash(hash)?;
        self.catalog.blob_pin(hash)?;
        Ok(())
    }

    /// Remove staging objects older than `older_than`. Interrupted uploads
    /// leave staging garbage; canonical objects are removed by
    /// [`Self::sweep_unreferenced`] instead.
    pub fn sweep_staging(&self, older_than: Duration) -> Result<Vec<String>, BlobErr> {
        let Some(media) = self.media else {
            return Ok(Vec::new());
        };
        let now = now_unix();
        let cutoff = now - older_than.as_secs() as i64;
        let prefix = format!("staging/{}/", self.workspace);

        let mut removed = Vec::new();
        let mut bytes = 0i64;
        for object in media.list(&prefix)? {
            // List metadata is not guaranteed to carry an mtime; stat is.
            // Backends that still do not report one keep their staging
            // objects: deleting a live upload is worse than keeping garbage.
            let modified = media.stat(&object.key)?.and_then(|meta| meta.modified);
            if modified.is_none_or(|modified| modified > cutoff) {
                continue;
            }
            media.delete(&object.key)?;
            bytes += object.size as i64;
            removed.push(object.key);
        }
        if !removed.is_empty() {
            self.catalog.metric_add(METRIC_SWEPT_BYTES, bytes)?;
            self.catalog
                .metric_add(METRIC_SWEPT_COUNT, removed.len() as i64)?;
        }
        Ok(removed)
    }

    /// Delete raw canonical objects that no entry references and that no
    /// release pins, then forget their catalog rows. Returns the hashes.
    ///
    /// The row is removed before the object: a crash between the two steps
    /// leaves an orphan object, never a catalog row whose bytes are gone.
    pub fn sweep_unreferenced(&self) -> Result<Vec<String>, BlobErr> {
        let media = self.media()?;
        let mut removed = Vec::new();
        let mut bytes = 0i64;
        for blob in self.catalog.unreferenced_blobs()? {
            self.catalog.blob_delete(&blob.hash)?;
            media.delete(&blob.locator)?;
            bytes += blob.size;
            removed.push(blob.hash);
        }
        if !removed.is_empty() {
            self.catalog.metric_add(METRIC_SWEPT_BYTES, bytes)?;
            self.catalog
                .metric_add(METRIC_SWEPT_COUNT, removed.len() as i64)?;
        }
        Ok(removed)
    }

    /// Delete archived objects that no catalog row names, then forget catalog
    /// rows that no entry references and that no release pins. Returns the
    /// hashes of the forgotten rows.
    ///
    /// Rows go before objects. A crash in between leaves a manifest no row
    /// names, which the next sweep removes by walking the volume; the reverse
    /// order could leave a live row pointing at missing bytes. The freed pages
    /// are only reclaimed by [`ArchiveStore::gc`].
    pub fn sweep_unreferenced_cdc(&self) -> Result<Vec<String>, BlobErr> {
        let archive = self.archive.as_ref().ok_or(BlobErr::NoArchiveStore)?;
        let mut removed = Vec::new();
        let mut bytes = 0i64;
        for blob in self.catalog.unreferenced_cdc_blobs()? {
            self.catalog.blob_delete(&blob.hash)?;
            archive.delete(&blob.locator)?;
            bytes += blob.size;
            removed.push(blob.hash);
        }

        let live: HashSet<String> = self
            .catalog
            .blob_list()?
            .into_iter()
            .filter(|blob| blob.storage == BlobStorage::Cdc)
            .map(|blob| blob.locator)
            .collect();
        let prefix = format!("media/cdc/{}/", self.workspace);
        for path in archive.list(&prefix)? {
            if !live.contains(&path) {
                archive.delete(&path)?;
            }
        }
        if !removed.is_empty() {
            self.catalog.metric_add(METRIC_SWEPT_BYTES, bytes)?;
            self.catalog
                .metric_add(METRIC_SWEPT_COUNT, removed.len() as i64)?;
        }
        Ok(removed)
    }

    fn staging_key(&self) -> String {
        format!("staging/{}/{}", self.workspace, ticket_id())
    }

    fn canonical_key(&self, hash: &str) -> String {
        let digest = hash.strip_prefix("blake3:").unwrap_or(hash);
        format!("media/raw/{}/{digest}", self.workspace)
    }
}

/// Reads through a payload once, hashing it on the way to the archive.
struct HashingReader<R> {
    inner: R,
    blake: Blake3,
    sha: Sha256,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        HashingReader {
            inner,
            blake: Blake3::new(),
            sha: Sha256::new(),
        }
    }

    fn digests(self) -> (String, String) {
        (
            format!("blake3:{}", self.blake.finalize().to_hex()),
            hex(&self.sha.finalize()),
        )
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.blake.update(&buf[..read]);
        self.sha.update(&buf[..read]);
        Ok(read)
    }
}

fn validate_hash(hash: &str) -> Result<(), BlobErr> {
    let Some(hex) = hash.strip_prefix("blake3:") else {
        return Err(BlobErr::InvalidHash(hash.to_string()));
    };
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(BlobErr::InvalidHash(hash.to_string()));
    }
    Ok(())
}

fn ticket_id() -> String {
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
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
