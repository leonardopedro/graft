//! Binary update repository and client-side `FastCDC` deltas.
//!
//! A release is a content-addressed manifest. Files are split with the existing
//! `FastCDC` chunker (`cdc_volume::chunker`), and each chunk is stored once under
//! its blake3 hash. Clients keep an installed-state manifest; an update reuses
//! every chunk that can be read and verified from the currently installed files
//! and downloads only the rest.
//!
//! Repository layout under a prefix:
//!
//! ```text
//! <prefix>/<name>/refs/<release>            blake3:<manifest hash>
//! <prefix>/<name>/manifests/<hash>.json
//! <prefix>/<name>/chunks/<2>/<hash>         raw FastCDC chunk
//! <prefix>/<name>/chunks/<2>/<hash>.zst     zstd-compressed chunk
//! ```
//!
//! The ref pins the manifest hash. Manifests, files and chunks are all verified
//! before their bytes are used. Refs may carry Ed25519 signatures; clients can
//! require one with a public key file.
//!
//! Object stores may be local, S3-compatible, HTTP/2, or HTTP/3 over QUIC.
//! HTTP repositories are read-only: they support `plan`, `apply` and `deploy`,
//! but not publish, listing or garbage collection.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader, Cursor, Read, Seek, SeekFrom, Write},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use blake3::Hasher as Blake3;
use cdc_volume::{
    chunker::{MAX_CHUNK, chunk_stream},
    err::CdcErr,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    code,
    config::MediaConfig,
    media::{BlobErr, MediaStore, ObjectMeta},
};

/// Refuse manifests larger than this; they are small indexes, and a corrupt
/// size must not allocate without bound.
const MAX_MANIFEST: u64 = 64 * 1024 * 1024;
const MAX_REF: u64 = 64 * 1024;
const MANIFEST_SCHEMA: u32 = 1;
/// Default zstd level used by `publish --compress`.
pub const DEFAULT_COMPRESSION_LEVEL: i32 = 3;
const MAX_COMPRESSION_LEVEL: i32 = 22;

#[derive(Debug, Error)]
pub enum UpdateErr {
    #[error("object storage: {0}")]
    Blob(#[from] BlobErr),

    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("manifest JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("chunking: {0}")]
    Cdc(#[from] CdcErr),

    #[error("tree: {0}")]
    Code(#[from] crate::code::CodeErr),

    #[error("invalid update repository name {0:?}")]
    InvalidName(String),

    #[error("invalid release {0:?}: use one path segment of letters, digits, '.', '_' or '-'")]
    InvalidRelease(String),

    #[error("invalid repository prefix {0:?}")]
    InvalidPrefix(String),

    #[error("invalid update repository URL: {0}")]
    InvalidUrl(String),

    #[error("HTTP update repository: {0}")]
    Http(String),

    #[error("HTTP update repositories are read-only; {0} requires fs: or s3:")]
    ReadOnly(&'static str),

    #[error("HTTP update repositories cannot list objects; {0} requires fs: or s3:")]
    NoListing(&'static str),

    #[error("invalid release prune: {0}")]
    InvalidPrune(String),

    #[error("path {0:?} is not a safe relative path")]
    UnsafePath(String),

    #[error("file {path:?} has unsupported mode {mode:#o}; only permission bits are allowed")]
    InvalidFileMode { path: String, mode: u32 },

    #[error("invalid blake3 hash {0:?}")]
    InvalidHash(String),

    #[error("manifest {0:?} is larger than {MAX_MANIFEST} bytes")]
    ManifestTooLarge(String),

    #[error("signing key {path}: {reason}")]
    InvalidKey { path: String, reason: String },

    #[error("release {name}/{release} is not signed and a public key was required")]
    UnsignedRelease { name: String, release: String },

    #[error("release {name}/{release} has no valid signature from the required key")]
    BadSignature { name: String, release: String },

    #[error("invalid update ref: {0}")]
    InvalidRef(String),

    #[error("release {name}/{release}: ref names {expected}, manifest hashes to {actual}")]
    ManifestMismatch {
        name: String,
        release: String,
        expected: String,
        actual: String,
    },

    #[error("unsupported update manifest schema {0}")]
    UnsupportedSchema(u32),

    #[error(
        "release {name}/{release} already exists as {existing}; refusing to replace it with {proposed}"
    )]
    ReleaseExists {
        name: String,
        release: String,
        existing: String,
        proposed: String,
    },

    #[error("manifest describes {name}/{release}, expected {expected_name}/{expected_release}")]
    WrongRelease {
        name: String,
        release: String,
        expected_name: String,
        expected_release: String,
    },

    #[error(
        "file {path:?} has ragged chunk sizes: {chunks} chunks total {actual} bytes, file is {size}"
    )]
    RaggedChunks {
        path: String,
        chunks: usize,
        actual: u64,
        size: u64,
    },

    #[error("chunk {hash:?} is {actual} bytes, manifest says {expected}")]
    ChunkSize {
        hash: String,
        expected: u32,
        actual: u64,
    },

    #[error("chunk {hash:?} in the repository does not match its hash")]
    CorruptChunk { hash: String },

    #[error("chunk {hash:?} is missing from the repository")]
    MissingChunk { hash: String },

    #[error("chunk {hash:?} could not be decoded from {encoding} compression: {reason}")]
    CorruptEncoding {
        hash: String,
        encoding: String,
        reason: String,
    },

    #[error("zstd compression level {0} is outside 1..={MAX_COMPRESSION_LEVEL}")]
    InvalidCompressionLevel(i32),

    #[error("file {path:?} changed while it was being published")]
    FileChanged { path: String },

    #[error("file {path:?} assembled to {actual}, manifest says {expected}")]
    FileMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("the installed state has no chunks for {0:?}")]
    EmptyInstalledChunk(String),
}

/// Optional storage encoding for a chunk. Chunk hashes always cover the
/// plaintext so a compressed and raw copy of the same bytes are one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChunkEncoding {
    Zstd,
}

impl ChunkEncoding {
    fn suffix(self) -> &'static str {
        match self {
            ChunkEncoding::Zstd => ".zst",
        }
    }

    fn name(self) -> &'static str {
        match self {
            ChunkEncoding::Zstd => "zstd",
        }
    }
}

/// One chunk in a release manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateChunk {
    pub hash: String,
    pub size: u32,
    /// Omitted for a raw chunk, including every chunk published before
    /// compression was introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<ChunkEncoding>,
}

/// One file in a release manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateFile {
    pub path: String,
    pub size: u64,
    pub mode: u32,
    pub hash: String,
    pub chunks: Vec<UpdateChunk>,
}

/// An immutable release description, or an installed-state snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub schema: u32,
    pub name: String,
    pub release: String,
    pub files: Vec<UpdateFile>,
}

impl ReleaseManifest {
    fn validate(
        &self,
        expected_name: &str,
        expected_release: Option<&str>,
    ) -> Result<(), UpdateErr> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(UpdateErr::UnsupportedSchema(self.schema));
        }
        validate_name(&self.name)?;
        validate_release(&self.release)?;
        if self.name != expected_name {
            return Err(UpdateErr::WrongRelease {
                name: self.name.clone(),
                release: self.release.clone(),
                expected_name: expected_name.to_string(),
                expected_release: expected_release.unwrap_or("*").to_string(),
            });
        }
        if let Some(release) = expected_release
            && self.release != release
        {
            return Err(UpdateErr::WrongRelease {
                name: self.name.clone(),
                release: self.release.clone(),
                expected_name: expected_name.to_string(),
                expected_release: release.to_string(),
            });
        }

        let mut paths = HashSet::new();
        for file in &self.files {
            safe_relative(&file.path)?;
            validate_hash(&file.hash)?;
            if file.mode & !0o777 != 0 {
                return Err(UpdateErr::InvalidFileMode {
                    path: file.path.clone(),
                    mode: file.mode,
                });
            }
            if !paths.insert(file.path.as_str()) {
                return Err(UpdateErr::UnsafePath(format!(
                    "duplicate manifest path {:?}",
                    file.path
                )));
            }
            let mut total = 0u64;
            for chunk in &file.chunks {
                validate_hash(&chunk.hash)?;
                if chunk.size > MAX_CHUNK {
                    return Err(UpdateErr::ChunkSize {
                        hash: chunk.hash.clone(),
                        expected: chunk.size,
                        actual: MAX_CHUNK as u64,
                    });
                }
                total = total.saturating_add(u64::from(chunk.size));
            }
            if total != file.size {
                return Err(UpdateErr::RaggedChunks {
                    path: file.path.clone(),
                    chunks: file.chunks.len(),
                    actual: total,
                    size: file.size,
                });
            }
        }
        Ok(())
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    pub fn total_chunks(&self) -> usize {
        self.files.iter().map(|file| file.chunks.len()).sum()
    }
}

/// An Ed25519 private key, loaded from 64 hex characters.
pub struct ManifestSigningKey {
    key: SigningKey,
}

impl ManifestSigningKey {
    pub fn load(path: &Path) -> Result<ManifestSigningKey, UpdateErr> {
        let seed = read_key_bytes(path)?;
        Ok(ManifestSigningKey { key: SigningKey::from_bytes(&seed) })
    }

    pub fn public_key(&self) -> ManifestPublicKey {
        ManifestPublicKey { key: self.key.verifying_key() }
    }

    fn signature(&self, manifest: &[u8]) -> RefSignature {
        let signature = self.key.sign(&signing_message(manifest));
        RefSignature {
            algorithm: "ed25519".to_string(),
            public_key: self.public_key().to_string(),
            signature: hex(&signature.to_bytes()),
        }
    }
}

/// An Ed25519 public key that a client requires for a release.
#[derive(Clone, Debug)]
pub struct ManifestPublicKey {
    key: VerifyingKey,
}

impl ManifestPublicKey {
    pub fn load(path: &Path) -> Result<ManifestPublicKey, UpdateErr> {
        let bytes = read_key_bytes(path)?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|err| UpdateErr::InvalidKey {
            path: path.display().to_string(),
            reason: err.to_string(),
        })?;
        Ok(ManifestPublicKey { key })
    }

    fn verify(&self, manifest: &[u8], signature: &RefSignature) -> bool {
        if signature.public_key != self.to_string() {
            return false;
        }
        let Ok(bytes) = decode_hex::<64>(&signature.signature) else {
            return false;
        };
        let signature = Signature::from_bytes(&bytes);
        self.key
            .verify(&signing_message(manifest), &signature)
            .is_ok()
    }
}

impl std::fmt::Display for ManifestPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ed25519:{}", hex(self.key.as_bytes()))
    }
}

/// Generate a key pair and write the private and public key files. Existing
/// files are refused rather than overwritten.
#[cfg(unix)]
pub fn generate_keypair(private: &Path, public: &Path) -> Result<(), UpdateErr> {
    use std::os::unix::fs::OpenOptionsExt;
    generate_keypair_with(private, public, |path, bytes| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })
}

/// Generate a key pair and write the private and public key files. Existing
/// files are refused rather than overwritten.
#[cfg(not(unix))]
pub fn generate_keypair(private: &Path, public: &Path) -> Result<(), UpdateErr> {
    generate_keypair_with(private, public, |path, bytes| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })
}

fn generate_keypair_with(
    private: &Path,
    public: &Path,
    write: impl Fn(&Path, &[u8]) -> io::Result<()>,
) -> Result<(), UpdateErr> {
    let mut seed = [0u8; 32];
    for chunk in seed.chunks_mut(8) {
        chunk.copy_from_slice(&rand::random::<u64>().to_le_bytes());
    }
    let key = SigningKey::from_bytes(&seed);
    let private_text = format!("{}\n", hex(&seed));
    let public_text = format!("{}\n", hex(key.verifying_key().as_bytes()));
    write(private, private_text.as_bytes())?;
    if let Err(err) = write(public, public_text.as_bytes()) {
        let _ = fs::remove_file(private);
        return Err(err.into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RefSignature {
    algorithm: String,
    public_key: String,
    signature: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RefEnvelope {
    schema: u32,
    manifest: String,
    created_at: i64,
    #[serde(default)]
    signatures: Vec<RefSignature>,
}

struct LoadedRef {
    manifest: String,
    created_at: Option<i64>,
    signatures: Vec<RefSignature>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishReport {
    pub name: String,
    pub release: String,
    pub manifest: String,
    pub files: usize,
    pub chunks: usize,
    pub bytes: u64,
    pub uploaded_chunks: usize,
    pub uploaded_bytes: u64,
    pub reused_chunks: usize,
    pub compressed_chunks: usize,
    pub compression_level: Option<i32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdatePlan {
    pub files_total: usize,
    pub files_new: usize,
    pub files_changed: usize,
    pub files_unchanged: usize,
    pub files_removed: usize,
    pub chunks_total: usize,
    pub chunks_missing: usize,
    pub bytes_total: u64,
    pub bytes_missing: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub release: String,
    pub manifest: String,
    pub signatures: usize,
    pub created_at: Option<i64>,
    /// Object-store modification time, used for unsigned refs.
    pub modified: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefPruneReport {
    pub kept: Vec<String>,
    pub removed: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub manifests_removed: usize,
    pub chunks_removed: usize,
    pub bytes_removed: u64,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub name: String,
    pub release: String,
    pub manifest: String,
    pub files_written: usize,
    pub files_unchanged: usize,
    pub files_deleted: usize,
    pub chunks_downloaded: usize,
    pub bytes_downloaded: u64,
    pub chunks_reused: usize,
    pub bytes_reused: u64,
    /// Chunks recovered from a staging tree left by an interrupted apply.
    pub chunks_resumed: usize,
    pub bytes_resumed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HttpProtocol {
    /// HTTPS negotiates HTTP/2 through ALPN; cleartext uses HTTP/1.1 unless the
    /// server upgrades. `h2c:` forces cleartext HTTP/2 prior knowledge.
    Auto,
    H2c,
    /// HTTP/3 over QUIC using the `h3` ALPN.
    H3,
}

struct HttpStore {
    base: url::Url,
    protocol: HttpProtocol,
    client: Option<reqwest::blocking::Client>,
    runtime: Option<tokio::runtime::Runtime>,
    roots: Option<Arc<rustls::RootCertStore>>,
}

struct HttpResponse {
    status: u16,
    content_length: Option<u64>,
    content_range_total: Option<u64>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn total_length(&self) -> Option<u64> {
        self.content_range_total.or(self.content_length)
    }
}

impl HttpStore {
    /// Parse `http:`, `https:`, `h2c:`, `http2:`, `quic:` or `h3:` locators.
    /// Returns `None` for non-HTTP schemes. `ca_file` adds a PEM certificate
    /// authority to the Mozilla roots for HTTPS and QUIC.
    fn parse(spec: &str, ca_file: Option<&Path>) -> Result<Option<HttpStore>, UpdateErr> {
        let (scheme, protocol) = if let Some(rest) = spec.strip_prefix("https+quic://") {
            return Self::parse_rest("https", HttpProtocol::H3, rest, ca_file).map(Some);
        } else if let Some(rest) = spec.strip_prefix("quic://") {
            return Self::parse_rest("https", HttpProtocol::H3, rest, ca_file).map(Some);
        } else if let Some(rest) = spec.strip_prefix("h3://") {
            return Self::parse_rest("https", HttpProtocol::H3, rest, ca_file).map(Some);
        } else if let Some(rest) = spec.strip_prefix("h2c://") {
            return Self::parse_rest("http", HttpProtocol::H2c, rest, ca_file).map(Some);
        } else if let Some(rest) = spec.strip_prefix("http2://") {
            return Self::parse_rest("http", HttpProtocol::H2c, rest, ca_file).map(Some);
        } else if spec.starts_with("http://") {
            ("http", HttpProtocol::Auto)
        } else if spec.starts_with("https://") {
            ("https", HttpProtocol::Auto)
        } else {
            return Ok(None);
        };

        Self::parse_rest(scheme, protocol, &spec[scheme.len() + 3..], ca_file).map(Some)
    }

    fn parse_rest(
        scheme: &str,
        protocol: HttpProtocol,
        rest: &str,
        ca_file: Option<&Path>,
    ) -> Result<HttpStore, UpdateErr> {
        if rest.is_empty() {
            return Err(UpdateErr::InvalidUrl(format!("{scheme}: needs a host")));
        }
        let base = url::Url::parse(&format!("{scheme}://{}/", rest.trim_end_matches('/')))
            .map_err(|err| UpdateErr::InvalidUrl(err.to_string()))?;
        if base.query().is_some() || base.fragment().is_some() {
            return Err(UpdateErr::InvalidUrl(
                "repository URLs cannot have a query or fragment".to_string(),
            ));
        }

        let client = match protocol {
            HttpProtocol::Auto | HttpProtocol::H2c => {
                let mut builder = reqwest::blocking::Client::builder()
                    .user_agent("graft-workspace-update/1")
                    .timeout(std::time::Duration::from_secs(120));
                if protocol == HttpProtocol::H2c {
                    builder = builder.http2_prior_knowledge();
                }
                if let Some(path) = ca_file {
                    let pem = fs::read(path)?;
                    builder = builder.add_root_certificate(
                        reqwest::Certificate::from_pem(&pem)
                            .map_err(|err| UpdateErr::Http(err.to_string()))?,
                    );
                }
                Some(
                    builder
                        .build()
                        .map_err(|err| UpdateErr::Http(err.to_string()))?,
                )
            }
            HttpProtocol::H3 => None,
        };
        let roots = if protocol == HttpProtocol::H3 {
            Some(Arc::new(load_roots(ca_file)?))
        } else {
            None
        };
        let runtime = if protocol == HttpProtocol::H3 {
            Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()?,
            )
        } else {
            None
        };
        Ok(HttpStore { base, protocol, client, runtime, roots })
    }

    fn url_for(&self, key: &str) -> Result<url::Url, UpdateErr> {
        self.base
            .join(key)
            .map_err(|err| UpdateErr::InvalidUrl(err.to_string()))
    }

    fn request(
        &self,
        method: http::Method,
        key: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<Option<HttpResponse>, UpdateErr> {
        if self.protocol == HttpProtocol::H3 {
            return self.h3_request(method, key, range);
        }
        let url = self.url_for(key)?;
        let client = self.client.as_ref().expect("HTTP/2 client");
        let mut request = client
            .request(method, url.clone())
            .header("accept-encoding", "identity");
        if let Some((offset, len)) = range {
            request = request.header("range", range_header(offset, len));
        }
        let response = request
            .send()
            .map_err(|err| UpdateErr::Http(format!("{url}: {err}")))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status().as_u16();
        let content_length = header_u64(response.headers(), "content-length");
        let content_range_total = response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .and_then(content_range_total);
        let body = response
            .bytes()
            .map_err(|err| UpdateErr::Http(format!("{url}: {err}")))?
            .to_vec();
        Ok(Some(HttpResponse {
            status,
            content_length,
            content_range_total,
            body,
        }))
    }

    fn h3_request(
        &self,
        method: http::Method,
        key: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<Option<HttpResponse>, UpdateErr> {
        let url = self.url_for(key)?;
        let runtime = self.runtime.as_ref().expect("QUIC runtime");
        let roots = self.roots.as_ref().expect("QUIC roots");
        runtime.block_on(h3_fetch(url, method, range, roots.clone()))
    }

    fn stat(&self, key: &str) -> Result<Option<ObjectMeta>, UpdateErr> {
        let mut response = match self.request(http::Method::HEAD, key, None)? {
            Some(response) => response,
            None => return Ok(None),
        };
        if response.status == 405 || response.status == 501 {
            response = match self.request(http::Method::GET, key, Some((0, Some(1))))? {
                Some(response) => response,
                None => return Ok(None),
            };
        }
        if !response.is_success() {
            return Err(UpdateErr::Http(format!(
                "HEAD {key}: HTTP {}",
                response.status
            )));
        }
        let size = response
            .total_length()
            .ok_or_else(|| UpdateErr::Http(format!("HEAD {key}: no content length")))?;
        Ok(Some(ObjectMeta {
            key: key.to_string(),
            size,
            modified: None,
        }))
    }

    fn read_range(&self, key: &str, offset: u64, len: Option<u64>) -> Result<Vec<u8>, UpdateErr> {
        let Some(mut response) = self.request(http::Method::GET, key, Some((offset, len)))? else {
            return Err(UpdateErr::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{key} is missing"),
            )));
        };
        if !response.is_success() {
            return Err(UpdateErr::Http(format!(
                "GET {key}: HTTP {}",
                response.status
            )));
        }
        if response.status == 200 && offset > 0 {
            let offset = usize::try_from(offset)
                .map_err(|_| UpdateErr::Http("range offset is too large".to_string()))?;
            if response.body.len() < offset {
                response.body.clear();
            } else {
                response.body.drain(..offset);
            }
        }
        if let Some(len) = len {
            let len = usize::try_from(len)
                .map_err(|_| UpdateErr::Http("range length is too large".to_string()))?;
            if response.body.len() > len {
                response.body.truncate(len);
            }
            if response.body.len() != len {
                return Err(UpdateErr::Http(format!(
                    "GET {key}: expected {len} bytes, got {}",
                    response.body.len()
                )));
            }
        }
        Ok(response.body)
    }
}

fn load_roots(ca_file: Option<&Path>) -> Result<rustls::RootCertStore, UpdateErr> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let Some(path) = ca_file else {
        return Ok(roots);
    };
    let pem = fs::read(path)?;
    let mut reader = BufReader::new(Cursor::new(pem));
    let mut added = 0usize;
    for cert in rustls_pemfile::certs(&mut reader) {
        let cert = cert.map_err(|err| UpdateErr::Http(format!("{}: {err}", path.display())))?;
        roots
            .add(cert)
            .map_err(|err| UpdateErr::Http(format!("{}: {err}", path.display())))?;
        added += 1;
    }
    if added == 0 {
        return Err(UpdateErr::Http(format!(
            "{}: no PEM certificates found",
            path.display()
        )));
    }
    Ok(roots)
}

fn range_header(offset: u64, len: Option<u64>) -> String {
    match len {
        Some(0) => format!("bytes={offset}-{offset}"),
        Some(len) => format!("bytes={offset}-{}", offset.saturating_add(len - 1)),
        None => format!("bytes={offset}-"),
    }
}

fn header_u64(headers: &http::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit_once('/')?.1.trim().parse().ok()
}

async fn h3_fetch(
    url: url::Url,
    method: http::Method,
    range: Option<(u64, Option<u64>)>,
    roots: Arc<rustls::RootCertStore>,
) -> Result<Option<HttpResponse>, UpdateErr> {
    use bytes::Buf;
    use futures::future::poll_fn;

    let host = url
        .host_str()
        .ok_or_else(|| UpdateErr::InvalidUrl("URL has no host".to_string()))?
        .to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let addr = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|err| UpdateErr::Http(format!("resolve {host}: {err}")))?
        .next()
        .ok_or_else(|| UpdateErr::Http(format!("resolve {host}: no addresses")))?;

    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates((*roots).clone())
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let quic = h3_quinn::quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|err| UpdateErr::Http(format!("QUIC TLS: {err}")))?;
    let bind = if addr.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let mut endpoint = h3_quinn::quinn::Endpoint::client(bind)
        .map_err(|err| UpdateErr::Http(format!("QUIC endpoint: {err}")))?;
    endpoint.set_default_client_config(h3_quinn::quinn::ClientConfig::new(std::sync::Arc::new(
        quic,
    )));

    let connection = endpoint
        .connect(addr, &host)
        .map_err(|err| UpdateErr::Http(format!("QUIC connect: {err}")))?
        .await
        .map_err(|err| UpdateErr::Http(format!("QUIC handshake: {err}")))?;
    let close = connection.clone();
    let (mut driver, mut send_request) = h3::client::new(h3_quinn::Connection::new(connection))
        .await
        .map_err(|err| UpdateErr::Http(format!("HTTP/3: {err}")))?;
    let driver_task = tokio::spawn(async move {
        let _ = poll_fn(|cx| driver.poll_close(cx)).await;
    });

    let uri: http::Uri = url
        .as_str()
        .parse()
        .map_err(|err| UpdateErr::InvalidUrl(format!("{url}: {err}")))?;
    let mut builder = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("accept-encoding", "identity");
    if let Some((offset, len)) = range {
        builder = builder.header("range", range_header(offset, len));
    }
    let request = builder
        .body(())
        .map_err(|err| UpdateErr::Http(format!("HTTP/3 request: {err}")))?;

    let result = async {
        let mut stream = send_request
            .send_request(request)
            .await
            .map_err(|err| UpdateErr::Http(format!("HTTP/3 send: {err}")))?;
        stream
            .finish()
            .await
            .map_err(|err| UpdateErr::Http(format!("HTTP/3 send: {err}")))?;
        let response = stream
            .recv_response()
            .await
            .map_err(|err| UpdateErr::Http(format!("HTTP/3 response: {err}")))?;
        let status = response.status().as_u16();
        if status == 404 {
            return Ok::<Option<HttpResponse>, UpdateErr>(None);
        }
        let content_length = header_u64(response.headers(), "content-length");
        let content_range_total = response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .and_then(content_range_total);
        let mut body = Vec::new();
        while let Some(mut chunk) = stream
            .recv_data()
            .await
            .map_err(|err| UpdateErr::Http(format!("HTTP/3 body: {err}")))?
        {
            let bytes = chunk.copy_to_bytes(chunk.remaining());
            body.extend_from_slice(&bytes);
        }
        Ok(Some(HttpResponse {
            status,
            content_length,
            content_range_total,
            body,
        }))
    }
    .await;

    close.close(0u32.into(), b"done");
    driver_task.abort();
    result
}

/// A release repository in one object store or read-only HTTP endpoint.
pub struct UpdateRepo {
    store: UpdateStore,
    prefix: String,
    name: String,
    store_label: String,
}

enum UpdateStore {
    Object(MediaStore),
    Http(HttpStore),
}

impl UpdateStore {
    fn stat(&self, key: &str) -> Result<Option<ObjectMeta>, UpdateErr> {
        match self {
            UpdateStore::Object(store) => Ok(store.stat(key)?),
            UpdateStore::Http(store) => store.stat(key),
        }
    }

    fn read_range(&self, key: &str, offset: u64, len: Option<u64>) -> Result<Vec<u8>, UpdateErr> {
        match self {
            UpdateStore::Object(store) => Ok(store.read_range(key, offset, len)?),
            UpdateStore::Http(store) => store.read_range(key, offset, len),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, UpdateErr> {
        match self {
            UpdateStore::Object(store) => Ok(store.list(prefix)?),
            UpdateStore::Http(_) => Err(UpdateErr::NoListing("listing releases")),
        }
    }

    fn write_bytes(&self, key: &str, data: &[u8]) -> Result<(), UpdateErr> {
        match self {
            UpdateStore::Object(store) => Ok(store.write_bytes(key, data)?),
            UpdateStore::Http(_) => Err(UpdateErr::ReadOnly("publishing")),
        }
    }

    fn delete(&self, key: &str) -> Result<(), UpdateErr> {
        match self {
            UpdateStore::Object(store) => Ok(store.delete(key)?),
            UpdateStore::Http(_) => Err(UpdateErr::ReadOnly("garbage collection")),
        }
    }

    fn ensure_writable(&self, action: &'static str) -> Result<(), UpdateErr> {
        match self {
            UpdateStore::Object(_) => Ok(()),
            UpdateStore::Http(_) => Err(UpdateErr::ReadOnly(action)),
        }
    }
}

impl UpdateRepo {
    /// Open a repo named `name` in `store`, under `prefix`.
    pub fn open(store: MediaStore, prefix: &str, name: &str) -> Result<UpdateRepo, UpdateErr> {
        Self::open_store(
            UpdateStore::Object(store),
            prefix,
            name,
            "object-store".to_string(),
        )
    }

    fn open_store(
        store: UpdateStore,
        prefix: &str,
        name: &str,
        store_label: String,
    ) -> Result<UpdateRepo, UpdateErr> {
        validate_prefix(prefix)?;
        validate_name(name)?;
        Ok(UpdateRepo {
            store,
            prefix: prefix.trim_matches('/').to_string(),
            name: name.to_string(),
            store_label,
        })
    }

    pub fn from_config(
        config: &MediaConfig,
        prefix: &str,
        name: &str,
    ) -> Result<UpdateRepo, UpdateErr> {
        let store = MediaStore::open(config)?;
        Self::open_store(
            UpdateStore::Object(store),
            prefix,
            name,
            media_label(config),
        )
    }

    /// Open a repository from an `fs:`, `s3:`, `http:`, `https:`, `h2c:`,
    /// `quic:` or `h3:` locator.
    pub fn from_locator(spec: &str, prefix: &str, name: &str) -> Result<UpdateRepo, UpdateErr> {
        Self::from_locator_with_ca(spec, prefix, name, None)
    }

    /// Open a repository, adding a PEM certificate authority for HTTPS or
    /// QUIC. Use this for a private CA or a test certificate.
    pub fn from_locator_with_ca(
        spec: &str,
        prefix: &str,
        name: &str,
        ca_file: Option<&Path>,
    ) -> Result<UpdateRepo, UpdateErr> {
        if let Some(store) = HttpStore::parse(spec, ca_file)? {
            return Self::open_store(
                UpdateStore::Http(store),
                prefix,
                name,
                format!("http:{spec}"),
            );
        }
        let config = parse_media_spec(spec)?;
        let store = MediaStore::open(&config)?;
        Self::open_store(
            UpdateStore::Object(store),
            prefix,
            name,
            media_label(&config),
        )
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Stable label for this repository's object store, recorded by the
    /// workspace catalog.
    pub fn store_label(&self) -> &str {
        &self.store_label
    }

    /// Publish `root` as `release`. Chunks are uploaded before the manifest,
    /// and the ref is written last.
    pub fn publish(
        &self,
        release: &str,
        root: &Path,
        signing_key: Option<&ManifestSigningKey>,
    ) -> Result<PublishReport, UpdateErr> {
        self.publish_with_compression(release, root, signing_key, None)
    }

    /// Publish with the default zstd level when `compress` is true.
    pub fn publish_with_options(
        &self,
        release: &str,
        root: &Path,
        signing_key: Option<&ManifestSigningKey>,
        compress: bool,
    ) -> Result<PublishReport, UpdateErr> {
        self.publish_with_compression(
            release,
            root,
            signing_key,
            compress.then_some(DEFAULT_COMPRESSION_LEVEL),
        )
    }

    /// Publish with optional zstd chunk compression at an explicit level.
    /// Compression is per chunk and only kept when it makes the stored object
    /// smaller.
    pub fn publish_with_compression(
        &self,
        release: &str,
        root: &Path,
        signing_key: Option<&ManifestSigningKey>,
        compression_level: Option<i32>,
    ) -> Result<PublishReport, UpdateErr> {
        if let Some(level) = compression_level
            && !(1..=MAX_COMPRESSION_LEVEL).contains(&level)
        {
            return Err(UpdateErr::InvalidCompressionLevel(level));
        }
        self.store.ensure_writable("publishing")?;
        validate_release(release)?;
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

        let mut available = self.existing_chunks()?;
        let mut uploaded_chunks = 0usize;
        let mut uploaded_bytes = 0u64;
        let mut reused_chunks = 0usize;
        let store = &self.store;
        let chunk_key =
            |hash: &str, encoding: Option<ChunkEncoding>| self.chunk_key(hash, encoding);
        let manifest = scan_tree(
            &root,
            &self.name,
            release,
            compression_level,
            |hash, data, encoding| {
                let size = data.len() as u64;
                let id = chunk_object_id(hash, encoding);
                if available.contains(&id) {
                    reused_chunks += 1;
                    return Ok(());
                }
                let key = chunk_key(hash, encoding);
                store.write_bytes(&key, data)?;
                available.insert(id);
                uploaded_chunks += 1;
                uploaded_bytes += size;
                Ok(())
            },
        )?;

        let bytes = serde_json::to_vec_pretty(&manifest)?;
        let hash = blake3_hash(&bytes);
        let ref_key = self.ref_key(release);
        let mut signatures = Vec::new();
        let mut created_at = now_unix();
        let mut exists = false;
        if self.store.stat(&ref_key)?.is_some() {
            exists = true;
            let existing = parse_ref(&self.read_text(&ref_key, MAX_REF)?)?;
            if existing.manifest != hash {
                return Err(UpdateErr::ReleaseExists {
                    name: self.name.clone(),
                    release: release.to_string(),
                    existing: existing.manifest,
                    proposed: hash,
                });
            }
            signatures = existing.signatures;
            created_at = existing.created_at.unwrap_or(created_at);
        }
        if let Some(key) = signing_key {
            let signature = key.signature(&bytes);
            signatures.retain(|existing| existing.public_key != signature.public_key);
            signatures.push(signature);
        }

        let key = self.manifest_key(&hash);
        if store.stat(&key)?.is_none() {
            store.write_bytes(&key, &bytes)?;
        }
        let ref_bytes = if signatures.is_empty() {
            hash.as_bytes().to_vec()
        } else {
            serde_json::to_vec_pretty(&RefEnvelope {
                schema: 1,
                manifest: hash.clone(),
                created_at,
                signatures,
            })?
        };
        if !exists || signing_key.is_some() {
            self.store.write_bytes(&ref_key, &ref_bytes)?;
        }

        Ok(PublishReport {
            name: self.name.clone(),
            release: release.to_string(),
            manifest: hash,
            files: manifest.files.len(),
            chunks: manifest.total_chunks(),
            bytes: manifest.total_bytes(),
            uploaded_chunks,
            uploaded_bytes,
            reused_chunks,
            compressed_chunks: manifest
                .files
                .iter()
                .flat_map(|file| &file.chunks)
                .filter(|chunk| chunk.encoding.is_some())
                .count(),
            compression_level,
        })
    }

    /// Load and verify the manifest selected by `release`. When `trust` is
    /// present, a matching signature is required before the JSON is parsed.
    pub fn manifest(
        &self,
        release: &str,
        trust: Option<&ManifestPublicKey>,
    ) -> Result<ReleaseManifest, UpdateErr> {
        Ok(self.load_manifest(release, trust)?.0)
    }

    /// Load a manifest and return its content hash as well.
    pub fn manifest_with_hash(
        &self,
        release: &str,
        trust: Option<&ManifestPublicKey>,
    ) -> Result<(ReleaseManifest, String), UpdateErr> {
        let (manifest, hash, _) = self.load_manifest(release, trust)?;
        Ok((manifest, hash))
    }

    fn load_manifest(
        &self,
        release: &str,
        trust: Option<&ManifestPublicKey>,
    ) -> Result<(ReleaseManifest, String, Vec<u8>), UpdateErr> {
        validate_release(release)?;
        let ref_key = self.ref_key(release);
        let reference = parse_ref(&self.read_text(&ref_key, MAX_REF)?)?;
        let expected = reference.manifest.clone();
        let manifest_key = self.manifest_key(&expected);
        let data = self.read_bytes(&manifest_key, MAX_MANIFEST)?;
        let actual = blake3_hash(&data);
        if actual != expected {
            return Err(UpdateErr::ManifestMismatch {
                name: self.name.clone(),
                release: release.to_string(),
                expected,
                actual,
            });
        }
        if let Some(public_key) = trust {
            if reference.signatures.is_empty() {
                return Err(UpdateErr::UnsignedRelease {
                    name: self.name.clone(),
                    release: release.to_string(),
                });
            }
            if !reference
                .signatures
                .iter()
                .any(|signature| public_key.verify(&data, signature))
            {
                return Err(UpdateErr::BadSignature {
                    name: self.name.clone(),
                    release: release.to_string(),
                });
            }
        }
        let manifest: ReleaseManifest = serde_json::from_slice(&data)?;
        manifest.validate(&self.name, Some(release))?;
        Ok((manifest, expected, data))
    }

    /// Compare a target release with an installed-state manifest.
    pub fn plan(
        &self,
        release: &str,
        installed: Option<&ReleaseManifest>,
        trust: Option<&ManifestPublicKey>,
    ) -> Result<UpdatePlan, UpdateErr> {
        let target = self.manifest(release, trust)?;
        if let Some(state) = installed {
            state.validate(&self.name, None)?;
        }

        let old_files: HashMap<&str, &UpdateFile> = installed
            .map(|state| {
                state
                    .files
                    .iter()
                    .map(|file| (file.path.as_str(), file))
                    .collect()
            })
            .unwrap_or_default();
        let old_chunks: HashSet<&str> = installed
            .map(|state| {
                state
                    .files
                    .iter()
                    .flat_map(|file| file.chunks.iter())
                    .map(|chunk| chunk.hash.as_str())
                    .collect()
            })
            .unwrap_or_default();

        let mut plan = UpdatePlan {
            files_total: target.files.len(),
            bytes_total: target.total_bytes(),
            ..Default::default()
        };
        let mut target_paths = HashSet::new();
        let mut missing_chunks: HashMap<&str, u32> = HashMap::new();
        for file in &target.files {
            target_paths.insert(file.path.as_str());
            match old_files.get(file.path.as_str()) {
                None => plan.files_new += 1,
                Some(old) if old.hash == file.hash => plan.files_unchanged += 1,
                Some(_) => plan.files_changed += 1,
            }
            for chunk in &file.chunks {
                plan.chunks_total += 1;
                if !old_chunks.contains(chunk.hash.as_str()) {
                    missing_chunks.insert(chunk.hash.as_str(), chunk.size);
                }
            }
        }
        plan.chunks_missing = missing_chunks.len();
        plan.bytes_missing = missing_chunks.values().map(|size| u64::from(*size)).sum();
        plan.files_removed = old_files
            .keys()
            .filter(|path| !target_paths.contains(**path))
            .count();
        Ok(plan)
    }

    /// Refs currently in this repository.
    pub fn releases(&self) -> Result<Vec<ReleaseInfo>, UpdateErr> {
        let prefix = format!("{}/{}/refs/", self.prefix, self.name);
        let mut releases = Vec::new();
        for object in self.store.list(&prefix)? {
            let Some(release) = object.key.strip_prefix(&prefix) else {
                continue;
            };
            if release.is_empty() || release.contains('/') {
                continue;
            }
            let reference = parse_ref(&self.read_text(&object.key, MAX_REF)?)?;
            let modified = match object.modified {
                Some(modified) => Some(modified),
                None => self.store.stat(&object.key)?.and_then(|meta| meta.modified),
            };
            releases.push(ReleaseInfo {
                release: release.to_string(),
                manifest: reference.manifest,
                signatures: reference.signatures.len(),
                created_at: reference.created_at,
                modified,
            });
        }
        releases.sort_by(|left, right| left.release.cmp(&right.release));
        Ok(releases)
    }

    /// Delete old release refs while keeping the `keep` most recently
    /// published. Refs within `grace_secs` are also kept. Manifests and chunks
    /// are left for [`Self::gc`].
    pub fn prune_refs(
        &self,
        keep: usize,
        grace_secs: u64,
        dry_run: bool,
    ) -> Result<RefPruneReport, UpdateErr> {
        self.store.ensure_writable("pruning release refs")?;
        if keep == 0 {
            return Err(UpdateErr::InvalidPrune(
                "--keep must be at least 1".to_string(),
            ));
        }
        let mut releases = self.releases()?;
        let now = now_unix();
        let cutoff = now.saturating_sub(grace_secs as i64);
        releases.sort_by(|left, right| {
            release_time(right)
                .cmp(&release_time(left))
                .then_with(|| right.release.cmp(&left.release))
        });

        let mut report = RefPruneReport { dry_run, ..Default::default() };
        for (index, release) in releases.iter().enumerate() {
            let stamp = release.created_at.or(release.modified);
            let recent = stamp.is_none_or(|stamp| grace_secs != 0 && stamp > cutoff);
            if index < keep || recent {
                report.kept.push(release.release.clone());
                continue;
            }
            if !dry_run {
                self.store.delete(&self.ref_key(&release.release))?;
            }
            report.removed.push(release.release.clone());
        }
        report.kept.sort();
        report.removed.sort();
        Ok(report)
    }

    /// Remove manifests and chunks that no current ref reaches.
    ///
    /// All refs and manifests are read before anything is deleted; a broken ref
    /// aborts the pass. Objects newer than `grace_secs` are kept so a publish
    /// that has uploaded chunks but not yet written its ref cannot be collected.
    /// Refs themselves are never deleted.
    pub fn gc(&self, grace_secs: u64, dry_run: bool) -> Result<GcReport, UpdateErr> {
        self.gc_with_pins(grace_secs, dry_run, &[])
    }

    /// Garbage collection that also preserves manifests pinned by the workspace
    /// catalog, even if their release ref has been pruned.
    pub fn gc_with_pins(
        &self,
        grace_secs: u64,
        dry_run: bool,
        pinned_manifests: &[String],
    ) -> Result<GcReport, UpdateErr> {
        self.store.ensure_writable("garbage collection")?;
        let releases = self.releases()?;
        let mut live_manifests = HashSet::new();
        let mut live_chunks = HashSet::new();
        for release in &releases {
            let (manifest, hash, _) = self.load_manifest(&release.release, None)?;
            live_manifests.insert(digest(&hash).to_string());
            for file in manifest.files {
                for chunk in file.chunks {
                    live_chunks.insert(chunk_object_id(&chunk.hash, chunk.encoding));
                }
            }
        }
        for pin in pinned_manifests {
            validate_hash(pin)?;
            if !live_manifests.insert(digest(pin).to_string()) {
                continue;
            }
            let data = self.read_bytes(&self.manifest_key(pin), MAX_MANIFEST)?;
            let actual = blake3_hash(&data);
            if actual != *pin {
                return Err(UpdateErr::ManifestMismatch {
                    name: self.name.clone(),
                    release: "(catalog pin)".to_string(),
                    expected: pin.clone(),
                    actual,
                });
            }
            let manifest: ReleaseManifest = serde_json::from_slice(&data)?;
            manifest.validate(&self.name, None)?;
            for file in manifest.files {
                for chunk in file.chunks {
                    live_chunks.insert(chunk_object_id(&chunk.hash, chunk.encoding));
                }
            }
        }

        let cutoff = now_unix().saturating_sub(grace_secs as i64);
        let mut report = GcReport { dry_run, ..Default::default() };
        self.gc_prefix(
            &format!("{}/{}/manifests/", self.prefix, self.name),
            &live_manifests,
            cutoff,
            grace_secs,
            dry_run,
            true,
            &mut report,
        )?;
        self.gc_prefix(
            &format!("{}/{}/chunks/", self.prefix, self.name),
            &live_chunks,
            cutoff,
            grace_secs,
            dry_run,
            false,
            &mut report,
        )?;
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    fn gc_prefix(
        &self,
        prefix: &str,
        live: &HashSet<String>,
        cutoff: i64,
        grace_secs: u64,
        dry_run: bool,
        manifests: bool,
        report: &mut GcReport,
    ) -> Result<(), UpdateErr> {
        for object in self.store.list(prefix)? {
            let Some(name) = object.key.rsplit('/').next() else {
                continue;
            };
            let hash = if manifests {
                name.strip_suffix(".json").unwrap_or(name)
            } else {
                name.strip_suffix(".zst").unwrap_or(name)
            };
            let id = if manifests { hash } else { name };
            if validate_hex(hash).is_err() || live.contains(id) {
                continue;
            }
            if grace_secs != 0 {
                let Some(modified) = object.modified else {
                    continue;
                };
                if modified > cutoff {
                    continue;
                }
            }
            let size = self.store.stat(&object.key)?.map_or(0, |meta| meta.size);
            if !dry_run {
                self.store.delete(&object.key)?;
            }
            report.bytes_removed += size;
            if manifests {
                report.manifests_removed += 1;
            } else {
                report.chunks_removed += 1;
            }
        }
        Ok(())
    }

    /// Reconstruct `release` into `root`, reusing chunks from the installed
    /// state's files where possible.
    ///
    /// Files are assembled in a sibling staging tree and moved into place only
    /// after every file verifies. `state` is written last. `delete` removes
    /// files that belonged to the old state and are absent from the new release.
    #[allow(clippy::too_many_arguments)]
    pub fn apply(
        &self,
        release: &str,
        root: &Path,
        state_path: &Path,
        installed: Option<&ReleaseManifest>,
        delete: bool,
        trust: Option<&ManifestPublicKey>,
        reuse_root: Option<&Path>,
    ) -> Result<ApplyReport, UpdateErr> {
        let target = self.manifest(release, trust)?;
        if let Some(state) = installed {
            state.validate(&self.name, None)?;
        }
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;

        let local_root = reuse_root.unwrap_or(&root);
        let local = installed.map(local_chunks).transpose()?.unwrap_or_default();
        let old_files: HashMap<&str, &UpdateFile> = installed
            .map(|state| {
                state
                    .files
                    .iter()
                    .map(|file| (file.path.as_str(), file))
                    .collect()
            })
            .unwrap_or_default();

        let staging = Staging::create(&root)?;
        let tree = staging.path.join("tree");
        let chunk_cache = staging.path.join("chunks");
        fs::create_dir_all(&tree)?;
        fs::create_dir_all(&chunk_cache)?;

        let mut report = ApplyReport {
            name: self.name.clone(),
            release: release.to_string(),
            manifest: blake3_hash(&serde_json::to_vec_pretty(&target)?),
            ..Default::default()
        };
        let mut staged = Vec::new();
        let mut downloaded = HashSet::new();

        for file in &target.files {
            let relative = safe_relative(&file.path)?;
            let destination = root.join(&relative);
            let unchanged = old_files
                .get(file.path.as_str())
                .is_some_and(|old| old.hash == file.hash)
                && fs::symlink_metadata(&destination)
                    .is_ok_and(|meta| meta.is_file() && meta.len() == file.size)
                && code::mode_of(&fs::symlink_metadata(&destination)?) == file.mode;
            if unchanged {
                report.files_unchanged += 1;
                continue;
            }

            let stage = tree.join(&relative);
            ensure_parent(&tree, &relative, true)?;
            let (mut whole, mut written, resumed_chunks) = resume_staged(&stage, file)?;
            report.chunks_resumed += resumed_chunks;
            report.bytes_resumed += written;
            let mut out = match fs::symlink_metadata(&stage) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                    return Err(UpdateErr::UnsafePath(format!(
                        "{}: staging path is not a regular file",
                        stage.display()
                    )));
                }
                Ok(_) => fs::OpenOptions::new().read(true).write(true).open(&stage)?,
                Err(err) if err.kind() == io::ErrorKind::NotFound => File::create(&stage)?,
                Err(err) => return Err(err.into()),
            };
            out.set_len(written)?;
            out.seek(SeekFrom::Start(written))?;
            for chunk in &file.chunks[resumed_chunks..] {
                let data = if let Some(data) =
                    try_local_chunk(local_root, &local, &chunk.hash, chunk.size)?
                {
                    report.chunks_reused += 1;
                    report.bytes_reused += u64::from(chunk.size);
                    data
                } else {
                    let (data, fetched) = self.fetch_chunk(&chunk_cache, chunk, &mut downloaded)?;
                    if fetched {
                        report.chunks_downloaded += 1;
                        report.bytes_downloaded += u64::from(chunk.size);
                    } else {
                        report.chunks_reused += 1;
                        report.bytes_reused += u64::from(chunk.size);
                    }
                    data
                };
                whole.update(&data);
                out.write_all(&data)?;
                written += data.len() as u64;
            }
            out.sync_all()?;
            let actual = format!("blake3:{}", whole.finalize().to_hex());
            if actual != file.hash || written != file.size {
                return Err(UpdateErr::FileMismatch {
                    path: file.path.clone(),
                    expected: file.hash.clone(),
                    actual,
                });
            }
            code::set_mode(&stage, file.mode)?;
            staged.push((relative, stage));
            report.files_written += 1;
        }

        for (relative, stage) in staged {
            ensure_parent(&root, &relative, true)?;
            let destination = root.join(&relative);
            #[cfg(windows)]
            if destination.exists() {
                fs::remove_file(&destination)?;
            }
            fs::rename(stage, destination)?;
        }

        if delete {
            let target_paths: HashSet<&str> =
                target.files.iter().map(|file| file.path.as_str()).collect();
            for old in installed.into_iter().flat_map(|state| &state.files) {
                if target_paths.contains(old.path.as_str()) {
                    continue;
                }
                let relative = safe_relative(&old.path)?;
                ensure_parent(&root, &relative, false)?;
                let path = root.join(relative);
                match fs::remove_file(&path) {
                    Ok(()) => report.files_deleted += 1,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.into()),
                }
            }
        }

        write_atomic(state_path, &serde_json::to_vec_pretty(&target)?)?;
        // The state is durable; a staging tree is only a cache now. Leave it
        // alone if cleanup fails rather than reporting a successful update as
        // failed.
        let _ = fs::remove_dir_all(&staging.path);
        Ok(report)
    }
    fn existing_chunks(&self) -> Result<HashSet<String>, UpdateErr> {
        let prefix = format!("{}/{}/chunks/", self.prefix, self.name);
        let mut chunks = HashSet::new();
        for object in self.store.list(&prefix)? {
            let Some(name) = object.key.rsplit('/').next() else {
                continue;
            };
            if validate_hex(name).is_ok() {
                chunks.insert(name.to_string());
            } else if let Some(hash) = name.strip_suffix(".zst")
                && validate_hex(hash).is_ok()
            {
                chunks.insert(name.to_string());
            }
        }
        Ok(chunks)
    }

    fn fetch_chunk(
        &self,
        cache: &Path,
        chunk: &UpdateChunk,
        downloaded: &mut HashSet<String>,
    ) -> Result<(Vec<u8>, bool), UpdateErr> {
        let path = cache.join(digest(&chunk.hash));
        if let Ok(data) = fs::read(&path)
            && data.len() == chunk.size as usize
            && blake3_hash(&data) == chunk.hash
        {
            return Ok((data, false));
        }

        let key = self.chunk_key(&chunk.hash, chunk.encoding);
        let Some(meta) = self.store.stat(&key)? else {
            return Err(UpdateErr::MissingChunk { hash: chunk.hash.clone() });
        };
        if chunk.encoding.is_none() && meta.size != u64::from(chunk.size) {
            return Err(UpdateErr::ChunkSize {
                hash: chunk.hash.clone(),
                expected: chunk.size,
                actual: meta.size,
            });
        }
        if chunk.encoding.is_some() && meta.size > u64::from(chunk.size) {
            return Err(UpdateErr::CorruptChunk { hash: chunk.hash.clone() });
        }
        let stored = self.store.read_range(&key, 0, Some(meta.size))?;
        let data = decode_chunk(chunk, stored)?;
        if data.len() != chunk.size as usize {
            return Err(UpdateErr::ChunkSize {
                hash: chunk.hash.clone(),
                expected: chunk.size,
                actual: data.len() as u64,
            });
        }
        if blake3_hash(&data) != chunk.hash {
            return Err(UpdateErr::CorruptChunk { hash: chunk.hash.clone() });
        }
        write_atomic(&path, &data)?;
        let fetched = downloaded.insert(chunk.hash.clone());
        Ok((data, fetched))
    }

    fn read_text(&self, key: &str, max: u64) -> Result<String, UpdateErr> {
        let bytes = self.read_bytes(key, max)?;
        String::from_utf8(bytes).map_err(|err| {
            UpdateErr::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{key} is not UTF-8: {err}"),
            ))
        })
    }

    fn read_bytes(&self, key: &str, max: u64) -> Result<Vec<u8>, UpdateErr> {
        let Some(meta) = self.store.stat(key)? else {
            return Err(UpdateErr::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{key} is missing"),
            )));
        };
        if meta.size > max {
            return Err(UpdateErr::ManifestTooLarge(key.to_string()));
        }
        self.store.read_range(key, 0, Some(meta.size))
    }

    fn root_key(&self) -> String {
        format!("{}/{}", self.prefix, self.name)
    }

    fn ref_key(&self, release: &str) -> String {
        format!("{}/refs/{release}", self.root_key())
    }

    fn manifest_key(&self, hash: &str) -> String {
        format!(
            "{}/manifests/{}.json",
            self.root_key(),
            hash.strip_prefix("blake3:").unwrap_or(hash)
        )
    }

    fn chunk_key(&self, hash: &str, encoding: Option<ChunkEncoding>) -> String {
        let id = chunk_object_id(hash, encoding);
        format!("{}/chunks/{}/{id}", self.root_key(), &id[..2])
    }
}

fn media_label(config: &MediaConfig) -> String {
    match config {
        MediaConfig::Fs { root } => {
            let path = fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
            format!("fs:{}", path.display())
        }
        MediaConfig::S3Compatible { bucket, prefix } => match prefix {
            Some(prefix) if !prefix.is_empty() => {
                format!("s3:{bucket}/{}", prefix.trim_matches('/'))
            }
            _ => format!("s3:{bucket}"),
        },
    }
}

fn digest(hash: &str) -> &str {
    hash.strip_prefix("blake3:").unwrap_or(hash)
}

fn chunk_object_id(hash: &str, encoding: Option<ChunkEncoding>) -> String {
    format!(
        "{}{}",
        digest(hash),
        encoding.map(ChunkEncoding::suffix).unwrap_or_default()
    )
}

fn encode_chunk(
    data: &[u8],
    compression_level: Option<i32>,
) -> Result<(Vec<u8>, Option<ChunkEncoding>), UpdateErr> {
    let Some(level) = compression_level else {
        return Ok((data.to_vec(), None));
    };
    let compressed = zstd::bulk::compress(data, level).map_err(|err| {
        UpdateErr::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("zstd chunk compression: {err}"),
        ))
    })?;
    if compressed.len() < data.len() {
        Ok((compressed, Some(ChunkEncoding::Zstd)))
    } else {
        Ok((data.to_vec(), None))
    }
}

fn decode_chunk(chunk: &UpdateChunk, stored: Vec<u8>) -> Result<Vec<u8>, UpdateErr> {
    let Some(encoding) = chunk.encoding else {
        return Ok(stored);
    };
    match encoding {
        ChunkEncoding::Zstd => {
            zstd::bulk::decompress(&stored, chunk.size as usize).map_err(|err| {
                UpdateErr::CorruptEncoding {
                    hash: chunk.hash.clone(),
                    encoding: encoding.name().to_string(),
                    reason: err.to_string(),
                }
            })
        }
    }
}

/// Build an installed-state manifest by scanning `root` with `FastCDC`, without
/// uploading anything.
pub fn build_state(root: &Path, name: &str, release: &str) -> Result<ReleaseManifest, UpdateErr> {
    validate_name(name)?;
    validate_release(release)?;
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        )
        .into());
    }
    scan_tree(&root, name, release, None, |_, _, _| Ok(()))
}

/// Write an installed-state manifest atomically.
pub fn write_state(path: &Path, manifest: &ReleaseManifest) -> Result<(), UpdateErr> {
    manifest.validate(&manifest.name, Some(&manifest.release))?;
    write_atomic(path, &serde_json::to_vec_pretty(manifest)?)
}

/// Read and validate an installed-state manifest.
pub fn read_state(path: &Path, expected_name: &str) -> Result<ReleaseManifest, UpdateErr> {
    let data = fs::read(path)?;
    let manifest: ReleaseManifest = serde_json::from_slice(&data)?;
    manifest.validate(expected_name, None)?;
    Ok(manifest)
}

/// Parse `fs:DIR` or `s3:BUCKET[/PREFIX]` for an update repository.
pub fn parse_media_spec(spec: &str) -> Result<MediaConfig, UpdateErr> {
    if let Some(root) = spec.strip_prefix("fs:") {
        if root.is_empty() {
            return Err(UpdateErr::InvalidPrefix(spec.to_string()));
        }
        return Ok(MediaConfig::Fs { root: root.to_string() });
    }
    if let Some(rest) = spec.strip_prefix("s3:") {
        let (bucket, prefix) = match rest.split_once('/') {
            Some((bucket, prefix)) if !bucket.is_empty() => {
                (bucket.to_string(), Some(prefix.to_string()))
            }
            Some(_) => return Err(UpdateErr::InvalidPrefix(spec.to_string())),
            None if !rest.is_empty() => (rest.to_string(), None),
            None => return Err(UpdateErr::InvalidPrefix(spec.to_string())),
        };
        return Ok(MediaConfig::S3Compatible { bucket, prefix });
    }
    Err(UpdateErr::InvalidPrefix(spec.to_string()))
}

fn scan_tree(
    root: &Path,
    name: &str,
    release: &str,
    compression_level: Option<i32>,
    mut on_chunk: impl FnMut(&str, &[u8], Option<ChunkEncoding>) -> Result<(), UpdateErr>,
) -> Result<ReleaseManifest, UpdateErr> {
    let mut files = Vec::new();
    for relative in code::collect_files(root)? {
        let full = root.join(&relative);
        let metadata = match fs::metadata(&full) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        let mut whole = Blake3::new();
        let mut chunks = Vec::new();
        let mut size = 0u64;
        for chunk in chunk_stream(File::open(&full)?) {
            let data = chunk?;
            whole.update(&data);
            size += data.len() as u64;
            let hash = blake3_hash(&data);
            let (stored, encoding) = encode_chunk(&data, compression_level)?;
            on_chunk(&hash, &stored, encoding)?;
            chunks.push(UpdateChunk {
                hash,
                size: u32::try_from(data.len()).map_err(|_| UpdateErr::ChunkSize {
                    hash: String::from("chunk"),
                    expected: u32::MAX,
                    actual: data.len() as u64,
                })?,
                encoding,
            });
        }
        if size != metadata.len() {
            return Err(UpdateErr::FileChanged { path: relative });
        }
        files.push(UpdateFile {
            path: relative,
            size,
            mode: code::mode_of(&metadata),
            hash: format!("blake3:{}", whole.finalize().to_hex()),
            chunks,
        });
    }
    Ok(ReleaseManifest {
        schema: MANIFEST_SCHEMA,
        name: name.to_string(),
        release: release.to_string(),
        files,
    })
}

#[derive(Debug)]
struct LocalChunk {
    path: String,
    offset: u64,
    size: u32,
}

fn local_chunks(state: &ReleaseManifest) -> Result<HashMap<String, Vec<LocalChunk>>, UpdateErr> {
    let mut chunks: HashMap<String, Vec<LocalChunk>> = HashMap::new();
    for file in &state.files {
        let mut offset = 0u64;
        for chunk in &file.chunks {
            chunks
                .entry(chunk.hash.clone())
                .or_default()
                .push(LocalChunk {
                    path: file.path.clone(),
                    offset,
                    size: chunk.size,
                });
            offset += u64::from(chunk.size);
        }
        if offset != file.size {
            return Err(UpdateErr::EmptyInstalledChunk(file.path.clone()));
        }
        // Missing local files simply make their chunks unavailable; the
        // candidate lookup opens each path lazily.
    }
    Ok(chunks)
}

fn try_local_chunk(
    root: &Path,
    local: &HashMap<String, Vec<LocalChunk>>,
    hash: &str,
    size: u32,
) -> Result<Option<Vec<u8>>, UpdateErr> {
    let Some(candidates) = local.get(hash) else {
        return Ok(None);
    };
    for candidate in candidates {
        if candidate.size != size {
            continue;
        }
        let relative = safe_relative(&candidate.path)?;
        ensure_parent(root, &relative, false)?;
        let path = root.join(relative);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        if file.seek(SeekFrom::Start(candidate.offset)).is_err() {
            continue;
        }
        let mut data = vec![0u8; size as usize];
        if file.read_exact(&mut data).is_err() {
            continue;
        }
        if blake3_hash(&data) == hash {
            return Ok(Some(data));
        }
    }
    Ok(None)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), UpdateErr> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        ticket()
    ));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// A deterministic sibling workspace for one destination tree.
///
/// The path is stable so an interrupted apply can resume downloaded chunks and
/// partially written files. It is removed only after the installed state has
/// been written, so a crash never discards progress.
struct Staging {
    path: PathBuf,
}

impl Staging {
    fn create(root: &Path) -> Result<Staging, UpdateErr> {
        let parent = root.parent().unwrap_or_else(|| Path::new("."));
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("root");
        let path = parent.join(format!(".{name}.graft-update"));
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(UpdateErr::UnsafePath(format!(
                    "{}: staging path is not a directory",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(err) => return Err(err.into()),
        }
        fs::create_dir_all(path.join("tree"))?;
        fs::create_dir_all(path.join("chunks"))?;
        Ok(Staging { path })
    }
}

/// Read the verified prefix of a staged file and return its hash state, byte
/// length and complete chunk count. Any mismatch or short read restarts the
/// file at zero; the caller truncates the staging file before appending.
fn resume_staged(path: &Path, file: &UpdateFile) -> Result<(Blake3, u64, usize), UpdateErr> {
    let mut whole = Blake3::new();
    let mut bytes = 0u64;
    let mut chunks = 0usize;
    let mut input = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(UpdateErr::UnsafePath(format!(
                "{}: staging path is not a regular file",
                path.display()
            )));
        }
        Ok(_) => match File::open(path) {
            Ok(file) => file,
            Err(_) => return Ok((whole, 0, 0)),
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((whole, 0, 0)),
        Err(err) => return Err(err.into()),
    };

    for chunk in &file.chunks {
        let mut data = vec![0u8; chunk.size as usize];
        if input.read_exact(&mut data).is_err() {
            break;
        }
        if blake3_hash(&data) != chunk.hash {
            break;
        }
        whole.update(&data);
        bytes += u64::from(chunk.size);
        chunks += 1;
    }
    Ok((whole, bytes, chunks))
}

fn validate_name(name: &str) -> Result<(), UpdateErr> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(UpdateErr::InvalidName(name.to_string()));
    }
    Ok(())
}

fn validate_release(release: &str) -> Result<(), UpdateErr> {
    if release.is_empty()
        || release == "."
        || release == ".."
        || release.len() > 128
        || !release
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(UpdateErr::InvalidRelease(release.to_string()));
    }
    Ok(())
}

fn validate_prefix(prefix: &str) -> Result<(), UpdateErr> {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty()
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(UpdateErr::InvalidPrefix(prefix.to_string()));
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<(), UpdateErr> {
    let Some(hex) = hash.strip_prefix("blake3:") else {
        return Err(UpdateErr::InvalidHash(hash.to_string()));
    };
    validate_hex(hex).map_err(|_| UpdateErr::InvalidHash(hash.to_string()))
}

fn validate_hex(hex: &str) -> Result<(), ()> {
    if hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(())
    }
}

/// Create or check every parent component without following symlinks. A
/// manifest cannot turn an existing symlink into a write outside the root.
fn ensure_parent(root: &Path, relative: &Path, create: bool) -> Result<(), UpdateErr> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = root.to_path_buf();
    for component in parent.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(UpdateErr::UnsafePath(format!(
                    "{}: {} is a symlink or not a directory",
                    relative.display(),
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if create {
                    fs::create_dir(&current)?;
                } else {
                    return Ok(());
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn safe_relative(path: &str) -> Result<PathBuf, UpdateErr> {
    let candidate = Path::new(path);
    if path.is_empty() || path.contains('\0') || path.contains('\\') || candidate.is_absolute() {
        return Err(UpdateErr::UnsafePath(path.to_string()));
    }
    let mut safe = PathBuf::new();
    for component in candidate.components() {
        match component {
            std::path::Component::Normal(part) => safe.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(UpdateErr::UnsafePath(path.to_string()));
            }
        }
    }
    if safe.as_os_str().is_empty() {
        return Err(UpdateErr::UnsafePath(path.to_string()));
    }
    Ok(safe)
}

fn parse_ref(text: &str) -> Result<LoadedRef, UpdateErr> {
    let text = text.trim();
    if let Some(hash) = text.strip_prefix("blake3:") {
        validate_hex(hash).map_err(|_| UpdateErr::InvalidRef(text.to_string()))?;
        return Ok(LoadedRef {
            manifest: text.to_string(),
            created_at: None,
            signatures: Vec::new(),
        });
    }
    let envelope: RefEnvelope = serde_json::from_str(text)
        .map_err(|err| UpdateErr::InvalidRef(format!("ref JSON: {err}")))?;
    if envelope.schema != 1 {
        return Err(UpdateErr::InvalidRef(format!(
            "unsupported ref schema {}",
            envelope.schema
        )));
    }
    validate_hash(&envelope.manifest)?;
    for signature in &envelope.signatures {
        if signature.algorithm != "ed25519" {
            return Err(UpdateErr::InvalidRef(format!(
                "unsupported signature algorithm {:?}",
                signature.algorithm
            )));
        }
        let Some(key) = signature.public_key.strip_prefix("ed25519:") else {
            return Err(UpdateErr::InvalidRef("public key is not ed25519".into()));
        };
        decode_hex::<32>(key)
            .map_err(|_| UpdateErr::InvalidRef("bad ed25519 public key".into()))?;
        decode_hex::<64>(&signature.signature)
            .map_err(|_| UpdateErr::InvalidRef("bad ed25519 signature".into()))?;
    }
    Ok(LoadedRef {
        manifest: envelope.manifest,
        created_at: Some(envelope.created_at),
        signatures: envelope.signatures,
    })
}

fn signing_message(manifest: &[u8]) -> Vec<u8> {
    let mut message = b"graft-update-manifest-v1\0".to_vec();
    message.extend_from_slice(manifest);
    message
}

fn read_key_bytes(path: &Path) -> Result<[u8; 32], UpdateErr> {
    let text = fs::read_to_string(path).map_err(|err| UpdateErr::InvalidKey {
        path: path.display().to_string(),
        reason: err.to_string(),
    })?;
    decode_hex::<32>(text.trim()).map_err(|_| UpdateErr::InvalidKey {
        path: path.display().to_string(),
        reason: "expected 64 hex characters".to_string(),
    })
}

fn decode_hex<const N: usize>(text: &str) -> Result<[u8; N], ()> {
    if text.len() != N * 2 {
        return Err(());
    }
    let mut bytes = [0u8; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn blake3_hash(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn ticket() -> String {
    let hi = rand::random::<u64>().to_le_bytes();
    let lo = rand::random::<u64>().to_le_bytes();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi);
    bytes[8..].copy_from_slice(&lo);
    bs58::encode(&bytes[..12]).into_string()
}

fn release_time(release: &ReleaseInfo) -> i64 {
    release.created_at.or(release.modified).unwrap_or(0)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> UpdateFile {
        UpdateFile {
            path: path.into(),
            size,
            mode: 0o644,
            hash: format!("blake3:{}", "00".repeat(32)),
            chunks: vec![UpdateChunk {
                hash: format!("blake3:{}", "11".repeat(32)),
                size: u32::try_from(size).unwrap(),
                encoding: None,
            }],
        }
    }

    fn manifest(files: Vec<UpdateFile>) -> ReleaseManifest {
        ReleaseManifest {
            schema: MANIFEST_SCHEMA,
            name: "app".into(),
            release: "1".into(),
            files,
        }
    }

    #[test]
    fn rejects_non_canonical_hashes() {
        for hash in ["", "blake3:", "blake3:abc", "sha256:00"] {
            assert!(validate_hash(hash).is_err(), "{hash:?}");
        }
        assert!(validate_hash(&format!("blake3:{}", "00".repeat(32))).is_ok());
    }

    #[test]
    fn rejects_manifest_path_traversal_and_ragged_chunks() {
        let mut bad = manifest(vec![file("../escape", 1)]);
        assert!(matches!(
            bad.validate("app", Some("1")),
            Err(UpdateErr::UnsafePath(_))
        ));

        bad.files = vec![file("ok", 1)];
        bad.files[0].size = 2;
        assert!(matches!(
            bad.validate("app", Some("1")),
            Err(UpdateErr::RaggedChunks { .. })
        ));

        let mut bad = manifest(vec![file("ok", 1)]);
        bad.files[0].mode = 0o4755;
        assert!(matches!(
            bad.validate("app", Some("1")),
            Err(UpdateErr::InvalidFileMode { .. })
        ));
    }

    #[test]
    fn rejects_names_that_escape_the_repo() {
        for name in ["", ".", "..", "a/b", "a\\b"] {
            assert!(validate_name(name).is_err(), "{name:?}");
            assert!(validate_release(name).is_err(), "{name:?}");
        }
        for prefix in ["", "/", "a//b", "../updates", "a/../b"] {
            assert!(validate_prefix(prefix).is_err(), "{prefix:?}");
        }
    }

    #[test]
    fn parses_http_transport_schemes() {
        for (spec, scheme, protocol, path) in [
            (
                "https://example.com/root",
                "https",
                HttpProtocol::Auto,
                "/root/",
            ),
            ("http://example.com", "http", HttpProtocol::Auto, "/"),
            (
                "http2://example.com/root",
                "http",
                HttpProtocol::H2c,
                "/root/",
            ),
            ("h2c://example.com", "http", HttpProtocol::H2c, "/"),
            (
                "quic://example.com/root",
                "https",
                HttpProtocol::H3,
                "/root/",
            ),
            ("h3://example.com", "https", HttpProtocol::H3, "/"),
            (
                "https+quic://example.com/root",
                "https",
                HttpProtocol::H3,
                "/root/",
            ),
        ] {
            let store = HttpStore::parse(spec, None).unwrap().unwrap();
            assert_eq!(store.base.scheme(), scheme, "{spec}");
            assert_eq!(store.protocol, protocol, "{spec}");
            assert_eq!(store.base.path(), path, "{spec}");
        }
        assert!(HttpStore::parse("fs:/tmp/repo", None).unwrap().is_none());
        for bad in ["http://", "https://example.com/root?query=1", "quic://"] {
            assert!(HttpStore::parse(bad, None).is_err(), "{bad}");
        }
    }
}
