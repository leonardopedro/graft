//! Page allocator: bump watermark plus a sorted first-fit free list.
//!
//! Graft never reclaims pages inside a volume, so a freed page is simply one
//! below `next_page` that no live tree references. The free list is a chain of
//! ordinary encrypted frames: each free page's payload holds the next free page
//! index, and the chain is kept sorted ascending so a contiguous run of frames
//! (a chunk, or a node's pages) can be satisfiable from the list at all.
//!
//! The list is only consumed by later transactions: an allocator is loaded from
//! the superblock when a transaction opens and persisted when it commits, so
//! pages freed by a transaction are not handed back out by that same one.

use graft::core::PageIdx;

use crate::{
    err::{CdcErr, Result},
    io::{PageRead, PageWrite},
    layout::{FIRST_ALLOCATABLE, PAYLOAD_SIZE},
    superblock::Superblock,
};

const FREE_MAGIC: [u8; 4] = *b"GFR1";
const FREE_NEXT_OFFSET: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allocator {
    /// Pages `>= next_page` have never been written.
    next_page: u32,
    /// Head of the sorted free-page chain, 0 when empty.
    free_head: u32,
}

impl Allocator {
    pub fn load(sb: &Superblock) -> Self {
        Self {
            next_page: sb.next_page.max(FIRST_ALLOCATABLE),
            free_head: sb.free_head,
        }
    }

    pub fn store(&self, sb: &mut Superblock) {
        sb.next_page = self.next_page;
        sb.free_head = self.free_head;
    }

    /// First never-used page index.
    #[inline]
    pub fn next_page(&self) -> u32 {
        self.next_page
    }

    /// True when the next allocation appends, so `next_page` is where it lands.
    ///
    /// Callers that care about the *position* of a run (frame alignment) can
    /// only plan on it while the free chain is empty; a first-fit run comes from
    /// wherever a hole happens to be.
    #[inline]
    pub fn append_only(&self) -> bool {
        self.free_head == 0
    }

    /// Number of pages currently in the free chain. Diagnostics only.
    pub fn free_count(&self, io: &impl PageRead) -> Result<u32> {
        let mut count = 0;
        let mut cur = self.free_head;
        while let Some(idx) = PageIdx::try_new(cur) {
            count += 1;
            cur = read_next(io, idx)?;
            if count > u32::MAX / 2 {
                return Err(CdcErr::format("free list", "cycle detected"));
            }
        }
        Ok(count)
    }

    /// Allocate `count` consecutive pages.
    ///
    /// A contiguous run is required because a chunk frame run must be read back
    /// as consecutive pages. The search is first-fit over the sorted chain; a
    /// single page simply pops the head.
    pub fn alloc(&mut self, io: &mut impl PageWrite, count: u32) -> Result<PageIdx> {
        if count == 0 {
            return Err(CdcErr::format("allocator", "zero-sized allocation"));
        }

        if self.free_head != 0
            && let Some(start) = self.take_run(io, count)?
        {
            return Ok(start);
        }

        let start = self.next_page;
        let next = start.checked_add(count).ok_or(CdcErr::OutOfPages)?;
        if next > PageIdx::LAST.to_u32() {
            return Err(CdcErr::OutOfPages);
        }
        self.next_page = next;
        PageIdx::try_new(start).ok_or(CdcErr::OutOfPages)
    }

    /// Return `count` consecutive pages to the free chain.
    ///
    /// The run is pushed back to front so the chain stays sorted ascending.
    pub fn free(&mut self, io: &mut impl PageWrite, start: PageIdx, count: u32) -> Result<()> {
        if count == 0 {
            return Ok(());
        }

        let first = start.to_u32();
        let last = first
            .checked_add(count - 1)
            .ok_or(CdcErr::PageOutOfRange(first))?;
        if first < FIRST_ALLOCATABLE || last >= self.next_page.max(FIRST_ALLOCATABLE) {
            return Err(CdcErr::PageOutOfRange(first));
        }

        for offset in (0..count).rev() {
            // SAFETY: `first + offset <= last < next_page <= u32::MAX`.
            let idx =
                PageIdx::try_new(first + offset).ok_or(CdcErr::PageOutOfRange(first + offset))?;
            write_next(io, idx, self.free_head)?;
            self.free_head = idx.to_u32();
        }
        Ok(())
    }

    /// Pop a contiguous run of `count` pages from the chain, if one exists.
    fn take_run(&mut self, io: &mut impl PageWrite, count: u32) -> Result<Option<PageIdx>> {
        // SAFETY: free_head is non-zero (checked by the caller).
        let mut cur = PageIdx::try_new(self.free_head).expect("free_head is non-zero");
        let mut prev: Option<PageIdx> = None;

        let mut run_start = cur;
        let mut run_prev = prev;
        let mut run_len = 1u32;

        loop {
            if run_len == count {
                let next = read_next(io, cur)?;
                match run_prev {
                    None => self.free_head = next,
                    Some(before) => write_next(io, before, next)?,
                }
                return Ok(Some(run_start));
            }

            let next_raw = read_next(io, cur)?;
            let Some(next) = PageIdx::try_new(next_raw) else {
                return Ok(None);
            };

            prev = Some(cur);
            if next.to_u32() == cur.to_u32() + 1 {
                run_len += 1;
            } else {
                run_start = next;
                run_prev = prev;
                run_len = 1;
            }
            cur = next;
        }
    }
}

fn write_next(io: &mut impl PageWrite, idx: PageIdx, next: u32) -> Result<()> {
    let mut payload = [0u8; PAYLOAD_SIZE];
    payload[..4].copy_from_slice(&FREE_MAGIC);
    payload[FREE_NEXT_OFFSET..FREE_NEXT_OFFSET + 4].copy_from_slice(&next.to_le_bytes());
    io.write_payload(idx, &payload)
}

fn read_next(io: &impl PageRead, idx: PageIdx) -> Result<u32> {
    let payload = io.read_payload(idx)?;
    if payload[..4] != FREE_MAGIC {
        return Err(CdcErr::format("free list", "corrupt free page"));
    }
    Ok(u32::from_le_bytes(
        payload[FREE_NEXT_OFFSET..FREE_NEXT_OFFSET + 4]
            .try_into()
            .expect("slice is 4 bytes"),
    ))
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap};

    use graft::core::PageCount;

    use super::*;

    #[derive(Default)]
    struct MemIo {
        pages: RefCell<BTreeMap<u32, [u8; PAYLOAD_SIZE]>>,
        count: u32,
    }

    impl PageRead for MemIo {
        fn read_payload(&self, page_idx: PageIdx) -> Result<[u8; PAYLOAD_SIZE]> {
            Ok(self
                .pages
                .borrow()
                .get(&page_idx.to_u32())
                .copied()
                .unwrap_or([0u8; PAYLOAD_SIZE]))
        }

        fn page_count(&self) -> PageCount {
            PageCount::new(self.count)
        }
    }

    impl PageWrite for MemIo {
        fn write_payload(&mut self, page_idx: PageIdx, payload: &[u8; PAYLOAD_SIZE]) -> Result<()> {
            self.pages.borrow_mut().insert(page_idx.to_u32(), *payload);
            self.count = self.count.max(page_idx.to_u32());
            Ok(())
        }
    }

    #[test]
    fn bump_from_first_allocatable() {
        let mut io = MemIo::default();
        let mut a = Allocator::load(&Superblock::default());
        assert_eq!(a.alloc(&mut io, 1).unwrap().to_u32(), 2);
        assert_eq!(a.alloc(&mut io, 3).unwrap().to_u32(), 3);
        assert_eq!(a.next_page(), 6);
    }

    #[test]
    fn free_list_is_sorted_and_reused_contiguously() {
        let mut io = MemIo::default();
        let mut a = Allocator::load(&Superblock::default());
        let run = a.alloc(&mut io, 10).unwrap();
        assert_eq!(run.to_u32(), 2);
        assert_eq!(a.next_page(), 12);

        a.free(&mut io, PageIdx::try_new(4).unwrap(), 3).unwrap();
        assert_eq!(a.free_head, 4);
        assert_eq!(a.free_count(&io).unwrap(), 3);

        // A single page pops the sorted head.
        assert_eq!(a.alloc(&mut io, 1).unwrap().to_u32(), 4);
        // The remaining pair is not contiguous with anything else.
        assert_eq!(a.alloc(&mut io, 3).unwrap().to_u32(), 12);
        assert_eq!(a.alloc(&mut io, 2).unwrap().to_u32(), 5);
    }

    #[test]
    fn free_merges_nothing_but_stays_ordered() {
        let mut io = MemIo::default();
        let mut a = Allocator::load(&Superblock::default());
        a.alloc(&mut io, 8).unwrap();
        a.free(&mut io, PageIdx::try_new(5).unwrap(), 2).unwrap();
        a.free(&mut io, PageIdx::try_new(2).unwrap(), 1).unwrap();
        assert_eq!(a.free_head, 2);
        assert_eq!(a.alloc(&mut io, 3).unwrap().to_u32(), 10);
        assert_eq!(a.alloc(&mut io, 1).unwrap().to_u32(), 2);
    }

    #[test]
    fn rejects_freeing_below_first_or_above_watermark() {
        let mut io = MemIo::default();
        let mut a = Allocator::load(&Superblock::default());
        a.alloc(&mut io, 2).unwrap();
        assert!(a.free(&mut io, PageIdx::FIRST, 1).is_err());
        assert!(a.free(&mut io, PageIdx::try_new(4).unwrap(), 1).is_err());
    }
}
