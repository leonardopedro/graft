//! Encrypted payload IO over Graft pages.
//!
//! Everything above this layer sees 4056-byte plaintext payloads; this layer
//! owns the 4 KiB AEAD frame and counts frame reads so read amplification can be
//! measured directly.

use std::sync::atomic::{AtomicU64, Ordering};

use graft::{
    core::{PageCount, PageIdx},
    volume_reader::VolumeRead,
    volume_writer::VolumeWrite,
};

use crate::{
    crypto::PageCipher,
    err::Result,
    layout::{Payload, SUPERBLOCK_PAGE},
};

/// Read access to plaintext payloads.
pub trait PageRead {
    fn read_payload(&self, page_idx: PageIdx) -> Result<Payload>;
    fn page_count(&self) -> PageCount;
}

/// Write access to plaintext payloads in a staged transaction.
pub trait PageWrite: PageRead {
    fn write_payload(&mut self, page_idx: PageIdx, payload: &Payload) -> Result<()>;
}

/// A cipher plus a Graft reader or writer.
pub struct FrameIo<R> {
    inner: R,
    cipher: PageCipher,
    reads: AtomicU64,
    writes: AtomicU64,
}

impl<R> FrameIo<R> {
    pub fn new(inner: R, cipher: PageCipher) -> Self {
        Self {
            inner,
            cipher,
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn inner(&self) -> &R {
        &self.inner
    }

    #[inline]
    pub fn cipher(&self) -> &PageCipher {
        &self.cipher
    }

    /// Number of frame reads served since construction or the last reset.
    #[inline]
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn reset_counters(&self) {
        self.reads.store(0, Ordering::Relaxed);
        self.writes.store(0, Ordering::Relaxed);
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: VolumeRead> PageRead for FrameIo<R> {
    fn read_payload(&self, page_idx: PageIdx) -> Result<Payload> {
        let raw = self.inner.read_page(page_idx)?;
        self.reads.fetch_add(1, Ordering::Relaxed);

        // Pages past the logical end read back as the empty page; treat that as
        // "outside the volume" so a missing child pointer cannot authenticate.
        if !self.inner.page_count().contains(page_idx) {
            return Err(crate::err::CdcErr::PageOutOfRange(page_idx.to_u32()));
        }

        self.cipher.open(page_idx, &raw)
    }

    fn page_count(&self) -> PageCount {
        self.inner.page_count()
    }
}

impl<R: VolumeRead> FrameIo<R> {
    /// Read and validate the volume superblock from page 1.
    pub fn read_superblock(&self) -> Result<crate::superblock::Superblock> {
        crate::superblock::Superblock::decode(&self.read_payload(SUPERBLOCK_PAGE)?)
    }
}

impl<R: VolumeRead + VolumeWrite> PageWrite for FrameIo<R> {
    fn write_payload(&mut self, page_idx: PageIdx, payload: &Payload) -> Result<()> {
        let frame = self.cipher.seal(page_idx, payload);
        self.inner.write_page(page_idx, frame)?;
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Helpers shared by higher layers.
pub fn superblock_page() -> PageIdx {
    SUPERBLOCK_PAGE
}
