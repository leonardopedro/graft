//! Space reclamation by mark-and-sweep rewrite.
//!
//! Graft never reclaims pages inside a volume, so deleted files and interrupted
//! uploads leave unreachable chunk frames and index entries behind forever. GC
//! marks what is reachable from the superblock — path tree nodes and manifests,
//! their recipes, chunk tree nodes, and the frames of chunks some manifest
//! references — then copies only that into a fresh volume.
//!
//! The copy is also a re-keying: chunk ids are keyed by the volume's derived
//! keys, so the fresh volume gets new ids for the same content and every
//! manifest reference is rewritten with them. That keeps deduplication working
//! across the swap, and makes GC the path a key rotation would take.
//!
//! The caller swaps volumes (for the CLI, an atomic rename of the state file)
//! and keeps the old volume around for a grace period.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, btree_map::Entry};

use graft::core::PageIdx;

use crate::{
    btree::{BTree, ChunkKey, ChunkMeta, PathCodec},
    err::{CdcErr, Result},
    layout::{FIRST_ALLOCATABLE, SUPERBLOCK_PAGE},
    store::{
        BATCH_BYTES, ChunkRef, INLINE_REFS, Manifest, Recipe, Snapshot, Store, refs_pages,
        write_recipe,
    },
};

/// What one volume looks like to GC right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VolumeStats {
    /// Pages the volume has written, allocated or free.
    pub pages: u64,
    /// Pages reachable from the superblock.
    pub live_pages: u64,
    /// `pages - live_pages`, i.e. free-list pages plus garbage.
    pub dead_pages: u64,
    /// Live manifests.
    pub files: u64,
    /// Chunks some manifest references.
    pub chunks: u64,
    /// Total `raw_len` of those chunks, i.e. deduplicated file bytes.
    pub live_bytes: u64,
    /// Frames held by chunks no manifest references.
    pub dead_chunk_frames: u64,
}

/// Result of one rewrite.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    pub before: VolumeStats,
    pub after: VolumeStats,
    pub copied_chunks: u64,
    pub copied_bytes: u64,
}

impl GcStats {
    /// Pages the rewrite removed from the volume.
    pub fn pages_reclaimed(&self) -> u64 {
        self.before.pages.saturating_sub(self.after.pages)
    }
}

#[derive(Default)]
struct LiveIndex {
    pages: BTreeSet<u32>,
    /// Referenced chunk hash to its recorded length.
    chunks: BTreeMap<[u8; 32], u32>,
    files: u64,
    live_bytes: u64,
    dead_chunk_frames: u64,
}

impl LiveIndex {
    fn add_manifest(&mut self, snapshot: &Snapshot, manifest: &Manifest) -> Result<()> {
        self.files += 1;
        if let Recipe::Pages { start, pages, .. } = &manifest.recipe {
            for offset in 0..*pages {
                let page = start
                    .checked_add(offset)
                    .filter(|page| PageIdx::try_new(*page).is_some())
                    .ok_or(CdcErr::PageOutOfRange(*start))?;
                self.pages.insert(page);
            }
        }
        for entry in snapshot.read_recipe(&manifest.recipe)? {
            match self.chunks.entry(entry.hash) {
                Entry::Vacant(slot) => {
                    slot.insert(entry.raw_len);
                    self.live_bytes += u64::from(entry.raw_len);
                }
                Entry::Occupied(slot) if *slot.get() == entry.raw_len => {}
                Entry::Occupied(_) => {
                    return Err(CdcErr::format("gc", "one chunk hash with two lengths"));
                }
            }
        }
        Ok(())
    }
}

/// Mark every page reachable from the volume's superblock.
fn mark(store: &Store) -> Result<(LiveIndex, Snapshot)> {
    let snapshot = store.snapshot()?;
    let mut live = LiveIndex {
        pages: BTreeSet::from([SUPERBLOCK_PAGE.to_u32()]),
        ..LiveIndex::default()
    };

    for page in snapshot.index_pages()? {
        live.pages.insert(page.to_u32());
    }

    for manifest in &snapshot.manifests()? {
        live.add_manifest(&snapshot, manifest)?;
    }

    // Walk the chunk index once: referenced chunks are live, the rest is what
    // the sweep drops. A missing entry means a manifest is dangling.
    let mut found: HashSet<[u8; 32]> = HashSet::with_capacity(live.chunks.len());
    for (ChunkKey(hash), meta) in snapshot.chunk_entries()? {
        match live.chunks.get(&hash) {
            Some(&raw_len) if raw_len == meta.raw_len => {
                found.insert(hash);
                for offset in 0..meta.frames {
                    let page = meta
                        .start
                        .checked_add(offset)
                        .ok_or(CdcErr::PageOutOfRange(meta.start))?;
                    live.pages.insert(page);
                }
            }
            Some(_) => return Err(CdcErr::format("gc", "manifest and index lengths differ")),
            None => live.dead_chunk_frames += u64::from(meta.frames),
        }
    }
    if found.len() != live.chunks.len() {
        return Err(CdcErr::format("gc", "dangling chunk reference"));
    }

    Ok((live, snapshot))
}

fn stats_from(live: &LiveIndex, snapshot: &Snapshot) -> VolumeStats {
    // The watermark is the first never-written page and page 1 is the
    // superblock, so the pages that exist are 1..next_page.
    let pages = u64::from(snapshot.superblock().next_page).saturating_sub(1);
    VolumeStats {
        pages,
        live_pages: live.pages.len() as u64,
        dead_pages: pages.saturating_sub(live.pages.len() as u64),
        files: live.files,
        chunks: live.chunks.len() as u64,
        live_bytes: live.live_bytes,
        dead_chunk_frames: live.dead_chunk_frames,
    }
}

/// Mark and report, without writing anything.
pub fn stats(store: &Store) -> Result<VolumeStats> {
    let (live, snapshot) = mark(store)?;
    Ok(stats_from(&live, &snapshot))
}

/// Copy the live set of `source` into `target`, which must be an empty volume.
///
/// `source` stays untouched and readable: the swap is the caller's job, so a
/// failure here leaves the old volume authoritative.
pub fn rewrite(source: &Store, target: &Store) -> Result<GcStats> {
    let (live, snapshot) = mark(source)?;
    let before = stats_from(&live, &snapshot);
    ensure_empty(target)?;

    let src_keys = source.volume().keys();
    let dst_keys = target.volume().keys();
    let manifests = snapshot.manifests()?;

    // Chunk ids are keyed by per-volume keys, so copying a chunk gives it a new
    // id and every manifest reference has to be rewritten with it.
    let mut remap: HashMap<[u8; 32], [u8; 32]> = HashMap::with_capacity(live.chunks.len());
    let mut pending: Vec<(ChunkKey, Vec<u8>)> = Vec::new();
    let mut index: Vec<(ChunkKey, ChunkMeta)> = Vec::new();
    let mut pending_bytes = 0usize;
    let mut copied_chunks = 0u64;
    let mut copied_bytes = 0u64;

    for (ChunkKey(hash), meta) in snapshot.chunk_entries()? {
        if !live.chunks.contains_key(&hash) {
            continue;
        }
        let data = snapshot.read_chunk(&ChunkRef { hash, raw_len: meta.raw_len })?;
        // The copy verifies the index as it goes: a chunk whose plaintext does
        // not hash to its key is corruption, not something to replicate.
        if ChunkKey::new(src_keys, &data).0 != hash {
            return Err(CdcErr::format(
                "gc",
                "chunk content does not match its hash",
            ));
        }

        let new_key = ChunkKey::new(dst_keys, &data);
        remap.insert(hash, new_key.0);
        copied_chunks += 1;
        copied_bytes += data.len() as u64;
        pending_bytes += data.len();
        pending.push((new_key, data));
        if pending_bytes >= BATCH_BYTES {
            let pages = target.flush_chunks(&pending, &mut index)?;
            target.flush_chunk_index(&index, pages)?;
            index.clear();
            pending.clear();
            pending_bytes = 0;
        }
    }
    let pages = target.flush_chunks(&pending, &mut index)?;
    target.flush_chunk_index(&index, pages)?;

    target.volume().update(|txn| {
        let mut tree = BTree::<PathCodec>::new(txn.path_root());
        {
            let (io, alloc) = txn.parts();
            for manifest in &manifests {
                let refs = snapshot
                    .read_recipe(&manifest.recipe)?
                    .into_iter()
                    .map(|entry| {
                        let hash = *remap.get(&entry.hash).ok_or_else(|| {
                            CdcErr::format("gc", "manifest references an uncopied chunk")
                        })?;
                        Ok(ChunkRef { hash, raw_len: entry.raw_len })
                    })
                    .collect::<Result<Vec<_>>>()?;

                let recipe = if refs.len() <= INLINE_REFS {
                    Recipe::Inline(refs)
                } else {
                    let pages = refs_pages(refs.len()) as u32;
                    let start = alloc.alloc(io, pages)?;
                    write_recipe(io, start, &refs)?;
                    Recipe::Pages {
                        start: start.to_u32(),
                        pages,
                        len: refs.len() as u32,
                    }
                };
                let encoded = Manifest { stat: manifest.stat.clone(), recipe }.encode()?;
                tree.insert(io, alloc, manifest.stat.path.clone(), encoded)?;
            }
            tree.commit(io, alloc)?;
        }
        txn.set_path_root(tree.root());
        txn.set_accounting(copied_chunks, manifests.len() as u64, copied_bytes);
        Ok(())
    })?;

    // The rewrite must preserve the live set exactly, or the new volume is not
    // a replacement and the caller must not swap it in.
    let after = stats(target)?;
    if after.files != before.files
        || after.chunks != before.chunks
        || after.live_bytes != before.live_bytes
    {
        return Err(CdcErr::format("gc", "rewrite changed the live set"));
    }

    Ok(GcStats {
        before,
        after,
        copied_chunks,
        copied_bytes,
    })
}

fn ensure_empty(target: &Store) -> Result<()> {
    let snapshot = target.snapshot()?;
    let sb = snapshot.superblock();
    if sb.next_page != FIRST_ALLOCATABLE || sb.path_root_idx().is_some() {
        return Err(CdcErr::InvalidArgument("GC target volume is not empty"));
    }
    Ok(())
}
