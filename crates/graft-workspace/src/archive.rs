//! The archive lane: chunked, deduplicating, encrypted blobs in their own
//! Graft volume.
//!
//! `cdc-volume` owns the page format, chunking and per-page AEAD; this module
//! only binds a volume to the workspace catalog. The key lives in a file named
//! by `[archive] key_file`, never in `workspace.toml` or the catalog.

use std::{
    fs,
    io::{self, Read},
    path::Path,
};

use blake3::Hasher as Blake3;

use cdc_volume::{
    FileStat, MASTER_KEY_LEN, Store,
    gc::{self, GcStats},
};
use graft::{core::VolumeId, rt::runtime::Runtime};

use crate::{config::ArchiveConfig, media::BlobErr};

pub struct ArchiveStore {
    store: Store,
    volume: VolumeId,
}

impl ArchiveStore {
    /// Open the archive volume named by `tag`, creating it on first use.
    pub fn open(
        runtime: Runtime,
        tag: &str,
        config: &ArchiveConfig,
    ) -> Result<ArchiveStore, BlobErr> {
        let key = read_key(&config.key_file)?;
        let volume = match runtime
            .tag_get(tag)
            .map_err(|err| BlobErr::Archive(err.to_string()))?
        {
            Some(volume) => {
                runtime
                    .volume_open(Some(volume.clone()), None, None)
                    .map_err(|err| BlobErr::Archive(err.to_string()))?;
                volume
            }
            None => {
                let volume = runtime
                    .volume_open(None, None, None)
                    .map_err(|err| BlobErr::Archive(err.to_string()))?;
                runtime
                    .tag_replace(tag, volume.vid.clone())
                    .map_err(|err| BlobErr::Archive(err.to_string()))?;
                volume.vid
            }
        };

        let store = Store::create(runtime, volume.clone(), &key)?;
        Ok(ArchiveStore { store, volume })
    }

    pub fn volume_id(&self) -> VolumeId {
        self.volume.clone()
    }

    pub fn put<R: std::io::Read>(&self, path: &str, input: R) -> Result<FileStat, BlobErr> {
        Ok(self.store.put_file(path.as_bytes(), input)?)
    }

    pub fn stat(&self, path: &str) -> Result<Option<FileStat>, BlobErr> {
        Ok(self.store.stat(path.as_bytes())?)
    }

    pub fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, BlobErr> {
        Ok(self.store.read_range(path.as_bytes(), offset, len)?)
    }

    pub fn delete(&self, path: &str) -> Result<bool, BlobErr> {
        Ok(self.store.delete_file(path.as_bytes())?)
    }

    /// Copy one file into another archive, decrypting and re-encrypting it, and
    /// verify the plaintext size and blake3 hash. A mismatch removes the target
    /// manifest so an incomplete backup object is never left behind.
    pub fn copy_to_verified(
        &self,
        path: &str,
        target: &ArchiveStore,
        target_path: &str,
        expected_size: u64,
        expected_hash: &str,
    ) -> Result<FileStat, BlobErr> {
        let stat = self
            .stat(path)?
            .ok_or_else(|| BlobErr::NotFound(path.to_string()))?;
        if stat.size != expected_size {
            return Err(BlobErr::SizeMismatch {
                expected: expected_size,
                actual: stat.size,
            });
        }

        let reader = ArchiveReader {
            store: self,
            path: path.to_string(),
            offset: 0,
            end: stat.size,
        };
        let mut hashing = HashingReader::new(reader);
        let out = target.put(target_path, &mut hashing)?;
        let (size, hash) = hashing.finish();
        if out.size != expected_size || size != expected_size || hash != expected_hash {
            let _ = target.delete(target_path);
            return Err(BlobErr::HashMismatch {
                expected: expected_hash.to_string(),
                actual: hash,
            });
        }
        Ok(out)
    }

    /// Paths in this volume that start with `prefix`.
    pub fn list(&self, prefix: &str) -> Result<Vec<String>, BlobErr> {
        let mut paths = Vec::new();
        for file in self.store.ls(prefix.as_bytes())? {
            let path = String::from_utf8(file.path)
                .map_err(|err| BlobErr::Archive(format!("non-UTF-8 path in archive: {err}")))?;
            paths.push(path);
        }
        Ok(paths)
    }

    /// Rewrite this volume into a fresh one, dropping every page that is no
    /// longer reachable, then point `tag` at the copy.
    ///
    /// The copy re-keys chunk ids, so it also serves as a key rotation path.
    /// The old volume is left in place: callers can keep it for a grace period
    /// and remove it once a clone or sync from the new volume succeeds.
    pub fn gc(
        &self,
        runtime: &Runtime,
        tag: &str,
        config: &ArchiveConfig,
    ) -> Result<(GcStats, ArchiveStore), BlobErr> {
        let key = read_key(&config.key_file)?;
        let volume = runtime
            .volume_open(None, None, None)
            .map_err(|err| BlobErr::Archive(err.to_string()))?;
        let target = Store::create(runtime.clone(), volume.vid.clone(), &key)?;
        let stats = gc::rewrite(&self.store, &target)?;
        runtime
            .tag_replace(tag, volume.vid.clone())
            .map_err(|err| BlobErr::Archive(err.to_string()))?;
        Ok((stats, ArchiveStore { store: target, volume: volume.vid }))
    }
}

struct ArchiveReader<'a> {
    store: &'a ArchiveStore,
    path: String,
    offset: u64,
    end: u64,
}

impl Read for ArchiveReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.offset >= self.end || buf.is_empty() {
            return Ok(0);
        }
        let len = (self.end - self.offset).min(buf.len() as u64);
        let data = self
            .store
            .read_range(&self.path, self.offset, len)
            .map_err(io::Error::other)?;
        let read = data.len().min(buf.len());
        buf[..read].copy_from_slice(&data[..read]);
        self.offset += read as u64;
        Ok(read)
    }
}

struct HashingReader<R> {
    inner: R,
    blake: Blake3,
    size: u64,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        HashingReader { inner, blake: Blake3::new(), size: 0 }
    }

    fn finish(self) -> (u64, String) {
        (
            self.size,
            format!("blake3:{}", self.blake.finalize().to_hex()),
        )
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.blake.update(&buf[..read]);
        self.size += read as u64;
        Ok(read)
    }
}

/// Read a 32-byte key from a file containing exactly 64 hex characters.
fn read_key(path: &Path) -> Result<[u8; MASTER_KEY_LEN], BlobErr> {
    let text = fs::read_to_string(path).map_err(|err| BlobErr::ArchiveKey {
        path: path.display().to_string(),
        reason: err.to_string(),
    })?;
    let text = text.trim();
    if text.len() != MASTER_KEY_LEN * 2 {
        return Err(BlobErr::ArchiveKey {
            path: path.display().to_string(),
            reason: format!(
                "expected {} hex characters, found {}",
                MASTER_KEY_LEN * 2,
                text.len()
            ),
        });
    }

    let mut key = [0u8; MASTER_KEY_LEN];
    for (index, slot) in key.iter_mut().enumerate() {
        let pair = &text[index * 2..index * 2 + 2];
        *slot = u8::from_str_radix(pair, 16).map_err(|_| BlobErr::ArchiveKey {
            path: path.display().to_string(),
            reason: format!("{pair:?} is not hex"),
        })?;
    }
    Ok(key)
}
