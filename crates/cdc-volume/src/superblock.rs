//! The volume superblock, stored encrypted at page 1.

use graft::core::PageIdx;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    err::{CdcErr, Result},
    layout::{FORMAT_VERSION, PAYLOAD_SIZE, Payload, SUPERBLOCK_PAGE},
};

pub const MAGIC: [u8; 8] = *b"GRAFTCDC";

/// Fixed-size superblock. It carries only bookkeeping: roots, the allocation
/// watermark and space accounting. KDF parameters are not stored, because the
/// superblock is encrypted and the salt comes from the public `VolumeId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C)]
pub struct Superblock {
    pub magic: [u8; 8],
    pub format_version: u16,
    pub page_bytes: u16,
    pub reserved0: [u8; 4],
    pub generation: u64,
    pub chunk_root: u32,
    pub path_root: u32,
    pub next_page: u32,
    pub free_head: u32,
    pub chunk_count: u64,
    pub file_count: u64,
    pub live_bytes: u64,
    pub reserved1: [u8; 8],
}

pub const SUPERBLOCK_LEN: usize = size_of::<Superblock>();

const _: () = assert!(SUPERBLOCK_LEN <= PAYLOAD_SIZE);

impl Default for Superblock {
    fn default() -> Self {
        Self {
            magic: MAGIC,
            format_version: FORMAT_VERSION,
            page_bytes: crate::layout::PAYLOAD_SIZE as u16,
            reserved0: [0; 4],
            generation: 0,
            chunk_root: 0,
            path_root: 0,
            next_page: crate::layout::FIRST_ALLOCATABLE,
            free_head: 0,
            chunk_count: 0,
            file_count: 0,
            live_bytes: 0,
            reserved1: [0; 8],
        }
    }
}

impl Superblock {
    /// Encode into a full frame payload, zero padded past the struct.
    pub fn encode(&self) -> Payload {
        let mut payload = [0u8; PAYLOAD_SIZE];
        let bytes = self.as_bytes();
        payload[..bytes.len()].copy_from_slice(bytes);
        payload
    }

    /// Decode and validate a frame payload read from page 1.
    pub fn decode(payload: &Payload) -> Result<Self> {
        let sb = Superblock::read_from_bytes(&payload[..SUPERBLOCK_LEN])
            .map_err(|_| CdcErr::format("superblock", "truncated"))?;

        if sb.magic != MAGIC {
            return Err(CdcErr::format("superblock", "bad magic"));
        }
        if sb.format_version != FORMAT_VERSION {
            return Err(CdcErr::format("superblock", "unsupported format version"));
        }
        if usize::from(sb.page_bytes) != PAYLOAD_SIZE {
            return Err(CdcErr::format("superblock", "bad payload size"));
        }
        Ok(sb)
    }

    /// Page index of the chunk tree root, if the tree exists.
    #[inline]
    pub fn chunk_root_idx(&self) -> Option<PageIdx> {
        PageIdx::try_new(self.chunk_root)
    }

    /// Page index of the path tree root, if the tree exists.
    #[inline]
    pub fn path_root_idx(&self) -> Option<PageIdx> {
        PageIdx::try_new(self.path_root)
    }

    #[inline]
    pub fn set_chunk_root(&mut self, root: Option<PageIdx>) {
        self.chunk_root = root.map_or(0, PageIdx::to_u32);
    }

    #[inline]
    pub fn set_path_root(&mut self, root: Option<PageIdx>) {
        self.path_root = root.map_or(0, PageIdx::to_u32);
    }

    /// The superblock always lives at `SUPERBLOCK_PAGE`.
    #[inline]
    pub const fn page() -> PageIdx {
        SUPERBLOCK_PAGE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut sb = Superblock { generation: 42, ..Default::default() };
        sb.set_chunk_root(PageIdx::try_new(9));
        let decoded = Superblock::decode(&sb.encode()).unwrap();
        assert_eq!(decoded, sb);
        assert_eq!(decoded.chunk_root_idx().unwrap().to_u32(), 9);
        assert!(decoded.path_root_idx().is_none());
    }

    #[test]
    fn rejects_zeroed_page() {
        let empty = [0u8; PAYLOAD_SIZE];
        assert!(Superblock::decode(&empty).is_err());
    }

    #[test]
    fn struct_fits_frame() {
        assert!(size_of::<Superblock>() < PAYLOAD_SIZE);
    }
}
