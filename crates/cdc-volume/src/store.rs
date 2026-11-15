//! File-level operations: content-defined chunking, dedup, manifests, ranges.
//!
//! A file is a manifest in the path tree pointing at an ordered list of chunk
//! hashes, and the chunk tree maps each hash to where its frames live. Chunk
//! frames are written in batches, and the manifest is written last, so an
//! interrupted upload leaves unreachable chunks rather than a partial file.
//!
//! Range reads use the chunk lengths stored in the manifest, so a read only
//! needs the recipe page and the chunk entries it actually overlaps — no scan of
//! the rest of the index.

use std::{
    collections::HashSet,
    io::Read,
    time::{SystemTime, UNIX_EPOCH},
};

use graft::{core::PageIdx, rt::runtime::Runtime, volume_reader::VolumeReader};

use crate::{
    btree::{BTree, ChunkCodec, ChunkKey, ChunkMeta, PathCodec},
    chunker::chunk_stream,
    crypto::MASTER_KEY_LEN,
    err::{CdcErr, Result},
    io::{FrameIo, PageRead, PageWrite},
    layout::{PAYLOAD_SIZE, frames_for},
    volume::CdcVolume,
};

/// File metadata as seen by `stat` and `ls`; no chunk list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub path: Vec<u8>,
    pub size: u64,
    pub mtime_ns: u64,
    pub chunk_count: u32,
}

/// One entry of a manifest's recipe: which chunk, and how long it is.
///
/// Storing the length here is what makes range reads cheap: overlapping chunks
/// can be selected without reading the chunk tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkRef {
    pub hash: [u8; 32],
    pub raw_len: u32,
}

const MANIFEST_VERSION: u8 = 1;
const STAT_LEN: usize = 1 + 8 + 8 + 4;
const REF_LEN: usize = 36;
/// Chunk refs kept inside the manifest value itself, before spilling to pages.
pub const INLINE_REFS: usize = 48;
/// Chunk refs per recipe page: 112 * 36 = 4032 bytes of the 4056-byte payload.
const REFS_PER_PAGE: usize = PAYLOAD_SIZE / REF_LEN;

/// How many plaintext bytes of chunks one transaction stages at most.
pub const BATCH_BYTES: usize = 32 << 20;

/// Pages per remote segment frame (`FRAME_MAX_PAGES` in graft).
///
/// A segment packs its pages into frames of this many pages, in page order, so a
/// page read costs one frame. Frames begin at the segment's first page, which is
/// always the superblock: every commit rewrites it, so page 1 is in every pushed
/// segment and the frame grid is fixed at pages 1, 65, 129, …
const FRAME_PAGES: u32 = 64;

/// How much write amplification frame alignment may cost.
///
/// Filler pages are encrypted, so they do not compress: one frame of them is a
/// real 256 KiB on the wire. A batch that wrote fewer than `PAD_RATIO` pages per
/// filler page keeps its index where it lands instead — for a small file the
/// index sits in the same frame as its data anyway, and a 40 KiB upload must not
/// become a 300 KiB one.
const PAD_RATIO: u32 = 8;

/// Filler pages needed for the next allocation to begin a frame.
#[inline]
const fn frame_pad(next_page: u32) -> u32 {
    let offset = next_page.saturating_sub(1) % FRAME_PAGES;
    (FRAME_PAGES - offset) % FRAME_PAGES
}

/// Where a manifest's chunk refs live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Recipe {
    Inline(Vec<ChunkRef>),
    Pages { start: u32, pages: u32, len: u32 },
}

/// A decoded path-tree value.
#[derive(Clone, Debug)]
pub(crate) struct Manifest {
    pub(crate) stat: FileStat,
    pub(crate) recipe: Recipe,
}

impl Manifest {
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(STAT_LEN + self.stat.chunk_count as usize * REF_LEN);
        out.push(MANIFEST_VERSION);
        out.extend_from_slice(&self.stat.size.to_le_bytes());
        out.extend_from_slice(&self.stat.mtime_ns.to_le_bytes());
        out.extend_from_slice(&self.stat.chunk_count.to_le_bytes());
        match &self.recipe {
            Recipe::Inline(refs) => {
                for entry in refs {
                    out.extend_from_slice(&entry.hash);
                    out.extend_from_slice(&entry.raw_len.to_le_bytes());
                }
            }
            Recipe::Pages { start, pages, len } => {
                out.extend_from_slice(&start.to_le_bytes());
                out.extend_from_slice(&pages.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
            }
        }
        Ok(out)
    }

    pub(crate) fn decode(path: Vec<u8>, bytes: &[u8]) -> Result<Self> {
        if bytes.len() < STAT_LEN {
            return Err(CdcErr::format("manifest", "truncated header"));
        }
        if bytes[0] != MANIFEST_VERSION {
            return Err(CdcErr::format("manifest", "unsupported version"));
        }
        let size = u64::from_le_bytes(bytes[1..9].try_into().expect("8 bytes"));
        let mtime_ns = u64::from_le_bytes(bytes[9..17].try_into().expect("8 bytes"));
        let chunk_count = u32::from_le_bytes(bytes[17..21].try_into().expect("4 bytes"));
        let stat = FileStat { path, size, mtime_ns, chunk_count };

        let recipe = if chunk_count as usize <= INLINE_REFS {
            let body = &bytes[STAT_LEN..];
            if body.len() != chunk_count as usize * REF_LEN {
                return Err(CdcErr::format("manifest", "ragged inline refs"));
            }
            Recipe::Inline(
                body.as_chunks::<{ REF_LEN }>()
                    .0
                    .iter()
                    .map(decode_ref)
                    .collect(),
            )
        } else {
            if bytes.len() != STAT_LEN + 12 {
                return Err(CdcErr::format("manifest", "ragged recipe pointer"));
            }
            let start = u32::from_le_bytes(bytes[STAT_LEN..STAT_LEN + 4].try_into().expect("4"));
            let pages =
                u32::from_le_bytes(bytes[STAT_LEN + 4..STAT_LEN + 8].try_into().expect("4"));
            let len = u32::from_le_bytes(bytes[STAT_LEN + 8..STAT_LEN + 12].try_into().expect("4"));
            if len != chunk_count || pages as usize != refs_pages(len as usize) {
                return Err(CdcErr::format("manifest", "recipe shape mismatch"));
            }
            Recipe::Pages { start, pages, len }
        };
        Ok(Self { stat, recipe })
    }
}

fn decode_ref(bytes: &[u8; REF_LEN]) -> ChunkRef {
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes[..32]);
    ChunkRef {
        hash,
        raw_len: u32::from_le_bytes(bytes[32..36].try_into().expect("4 bytes")),
    }
}

pub(crate) fn refs_pages(count: usize) -> usize {
    count.div_ceil(REFS_PER_PAGE)
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64)
}

/// Smallest key strictly greater than every key starting with `prefix`.
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last == u8::MAX {
            end.pop();
        } else {
            *last += 1;
            return Some(end);
        }
    }
    None
}

/// A consistent read view: one Graft snapshot plus its superblock.
///
/// Reads served through one snapshot are counted, so read amplification can be
/// measured without instrumentation inside the store.
pub struct Snapshot {
    reader: FrameIo<VolumeReader>,
    sb: crate::superblock::Superblock,
}

impl Snapshot {
    /// Number of encrypted frames read since the snapshot was taken.
    pub fn reads(&self) -> u64 {
        self.reader.reads()
    }

    pub fn reset_counters(&self) {
        self.reader.reset_counters();
    }

    pub fn stat(&self, path: &[u8]) -> Result<Option<FileStat>> {
        Ok(self.manifest(path)?.map(|manifest| manifest.stat))
    }

    /// All files whose path starts with `prefix`, in byte order.
    pub fn ls(&self, prefix: &[u8]) -> Result<Vec<FileStat>> {
        let tree = BTree::<PathCodec>::new(self.sb.path_root_idx());
        let start = prefix.to_vec();
        let end = prefix_successor(prefix);
        let mut raw = Vec::new();
        tree.scan(
            &self.reader,
            Some(&start),
            end.as_ref(),
            &mut |key, value| {
                if key.starts_with(prefix) {
                    raw.push((key.clone(), value.clone()));
                }
            },
        )?;
        raw.into_iter()
            .map(|(key, value)| Manifest::decode(key, &value).map(|manifest| manifest.stat))
            .collect()
    }

    pub fn get_file(&self, path: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(manifest) = self.manifest(path)? else {
            return Ok(None);
        };
        let refs = self.read_recipe(&manifest.recipe)?;
        let mut out = Vec::with_capacity(manifest.stat.size as usize);
        for entry in &refs {
            out.extend_from_slice(&self.read_chunk(entry)?);
        }
        if out.len() as u64 != manifest.stat.size {
            return Err(CdcErr::format("file", "size does not match its chunks"));
        }
        Ok(Some(out))
    }

    /// Read `len` bytes at `offset`, clamping at end of file.
    pub fn read_range(&self, path: &[u8], offset: u64, len: u64) -> Result<Vec<u8>> {
        let Some(manifest) = self.manifest(path)? else {
            return Err(CdcErr::NotFound(String::from_utf8_lossy(path).into_owned()));
        };
        if offset > manifest.stat.size {
            return Err(CdcErr::InvalidArgument("range starts past end of file"));
        }
        let end = offset.saturating_add(len).min(manifest.stat.size);
        if end == offset {
            return Ok(Vec::new());
        }

        let refs = self.read_recipe(&manifest.recipe)?;
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut position = 0u64;
        for entry in &refs {
            let chunk_end = position + u64::from(entry.raw_len);
            if chunk_end <= offset {
                position = chunk_end;
                continue;
            }
            if position >= end {
                break;
            }
            let bytes = self.read_chunk(entry)?;
            let from = offset.saturating_sub(position) as usize;
            let to = (end.min(chunk_end) - position) as usize;
            out.extend_from_slice(&bytes[from..to]);
            position = chunk_end;
        }
        Ok(out)
    }

    /// Every live manifest in path order; GC uses this to find live chunks
    /// without trusting the chunk index.
    pub(crate) fn manifests(&self) -> Result<Vec<Manifest>> {
        let tree = BTree::<PathCodec>::new(self.sb.path_root_idx());
        tree.scan_all(&self.reader)?
            .into_iter()
            .map(|(path, value)| Manifest::decode(path, &value))
            .collect()
    }

    /// Node pages and values of both trees, for GC's live-page sweep.
    pub(crate) fn chunk_entries(&self) -> Result<Vec<(ChunkKey, ChunkMeta)>> {
        let tree = BTree::<ChunkCodec>::new(self.sb.chunk_root_idx());
        tree.scan_all(&self.reader)
    }

    pub(crate) fn index_pages(&self) -> Result<Vec<PageIdx>> {
        let mut pages = BTree::<PathCodec>::new(self.sb.path_root_idx()).pages(&self.reader)?;
        pages.extend(BTree::<ChunkCodec>::new(self.sb.chunk_root_idx()).pages(&self.reader)?);
        Ok(pages)
    }

    pub(crate) fn superblock(&self) -> &crate::superblock::Superblock {
        &self.sb
    }

    fn manifest(&self, path: &[u8]) -> Result<Option<Manifest>> {
        let tree = BTree::<PathCodec>::new(self.sb.path_root_idx());
        match tree.get(&self.reader, &path.to_vec())? {
            Some(value) => Ok(Some(Manifest::decode(path.to_vec(), &value)?)),
            None => Ok(None),
        }
    }

    fn chunk_meta(&self, hash: &[u8; 32]) -> Result<ChunkMeta> {
        let tree = BTree::<ChunkCodec>::new(self.sb.chunk_root_idx());
        tree.get(&self.reader, &ChunkKey(*hash))?
            .ok_or(CdcErr::format("chunk index", "dangling chunk reference"))
    }

    pub(crate) fn read_recipe(&self, recipe: &Recipe) -> Result<Vec<ChunkRef>> {
        match recipe {
            Recipe::Inline(refs) => Ok(refs.clone()),
            Recipe::Pages { start, pages, len } => {
                let mut refs = Vec::with_capacity(*len as usize);
                for page in 0..*pages {
                    let idx = PageIdx::try_new(start.saturating_add(page))
                        .ok_or(CdcErr::PageOutOfRange(*start))?;
                    let payload = self.reader.read_payload(idx)?;
                    for entry in payload.as_chunks::<{ REF_LEN }>().0 {
                        if refs.len() == *len as usize {
                            break;
                        }
                        refs.push(decode_ref(entry));
                    }
                }
                if refs.len() != *len as usize {
                    return Err(CdcErr::format("manifest", "short recipe"));
                }
                Ok(refs)
            }
        }
    }

    pub(crate) fn read_chunk(&self, entry: &ChunkRef) -> Result<Vec<u8>> {
        let meta = self.chunk_meta(&entry.hash)?;
        if meta.raw_len != entry.raw_len {
            return Err(CdcErr::format("chunk", "manifest and index lengths differ"));
        }
        let mut out = Vec::with_capacity(meta.raw_len as usize);
        for offset in 0..meta.frames {
            let idx = PageIdx::try_new(meta.start.saturating_add(offset))
                .ok_or(CdcErr::PageOutOfRange(meta.start))?;
            let payload = self.reader.read_payload(idx)?;
            let take = (meta.raw_len as usize - out.len()).min(PAYLOAD_SIZE);
            out.extend_from_slice(&payload[..take]);
        }
        Ok(out)
    }
}

/// File operations over one volume.
#[derive(Clone, Debug)]
pub struct Store {
    volume: CdcVolume,
}

impl Store {
    pub fn create(
        rt: Runtime,
        vid: graft::core::VolumeId,
        master_key: &[u8; MASTER_KEY_LEN],
    ) -> Result<Self> {
        Ok(Self {
            volume: CdcVolume::init(rt, vid, master_key)?,
        })
    }

    pub fn open(
        rt: Runtime,
        vid: graft::core::VolumeId,
        master_key: &[u8; MASTER_KEY_LEN],
    ) -> Result<Self> {
        Ok(Self {
            volume: CdcVolume::open(rt, vid, master_key)?,
        })
    }

    pub fn volume(&self) -> &CdcVolume {
        &self.volume
    }

    /// Take a read snapshot for a batch of reads.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let reader = self.volume.reader()?;
        let sb = reader.read_superblock()?;
        Ok(Snapshot { reader, sb })
    }

    /// Chunk and store `input` at `path`, replacing any existing entry.
    ///
    /// Chunk data is committed in batches and the manifest last, so a failure
    /// part-way through leaves unreachable chunks, never a partial file.
    pub fn put_file<R: Read>(&self, path: &[u8], input: R) -> Result<FileStat> {
        if path.is_empty() {
            return Err(CdcErr::InvalidArgument("empty path"));
        }

        let mut refs = Vec::new();
        let mut batch: Vec<(ChunkKey, Vec<u8>)> = Vec::new();
        let mut index: Vec<(ChunkKey, ChunkMeta)> = Vec::new();
        let mut batch_bytes = 0usize;
        let mut staged: HashSet<ChunkKey> = HashSet::new();
        let mut size = 0u64;

        for chunk in chunk_stream(input) {
            let data = chunk?;
            let key = ChunkKey::new(self.volume.keys(), &data);
            refs.push(ChunkRef {
                hash: key.0,
                raw_len: u32::try_from(data.len())
                    .map_err(|_| CdcErr::InvalidArgument("chunk longer than 4 GiB"))?,
            });
            size += data.len() as u64;

            // Only stage a repeat once per batch; across batches the chunk tree
            // lookup in `flush_chunks` still keeps it from being written twice.
            if staged.insert(key) {
                batch_bytes += data.len();
                batch.push((key, data));
            }
            if batch_bytes >= BATCH_BYTES {
                let pages = self.flush_chunks(&batch, &mut index)?;
                self.flush_chunk_index(&index, pages)?;
                index.clear();
                batch.clear();
                batch_bytes = 0;
                staged.clear();
            }
        }
        let pages = self.flush_chunks(&batch, &mut index)?;
        self.flush_chunk_index(&index, pages)?;

        if refs.len() > u32::MAX as usize {
            return Err(CdcErr::InvalidArgument("file has too many chunks"));
        }
        let stat = FileStat {
            path: path.to_vec(),
            size,
            mtime_ns: now_ns(),
            chunk_count: refs.len() as u32,
        };
        self.write_manifest(&stat, &refs)?;
        Ok(stat)
    }

    /// Write chunk frames for one batch and record their metadata.
    ///
    /// The chunk index is deliberately not updated here: `flush_chunk_index`
    /// writes it once the batch's data pages all exist. Graft builds remote
    /// frames from page order, so appending index entries while data pages are
    /// still being allocated scatters the index across the file's data frames.
    /// Writing it after the batch keeps the index in a few adjacent pages, which
    /// is all a cold range read has to fetch on top of the chunk's own frame.
    ///
    /// Per batch, not once per file: a committed batch's entries let a retry
    /// after a failed upload deduplicate what already reached the volume.
    ///
    /// Chunks already present in the index are skipped, which is what keeps a
    /// deduplicated upload from rewriting frames. GC reuses this to fill a fresh
    /// volume.
    pub(crate) fn flush_chunks(
        &self,
        batch: &[(ChunkKey, Vec<u8>)],
        index: &mut Vec<(ChunkKey, ChunkMeta)>,
    ) -> Result<u32> {
        if batch.is_empty() {
            return Ok(0);
        }
        self.volume.update(|txn| {
            let mut written = 0u32;
            let tree = BTree::<ChunkCodec>::new(txn.chunk_root());
            let mut known: HashSet<ChunkKey> = index.iter().map(|(key, _)| *key).collect();
            {
                let (io, alloc) = txn.parts();
                for (key, data) in batch {
                    // Identical chunks are stored once; identity is the key.
                    if known.contains(key) || tree.get(&*io, key)?.is_some() {
                        continue;
                    }
                    let frames = frames_for(data.len() as u64);
                    let start = alloc.alloc(io, frames)?;
                    write_chunk(io, start, data)?;
                    written += frames;
                    known.insert(*key);
                    index.push((
                        *key,
                        ChunkMeta {
                            start: start.to_u32(),
                            frames,
                            raw_len: data.len() as u32,
                        },
                    ));
                }
            }
            Ok(written)
        })
    }

    /// Insert the batch's chunk entries in one commit, in key order.
    ///
    /// Sorted insertion only ever splits the rightmost leaf, so the index pages
    /// it allocates are contiguous — one small region instead of a trail through
    /// the data pages. The region is also pushed to the next frame boundary, so a
    /// reader that needs the index fetches one frame whatever the batch left
    /// behind: without the padding a 64-page frame boundary could fall inside
    /// the ~7-page index block and split it over two frames, costing a third
    /// request on every cold range read. `batch_pages` bounds what the alignment
    /// is allowed to cost; see `PAD_RATIO`.
    pub(crate) fn flush_chunk_index(
        &self,
        index: &[(ChunkKey, ChunkMeta)],
        batch_pages: u32,
    ) -> Result<()> {
        if index.is_empty() {
            return Ok(());
        }
        let mut sorted = index.to_vec();
        sorted.sort_by_key(|(key, _)| *key);
        self.volume.update(|txn| {
            let mut tree = BTree::<ChunkCodec>::new(txn.chunk_root());
            {
                let (io, alloc) = txn.parts();
                // Filler pages are never referenced by any tree; they only move
                // the frame grid. Skipped when the next run may come from the
                // free chain, where the position is not ours to choose.
                if alloc.append_only() {
                    let pad = frame_pad(alloc.next_page());
                    if pad > 0 && pad.saturating_mul(PAD_RATIO) <= batch_pages {
                        let start = alloc.alloc(io, pad)?;
                        let blank = [0u8; PAYLOAD_SIZE];
                        for offset in 0..pad {
                            io.write_payload(start.saturating_add(offset), &blank)?;
                        }
                    }
                }
                for (key, meta) in &sorted {
                    tree.insert(io, alloc, *key, *meta)?;
                }
                tree.commit(io, alloc)?;
            }
            txn.set_chunk_root(tree.root());
            Ok(())
        })
    }

    /// Insert the path entry, spilling its chunk list to recipe pages if large.
    fn write_manifest(&self, stat: &FileStat, refs: &[ChunkRef]) -> Result<()> {
        self.volume.update(|txn| {
            let mut tree = BTree::<PathCodec>::new(txn.path_root());
            {
                let (io, alloc) = txn.parts();
                let recipe = if refs.len() <= INLINE_REFS {
                    Recipe::Inline(refs.to_vec())
                } else {
                    let pages = refs_pages(refs.len()) as u32;
                    let start = alloc.alloc(io, pages)?;
                    write_recipe(io, start, refs)?;
                    Recipe::Pages {
                        start: start.to_u32(),
                        pages,
                        len: refs.len() as u32,
                    }
                };
                let manifest = Manifest { stat: stat.clone(), recipe };
                tree.insert(io, alloc, stat.path.clone(), manifest.encode()?)?;
                tree.commit(io, alloc)?;
            }
            txn.set_path_root(tree.root());
            Ok(())
        })
    }

    pub fn stat(&self, path: &[u8]) -> Result<Option<FileStat>> {
        self.snapshot()?.stat(path)
    }

    pub fn get_file(&self, path: &[u8]) -> Result<Option<Vec<u8>>> {
        self.snapshot()?.get_file(path)
    }

    pub fn read_range(&self, path: &[u8], offset: u64, len: u64) -> Result<Vec<u8>> {
        self.snapshot()?.read_range(path, offset, len)
    }

    pub fn ls(&self, prefix: &[u8]) -> Result<Vec<FileStat>> {
        self.snapshot()?.ls(prefix)
    }

    /// Remove a path entry. Chunk entries and frames stay until GC runs, since
    /// other files may reference the same chunks.
    pub fn delete_file(&self, path: &[u8]) -> Result<bool> {
        self.volume.update(|txn| {
            let mut tree = BTree::<PathCodec>::new(txn.path_root());
            let removed = {
                let (io, alloc) = txn.parts();
                let removed = tree.remove(io, alloc, &path.to_vec())?;
                tree.commit(io, alloc)?;
                removed
            };
            txn.set_path_root(tree.root());
            Ok(removed)
        })
    }
}

fn write_chunk(io: &mut impl PageWrite, start: PageIdx, data: &[u8]) -> Result<()> {
    for (i, part) in data.chunks(PAYLOAD_SIZE).enumerate() {
        let mut payload = [0u8; PAYLOAD_SIZE];
        payload[..part.len()].copy_from_slice(part);
        io.write_payload(start.saturating_add(i as u32), &payload)?;
    }
    Ok(())
}

pub(crate) fn write_recipe(
    io: &mut impl PageWrite,
    start: PageIdx,
    refs: &[ChunkRef],
) -> Result<()> {
    for (i, group) in refs.chunks(REFS_PER_PAGE).enumerate() {
        let mut payload = [0u8; PAYLOAD_SIZE];
        for (j, entry) in group.iter().enumerate() {
            let base = j * REF_LEN;
            payload[base..base + 32].copy_from_slice(&entry.hash);
            payload[base + 32..base + 36].copy_from_slice(&entry.raw_len.to_le_bytes());
        }
        io.write_payload(start.saturating_add(i as u32), &payload)?;
    }
    Ok(())
}
