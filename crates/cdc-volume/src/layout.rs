//! On-volume constants shared by the frame, superblock, allocator and B-tree.

use graft::core::PageIdx;

/// Every page stored in a volume is one AEAD frame of exactly this many bytes.
pub const FRAME_SIZE: usize = 4096;

/// XChaCha20-Poly1305 nonce length.
pub const NONCE_LEN: usize = 24;

/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;

/// Bytes of plaintext carried by one frame.
pub const PAYLOAD_SIZE: usize = FRAME_SIZE - NONCE_LEN - TAG_LEN;

/// The superblock lives in the first page, because Graft has no page 0.
pub const SUPERBLOCK_PAGE: PageIdx = PageIdx::FIRST;

/// First page handed out by the allocator (page 1 is the superblock).
pub const FIRST_ALLOCATABLE: u32 = 2;

/// AAD domain separator, binding a frame to the format and its page index.
pub const AAD_TAG: &[u8; 8] = b"GRAFTCDC";

/// Wire format version. KDF parameters and frame layout are fixed per version.
pub const FORMAT_VERSION: u16 = 1;

/// Plaintext payload carried by a single frame.
pub type Payload = [u8; PAYLOAD_SIZE];

const _: () = assert!(PAYLOAD_SIZE == 4056);
const _: () = assert!(FRAME_SIZE == 4096);

/// Number of frames needed to hold `raw_len` bytes of chunk data.
#[inline]
pub const fn frames_for(raw_len: u64) -> u32 {
    let per = PAYLOAD_SIZE as u64;
    raw_len.div_ceil(per) as u32
}
