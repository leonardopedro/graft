//! Encrypted, deduplicating content-addressed store built directly on Graft
//! pages.
//!
//! Layout: page 1 is the encrypted superblock, every other page is either a
//! chunk frame, an index node, or a manifest/recipe page. Each page is one
//! 4096-byte XChaCha20-Poly1305 frame carrying 4056 bytes of plaintext, bound
//! to its page index by the AEAD associated data.
//!
//! This crate implements Phases 1–5 of `CDC_STORE_PLAN.md`: page framing,
//! superblock, allocator, the copy-on-write B+ tree with a replay loop for write
//! conflicts, the chunked file store on top of it, and mark-and-sweep GC that
//! rewrites the live set into a fresh volume.

pub mod alloc;
pub mod btree;
pub mod chunker;
pub mod crypto;
pub mod err;
pub mod gc;
pub mod io;
pub mod layout;
pub mod store;
pub mod superblock;
pub mod volume;

pub use btree::{BTree, ChunkCodec, ChunkKey, ChunkMeta, Codec, PathCodec};
pub use crypto::{MASTER_KEY_LEN, PageCipher, VolumeKeys};
pub use err::{CdcErr, Result};
pub use gc::{GcStats, VolumeStats};
pub use io::{FrameIo, PageRead, PageWrite};
pub use layout::{PAYLOAD_SIZE, Payload};
pub use store::{ChunkRef, FileStat, Snapshot, Store};
pub use superblock::Superblock;
pub use volume::{CdcVolume, Txn};
