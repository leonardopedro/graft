//! Volume handle and transaction loop.
//!
//! A `VolumeWriter` is bound to a snapshot, so a conflict is not fixable by
//! retrying `commit()`. Instead the whole logical mutation is replayed against a
//! fresh writer and a newer snapshot.

use graft::{
    core::{PageCount, PageIdx, VolumeId},
    rt::runtime::Runtime,
    volume_reader::{VolumeRead, VolumeReader},
    volume_writer::{VolumeWrite, VolumeWriter},
};

use crate::{
    alloc::Allocator,
    crypto::{MASTER_KEY_LEN, VolumeKeys},
    err::{CdcErr, Result},
    io::{FrameIo, PageRead, PageWrite},
    layout::SUPERBLOCK_PAGE,
    superblock::Superblock,
};

/// How many times a mutation is replayed before giving up under contention.
pub const MAX_REPLAYS: usize = 64;

/// Handle to one encrypted volume.
#[derive(Clone)]
pub struct CdcVolume {
    rt: Runtime,
    vid: VolumeId,
    keys: VolumeKeys,
}

impl CdcVolume {
    /// Open a volume, writing a fresh superblock if the volume has no pages.
    pub fn init(rt: Runtime, vid: VolumeId, master_key: &[u8; MASTER_KEY_LEN]) -> Result<Self> {
        let keys = VolumeKeys::derive(master_key, &vid);
        let volume = Self { rt, vid, keys };

        let writer = volume.rt.volume_writer(volume.vid.clone())?;
        if writer.page_count() == PageCount::ZERO {
            let mut io = FrameIo::new(writer, volume.keys.page_cipher());
            io.write_payload(SUPERBLOCK_PAGE, &Superblock::default().encode())?;
            io.into_inner().commit()?;
        } else {
            // An existing volume must already be readable with this key.
            volume.superblock()?;
        }
        Ok(volume)
    }

    /// Open an initialized volume and verify it decrypts with this key.
    pub fn open(rt: Runtime, vid: VolumeId, master_key: &[u8; MASTER_KEY_LEN]) -> Result<Self> {
        let keys = VolumeKeys::derive(master_key, &vid);
        let volume = Self { rt, vid, keys };
        volume.superblock()?;
        Ok(volume)
    }

    #[inline]
    pub fn id(&self) -> &VolumeId {
        &self.vid
    }

    #[inline]
    pub fn keys(&self) -> &VolumeKeys {
        &self.keys
    }

    /// Read the superblock from page 1 through a fresh snapshot.
    pub fn superblock(&self) -> Result<Superblock> {
        self.reader()?.read_superblock()
    }

    /// A read-only view, pinned to the snapshot taken at this call.
    pub fn reader(&self) -> Result<FrameIo<VolumeReader>> {
        let reader = self.rt.volume_reader(self.vid.clone())?;
        Ok(FrameIo::new(reader, self.keys.page_cipher()))
    }

    /// Run `mutation` in a transaction, replaying it from scratch on conflict.
    ///
    /// `mutation` must be a pure function of its input and of what it reads from
    /// the transaction: it is executed again against a newer snapshot whenever
    /// the writer it was given is stale.
    pub fn update<F, T>(&self, mut mutation: F) -> Result<T>
    where
        F: FnMut(&mut Txn) -> Result<T>,
    {
        for _ in 0..MAX_REPLAYS {
            let writer = self.rt.volume_writer(self.vid.clone())?;
            let mut txn = Txn::open(writer, &self.keys)?;
            let out = mutation(&mut txn)?;
            match txn.commit() {
                Ok(()) => return Ok(out),
                Err(err) if err.is_conflict() => {
                    tracing::debug!(%err, "cdc volume write conflict, replaying mutation");
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        Err(CdcErr::Conflict)
    }
}

impl std::fmt::Debug for CdcVolume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdcVolume")
            .field("vid", &self.vid)
            .finish_non_exhaustive()
    }
}

/// One write transaction.
pub struct Txn {
    io: FrameIo<VolumeWriter>,
    alloc: Allocator,
    sb: Superblock,
}

impl Txn {
    fn open(writer: VolumeWriter, keys: &VolumeKeys) -> Result<Self> {
        let io = FrameIo::new(writer, keys.page_cipher());
        if !io.page_count().contains(SUPERBLOCK_PAGE) {
            return Err(CdcErr::NotInitialized(SUPERBLOCK_PAGE.to_u32()));
        }
        let sb = Superblock::decode(&io.read_payload(SUPERBLOCK_PAGE)?)?;
        let alloc = Allocator::load(&sb);
        Ok(Self { io, alloc, sb })
    }

    #[inline]
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    #[inline]
    pub fn generation(&self) -> u64 {
        self.sb.generation
    }

    #[inline]
    pub fn chunk_root(&self) -> Option<PageIdx> {
        self.sb.chunk_root_idx()
    }

    #[inline]
    pub fn set_chunk_root(&mut self, root: Option<PageIdx>) {
        self.sb.set_chunk_root(root);
    }

    #[inline]
    pub fn path_root(&self) -> Option<PageIdx> {
        self.sb.path_root_idx()
    }

    #[inline]
    pub fn set_path_root(&mut self, root: Option<PageIdx>) {
        self.sb.set_path_root(root);
    }

    /// Replace the superblock's space accounting.
    ///
    /// The store does not maintain these fields on every write (see the plan's
    /// risks); GC is what makes them exact again, so it sets them explicitly.
    #[inline]
    pub fn set_accounting(&mut self, chunks: u64, files: u64, live_bytes: u64) {
        self.sb.chunk_count = chunks;
        self.sb.file_count = files;
        self.sb.live_bytes = live_bytes;
    }

    /// Allocate `count` consecutive pages.
    pub fn alloc(&mut self, count: u32) -> Result<PageIdx> {
        self.alloc.alloc(&mut self.io, count)
    }

    /// Return `count` consecutive pages to the free list.
    pub fn free(&mut self, start: PageIdx, count: u32) -> Result<()> {
        self.alloc.free(&mut self.io, start, count)
    }

    /// The frame IO and allocator, for layers such as the B-tree that need both.
    pub fn parts(&mut self) -> (&mut FrameIo<VolumeWriter>, &mut Allocator) {
        (&mut self.io, &mut self.alloc)
    }

    /// Stage the superblock and commit the transaction.
    fn commit(mut self) -> Result<()> {
        let mut sb = self.sb;
        self.alloc.store(&mut sb);
        sb.generation = sb
            .generation
            .checked_add(1)
            .ok_or(CdcErr::format("superblock", "generation overflow"))?;

        self.io.write_payload(SUPERBLOCK_PAGE, &sb.encode())?;
        frame_write_commit(self.io)?;
        Ok(())
    }
}

/// Commit the staged writer inside a `FrameIo`.
fn frame_write_commit(io: FrameIo<VolumeWriter>) -> Result<()> {
    let writer = io.into_inner();
    let _reader = writer.commit()?;
    Ok(())
}
