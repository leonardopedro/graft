//! Key hierarchy and per-page AEAD framing.
//!
//! ```text
//! master_key (32B)
//!   └─ HKDF-SHA256(salt = VolumeId bytes, info = "graft-cdc v1")
//!        ├─ chunk_id_key (BLAKE3 keyed hashing of plaintext chunks)
//!        └─ page_key     (XChaCha20-Poly1305, one nonce per page write)
//! ```
//!
//! The salt is the public `VolumeId`, so a cold reader can derive the page key
//! before it can decrypt the superblock. KDF parameters are fixed by the format
//! version rather than stored on the volume.

use chacha20poly1305::{
    KeyInit, Tag, XChaCha20Poly1305, XNonce,
    aead::{AeadInPlace, generic_array::GenericArray},
};
use graft::core::{PageIdx, VolumeId, page::Page};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::{
    err::{CdcErr, Result},
    layout::{AAD_TAG, FORMAT_VERSION, FRAME_SIZE, NONCE_LEN, PAYLOAD_SIZE, Payload, TAG_LEN},
};

pub const MASTER_KEY_LEN: usize = 32;
const KDF_INFO: &[u8] = b"graft-cdc v1";
const OKM_LEN: usize = 2 * 32;

/// Derived per-volume keys.
#[derive(Clone)]
pub struct VolumeKeys {
    chunk_id_key: [u8; 32],
    page_key: [u8; 32],
}

impl VolumeKeys {
    /// Derive the volume keys from the master key and the public volume id.
    pub fn derive(master_key: &[u8; MASTER_KEY_LEN], vid: &VolumeId) -> Self {
        // `vid.as_ref()` is the 16-byte GID; it is public, so using it as salt
        // does not leak anything and removes the superblock bootstrapping cycle.
        let hkdf = Hkdf::<Sha256>::new(Some(vid.as_ref()), master_key);
        let mut okm = [0u8; OKM_LEN];
        hkdf.expand(KDF_INFO, &mut okm)
            .expect("32*2 bytes is a valid HKDF-SHA256 output length");

        let mut chunk_id_key = [0u8; 32];
        let mut page_key = [0u8; 32];
        chunk_id_key.copy_from_slice(&okm[..32]);
        page_key.copy_from_slice(&okm[32..]);

        Self { chunk_id_key, page_key }
    }

    /// Key for deterministic, keyed chunk identities.
    #[inline]
    pub fn chunk_id(&self, plaintext: &[u8]) -> [u8; 32] {
        *blake3::keyed_hash(&self.chunk_id_key, plaintext).as_bytes()
    }

    /// Cipher for page frames.
    #[inline]
    pub fn page_cipher(&self) -> PageCipher {
        PageCipher::new(&self.page_key)
    }
}

impl std::fmt::Debug for VolumeKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VolumeKeys").finish_non_exhaustive()
    }
}

/// Encrypts and authenticates 4 KiB frames.
pub struct PageCipher {
    cipher: XChaCha20Poly1305,
}

impl PageCipher {
    pub fn new(page_key: &[u8; 32]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new(GenericArray::from_slice(page_key)),
        }
    }

    /// AAD binds a frame to the format and to its slot in the volume, so a
    /// frame cannot be moved to another page index.
    #[inline]
    fn aad(page_idx: PageIdx) -> [u8; 12] {
        let mut aad = [0u8; 12];
        aad[..8].copy_from_slice(AAD_TAG);
        aad[8..].copy_from_slice(&page_idx.to_u32().to_be_bytes());
        aad
    }

    /// Encrypt a payload into a fresh 4096-byte frame with a random nonce.
    /// Pages are write-once (copy-on-write), so random 192-bit nonces are safe.
    pub fn seal(&self, page_idx: PageIdx, payload: &Payload) -> Page {
        let nonce: [u8; NONCE_LEN] = rand::random();
        self.seal_with_nonce(page_idx, payload, &nonce)
    }

    /// Deterministic variant used by tests and fault injection.
    pub fn seal_with_nonce(
        &self,
        page_idx: PageIdx,
        payload: &Payload,
        nonce: &[u8; NONCE_LEN],
    ) -> Page {
        let mut frame = [0u8; FRAME_SIZE];
        frame[..NONCE_LEN].copy_from_slice(nonce);
        frame[NONCE_LEN..NONCE_LEN + PAYLOAD_SIZE].copy_from_slice(payload);

        let tag: Tag = self
            .cipher
            .encrypt_in_place_detached(
                XNonce::from_slice(nonce),
                &Self::aad(page_idx),
                &mut frame[NONCE_LEN..NONCE_LEN + PAYLOAD_SIZE],
            )
            .expect("XChaCha20-Poly1305 encryption cannot fail");

        frame[FRAME_SIZE - TAG_LEN..].copy_from_slice(&tag);
        // SAFETY: `frame` is exactly FRAME_SIZE bytes.
        Page::try_from(frame.as_slice()).expect("frame length is FRAME_SIZE")
    }

    /// Decrypt and authenticate a frame read from `page_idx`.
    pub fn open(&self, page_idx: PageIdx, page: &Page) -> Result<Payload> {
        let frame = page.as_ref();
        if frame.len() != FRAME_SIZE {
            return Err(CdcErr::WrongFrameSize(frame.len()));
        }

        let mut body = frame[NONCE_LEN..NONCE_LEN + PAYLOAD_SIZE].to_vec();
        let tag = Tag::from_slice(&frame[FRAME_SIZE - TAG_LEN..]);
        self.cipher
            .decrypt_in_place_detached(
                XNonce::from_slice(&frame[..NONCE_LEN]),
                &Self::aad(page_idx),
                &mut body,
                tag,
            )
            .map_err(|_| CdcErr::Auth(page_idx.to_u32()))?;

        let mut payload = [0u8; PAYLOAD_SIZE];
        payload.copy_from_slice(&body);
        Ok(payload)
    }
}

impl std::fmt::Debug for PageCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageCipher").finish_non_exhaustive()
    }
}

/// Format version this cipher implementation belongs to.
pub const VERSION: u16 = FORMAT_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> VolumeKeys {
        VolumeKeys::derive(&[7u8; 32], &VolumeId::random())
    }

    #[test]
    fn seal_roundtrip() {
        let cipher = keys().page_cipher();
        let mut payload = [0u8; PAYLOAD_SIZE];
        payload[..5].copy_from_slice(b"hello");
        let frame = cipher.seal(PageIdx::FIRST, &payload);
        assert_eq!(cipher.open(PageIdx::FIRST, &frame).unwrap(), payload);
    }

    #[test]
    fn rejects_bit_flip() {
        let cipher = keys().page_cipher();
        let payload = [9u8; PAYLOAD_SIZE];
        let frame = cipher.seal(PageIdx::FIRST, &payload);
        let mut bytes = frame.as_ref().to_vec();
        bytes[NONCE_LEN + 100] ^= 0x01;
        let tampered = Page::try_from(bytes.as_slice()).unwrap();
        assert!(matches!(
            cipher.open(PageIdx::FIRST, &tampered),
            Err(CdcErr::Auth(_))
        ));
    }

    #[test]
    fn rejects_relocation() {
        let cipher = keys().page_cipher();
        let payload = [1u8; PAYLOAD_SIZE];
        let frame = cipher.seal(PageIdx::FIRST, &payload);
        let elsewhere = PageIdx::try_new(2).unwrap();
        assert!(matches!(
            cipher.open(elsewhere, &frame),
            Err(CdcErr::Auth(_))
        ));
    }

    #[test]
    fn keys_depend_on_volume_and_master() {
        let master = [3u8; 32];
        let a = VolumeId::random();
        let b = VolumeId::random();
        let ka = VolumeKeys::derive(&master, &a);
        let kb = VolumeKeys::derive(&master, &b);
        assert_ne!(ka.chunk_id(b"chunk"), kb.chunk_id(b"chunk"));
        assert_eq!(
            ka.chunk_id(b"chunk"),
            VolumeKeys::derive(&master, &a).chunk_id(b"chunk")
        );
    }
}
