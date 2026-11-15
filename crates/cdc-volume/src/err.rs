//! Error type for the CDC volume engine.

use graft::{GraftErr, LogicalErr};

pub type Result<T> = std::result::Result<T, CdcErr>;

#[derive(Debug, thiserror::Error)]
pub enum CdcErr {
    #[error(transparent)]
    Graft(#[from] GraftErr),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("frame failed authentication at page {0}")]
    Auth(u32),

    #[error("malformed {what}: {detail}")]
    Format {
        what: &'static str,
        detail: &'static str,
    },

    #[error("volume has no superblock at page {0}")]
    NotInitialized(u32),

    #[error("volume is already initialized")]
    AlreadyInitialized,

    #[error("frame size must be 4096 bytes, got {0}")]
    WrongFrameSize(usize),

    #[error("page {0} is outside the volume")]
    PageOutOfRange(u32),

    #[error("allocator exhausted")]
    OutOfPages,

    #[error("key too large for a node entry")]
    EntryTooLarge,

    #[error("write conflict, replay limit reached")]
    Conflict,

    #[error("no such file: {0}")]
    NotFound(String),

    #[error("page {0} is not an index node of this tree")]
    NotATreePage(u32),

    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),
}

impl CdcErr {
    pub fn format(what: &'static str, detail: &'static str) -> Self {
        CdcErr::Format { what, detail }
    }

    pub fn is_conflict(&self) -> bool {
        matches!(
            self,
            CdcErr::Graft(GraftErr::Logical(LogicalErr::VolumeConcurrentWrite(_)))
        )
    }
}
