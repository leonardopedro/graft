//! Content-defined chunking.
//!
//! Boundaries come from `FastCDC` v2020 with the parameters fixed by the plan:
//! 8 KiB minimum, 32 KiB target, 128 KiB maximum. Content-defined boundaries
//! mean an edit near the start of a file does not shift every later boundary, so
//! an edited upload reuses all but a handful of the original chunks.

use std::io::Read;

use crate::err::{CdcErr, Result};

/// Smallest chunk `FastCDC` may emit.
pub const MIN_CHUNK: u32 = 8 * 1024;
/// Target chunk size; actual sizes vary around this.
pub const AVG_CHUNK: u32 = 32 * 1024;
/// Largest chunk `FastCDC` may emit. Also the largest `raw_len` a chunk can have.
pub const MAX_CHUNK: u32 = 128 * 1024;

/// Stream chunks out of `reader` in order.
///
/// Each item is the chunk's plaintext, owned, so a caller can stage it into a
/// transaction without holding the reader open.
pub fn chunk_stream<R: Read>(reader: R) -> impl Iterator<Item = Result<Vec<u8>>> {
    fastcdc::v2020::StreamCDC::new(reader, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK).map(|item| {
        item.map(|chunk| chunk.data)
            .map_err(|err| CdcErr::Io(std::io::Error::from(err)))
    })
}
