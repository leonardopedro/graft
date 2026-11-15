//! Copy-on-write B+ tree over encrypted Graft pages.
//!
//! Nodes are ordinary frames: a 32-byte header, then `count` entries, then
//! `count + 1` child page pointers stored at the tail of the payload. Internal
//! node keys are lower bounds: `key[i]` is a lower bound for `child[i + 1]`.
//! Lower bounds need not be exact after deletes (a stale low separator only
//! causes an extra descent), but they are kept exact when an insert lowers a
//! subtree's minimum, otherwise a search could skip the child holding the key.
//!
//! Writes never modify a live page. The first touch of a node in a transaction
//! allocates a fresh page and records the old page to be freed when the tree is
//! committed. Changed nodes stay in the transaction cache and are sealed once
//! by [`BTree::commit`], so a transaction writes one frame per touched node
//! rather than one per operation.
//!
//! [`BTree::commit`] is mandatory: it both writes the staged nodes and frees the
//! pages they replaced. Without it the tree's new root would point at pages that
//! were never written.

use std::collections::HashMap;

use graft::core::PageIdx;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    alloc::Allocator,
    err::{CdcErr, Result},
    io::{PageRead, PageWrite},
    layout::{PAYLOAD_SIZE, Payload},
};

const HEADER_LEN: usize = 32;
const NODE_MAGIC: [u8; 2] = *b"GN";

#[derive(FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C)]
struct NodeHeader {
    magic: [u8; 2],
    kind: u8,
    level: u8,
    count: u16,
    /// Bytes of packed entries starting at `HEADER_LEN`. Needed because
    /// variable-length entries leave padding before the child pointers.
    entries_len: u16,
    reserved: [u8; 24],
}

const _: () = assert!(size_of::<NodeHeader>() == HEADER_LEN);

/// Layout and key/value encoding for one tree.
pub trait Codec {
    type Key: Ord + Clone + std::fmt::Debug;
    type Value: Clone + std::fmt::Debug;

    /// Distinguishes chunk-tree nodes from path-tree nodes.
    const KIND: u8;

    fn decode_entries(buf: &[u8]) -> Result<Vec<(Self::Key, Self::Value)>>;
    fn encode_entry(out: &mut Vec<u8>, key: &Self::Key, value: &Self::Value) -> Result<()>;
    /// Encoded size of one entry, so node fit can be decided without packing.
    fn entry_size(key: &Self::Key, value: &Self::Value) -> usize;
    fn empty_value() -> Self::Value;
}

#[derive(Clone)]
struct Node<K, V> {
    level: u8,
    entries: Vec<(K, V)>,
    children: Vec<PageIdx>,
}

struct Cached<K, V> {
    /// Current location of this node (a freshly allocated page).
    page: PageIdx,
    /// Where the node used to live, freed when the tree commits.
    old: Option<PageIdx>,
    node: Node<K, V>,
    written: bool,
    dropped: bool,
}

#[derive(Default)]
struct Cache<K, V> {
    slots: Vec<Cached<K, V>>,
    index: HashMap<PageIdx, usize>,
}

impl<K, V> Cache<K, V> {
    fn get(&self, idx: PageIdx) -> Option<&Cached<K, V>> {
        self.index.get(&idx).map(|&i| &self.slots[i])
    }

    fn resolve(&self, idx: PageIdx) -> PageIdx {
        self.get(idx).map_or(idx, |slot| slot.page)
    }

    fn insert_slot(&mut self, page: PageIdx, node: Node<K, V>, old: Option<PageIdx>) -> usize {
        let slot = Cached {
            page,
            old,
            node,
            written: false,
            dropped: false,
        };
        let i = self.slots.len();
        self.slots.push(slot);
        self.index.insert(page, i);
        if let Some(old) = old {
            self.index.insert(old, i);
        }
        i
    }
}

/// A B+ tree rooted at one page.
pub struct BTree<C: Codec> {
    root: Option<PageIdx>,
    cache: Cache<C::Key, C::Value>,
}

impl<C: Codec> BTree<C> {
    pub fn new(root: Option<PageIdx>) -> Self {
        Self {
            root,
            cache: Cache { slots: Vec::new(), index: HashMap::new() },
        }
    }

    /// Current root page, including pages staged by this transaction.
    #[inline]
    pub fn root(&self) -> Option<PageIdx> {
        self.root.map(|root| self.cache.resolve(root))
    }

    /// Point lookup. Reads one frame per level.
    pub fn get(&self, io: &impl PageRead, key: &C::Key) -> Result<Option<C::Value>> {
        let Some(mut idx) = self.root else {
            return Ok(None);
        };

        for _ in 0..=MAX_LEVEL {
            let node = self.peek(io, idx)?;
            if node.level == 0 {
                let pos = node.entries.partition_point(|(k, _)| k < key);
                return Ok(node
                    .entries
                    .get(pos)
                    .filter(|(k, _)| k == key)
                    .map(|(_, v)| v.clone()));
            }
            let pos = node.entries.partition_point(|(k, _)| k <= key);
            idx = *node
                .children
                .get(pos)
                .ok_or_else(|| CdcErr::format("node", "missing child pointer"))?;
        }
        Err(CdcErr::format("tree", "deeper than the maximum level"))
    }

    /// Insert or replace one entry.
    pub fn insert(
        &mut self,
        io: &mut impl PageWrite,
        alloc: &mut Allocator,
        key: C::Key,
        value: C::Value,
    ) -> Result<()> {
        let Some(root) = self.root else {
            let leaf = Node {
                level: 0,
                entries: vec![(key, value)],
                children: Vec::new(),
            };
            let page = self.alloc_node(io, alloc, &leaf)?;
            self.root = Some(page);
            return Ok(());
        };

        let (page, split, _) = self.insert_rec(io, alloc, root, &key, &value)?;
        self.root = Some(match split {
            None => page,
            Some((sep, right)) => {
                let level = self
                    .cache
                    .get(page)
                    .map_or(0, |slot| slot.node.level)
                    .saturating_add(1);
                let new_root = Node {
                    level,
                    entries: vec![(sep, C::empty_value())],
                    children: vec![page, right],
                };
                self.alloc_node(io, alloc, &new_root)?
            }
        });
        Ok(())
    }

    /// Remove one entry. Returns whether it was present.
    pub fn remove(
        &mut self,
        io: &mut impl PageWrite,
        alloc: &mut Allocator,
        key: &C::Key,
    ) -> Result<bool> {
        if !self.contains(io, key)? {
            return Ok(false);
        }
        let Some(root) = self.root else {
            return Ok(false);
        };
        let (new_root, _) = self.remove_rec(io, alloc, root, key)?;
        self.root = new_root;
        Ok(true)
    }

    /// Every node page of the committed tree, root first.
    ///
    /// Used by GC to mark live index pages. Staged (uncommitted) nodes are not
    /// visited; call this on a freshly built `BTree` over a read snapshot.
    pub fn pages(&self, io: &impl PageRead) -> Result<Vec<PageIdx>> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            self.pages_rec(io, root, &mut out)?;
        }
        Ok(out)
    }

    fn pages_rec(&self, io: &impl PageRead, idx: PageIdx, out: &mut Vec<PageIdx>) -> Result<()> {
        let node = self.peek(io, idx)?;
        out.push(idx);
        for child in &node.children {
            self.pages_rec(io, *child, out)?;
        }
        Ok(())
    }

    fn contains(&self, io: &impl PageRead, key: &C::Key) -> Result<bool> {
        self.get(io, key).map(|value| value.is_some())
    }

    /// In-order scan of `[start, end]` (inclusive on both ends, `None` = open).
    ///
    /// Ordering is byte ordering of the encoded key, which is what makes an
    /// ordered path tree usable for prefix listings.
    pub fn scan(
        &self,
        io: &impl PageRead,
        start: Option<&C::Key>,
        end: Option<&C::Key>,
        visit: &mut impl FnMut(&C::Key, &C::Value),
    ) -> Result<()> {
        let Some(root) = self.root else {
            return Ok(());
        };
        self.scan_rec(io, root, start, end, visit)
    }

    /// All entries in key order.
    pub fn scan_all(&self, io: &impl PageRead) -> Result<Vec<(C::Key, C::Value)>> {
        let mut out = Vec::new();
        self.scan(io, None, None, &mut |key, value| {
            out.push((key.clone(), value.clone()));
        })?;
        Ok(out)
    }

    /// Root level of the tree (0 for a single leaf), i.e. one less than the
    /// number of frames a point lookup reads.
    pub fn height(&self, io: &impl PageRead) -> Result<u8> {
        let Some(root) = self.root else {
            return Ok(0);
        };
        Ok(self.peek(io, root)?.level)
    }

    /// Write every staged node and free the pages this transaction replaced.
    ///
    /// Must be called before the enclosing transaction commits: the tree's root
    /// points at pages that only exist in the cache until this runs.
    pub fn commit(&mut self, io: &mut impl PageWrite, alloc: &mut Allocator) -> Result<()> {
        let slots = std::mem::take(&mut self.cache.slots);
        self.cache.index.clear();

        for slot in &slots {
            if !slot.dropped && slot.written {
                let payload = pack_node::<C>(&slot.node)?.ok_or(CdcErr::EntryTooLarge)?;
                io.write_payload(slot.page, &payload)?;
            }
        }

        for slot in slots {
            if slot.dropped || !slot.written {
                alloc.free(io, slot.page, 1)?;
            }
            if let Some(old) = slot.old {
                alloc.free(io, old, 1)?;
            }
        }
        Ok(())
    }

    // -- internals ---------------------------------------------------------

    /// Read a node, preferring the transaction's staged copy.
    fn peek(&self, io: &impl PageRead, idx: PageIdx) -> Result<Node<C::Key, C::Value>> {
        let resolved = self.cache.resolve(idx);
        if let Some(slot) = self.cache.get(resolved) {
            return Ok(slot.node.clone());
        }
        read_node::<C>(io, resolved)
    }

    /// Read a node for mutation, allocating its copy-on-write page on first touch.
    fn load_mut(
        &mut self,
        io: &mut impl PageWrite,
        alloc: &mut Allocator,
        idx: PageIdx,
    ) -> Result<Loaded<C>> {
        let resolved = self.cache.resolve(idx);
        if let Some(slot) = self.cache.get(resolved) {
            return Ok((slot.page, slot.node.clone()));
        }

        let node = read_node::<C>(&*io, resolved)?;
        let page = alloc.alloc(io, 1)?;
        self.cache.insert_slot(page, node.clone(), Some(resolved));
        Ok((page, node))
    }

    fn alloc_node(
        &mut self,
        io: &mut impl PageWrite,
        alloc: &mut Allocator,
        node: &Node<C::Key, C::Value>,
    ) -> Result<PageIdx> {
        let page = alloc.alloc(io, 1)?;
        self.cache.insert_slot(page, node.clone(), None);
        self.stage_slot(page, node.clone())?;
        Ok(page)
    }

    /// Mark a node's current contents dirty; the frame is sealed by `commit`.
    fn stage_slot(&mut self, page: PageIdx, node: Node<C::Key, C::Value>) -> Result<()> {
        let slot_idx = *self
            .cache
            .index
            .get(&page)
            .ok_or_else(|| CdcErr::format("node", "slot missing for page"))?;
        let slot = &mut self.cache.slots[slot_idx];
        slot.node = node;
        slot.written = true;
        slot.dropped = false;
        Ok(())
    }

    fn drop_slot(&mut self, page: PageIdx) -> Result<()> {
        let slot_idx = *self
            .cache
            .index
            .get(&page)
            .ok_or_else(|| CdcErr::format("node", "slot missing for page"))?;
        self.cache.slots[slot_idx].dropped = true;
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn insert_rec(
        &mut self,
        io: &mut impl PageWrite,
        alloc: &mut Allocator,
        idx: PageIdx,
        key: &C::Key,
        value: &C::Value,
    ) -> Result<(PageIdx, Option<(C::Key, PageIdx)>, bool)> {
        let (page, mut node) = self.load_mut(io, alloc, idx)?;

        if node.level == 0 {
            let pos = node.entries.partition_point(|(k, _)| k < key);
            let mut min_lowered = false;
            if pos < node.entries.len() && node.entries[pos].0 == *key {
                node.entries[pos].1 = value.clone();
            } else {
                if pos == 0 {
                    min_lowered = true;
                }
                node.entries.insert(pos, (key.clone(), value.clone()));
            }

            if fits::<C>(&node) {
                self.stage_slot(page, node)?;
                return Ok((page, None, min_lowered));
            }

            // Split the leaf; the separator is the first key of the right half.
            let mid = node.entries.len() / 2;
            if mid == 0 {
                return Err(CdcErr::EntryTooLarge);
            }
            let right_entries = node.entries.split_off(mid);
            let sep = right_entries[0].0.clone();

            let left = Node {
                level: 0,
                entries: node.entries,
                children: Vec::new(),
            };
            let right = Node {
                level: 0,
                entries: right_entries,
                children: Vec::new(),
            };

            if !fits::<C>(&left) {
                return Err(CdcErr::EntryTooLarge);
            }
            self.stage_slot(page, left)?;
            let right_page = self.alloc_node(io, alloc, &right)?;
            return Ok((page, Some((sep, right_page)), min_lowered));
        }

        let pos = node.entries.partition_point(|(k, _)| k <= key);
        let child = *node
            .children
            .get(pos)
            .ok_or_else(|| CdcErr::format("node", "missing child pointer"))?;

        let (child_page, split, child_min_lowered) =
            self.insert_rec(io, alloc, child, key, value)?;
        node.children[pos] = child_page;

        if let Some((sep, right)) = split {
            node.children.insert(pos + 1, right);
            node.entries.insert(pos, (sep, C::empty_value()));
        }

        let mut min_lowered = false;
        if child_min_lowered {
            if pos > 0 {
                // The separator before this child is its lower bound.
                node.entries[pos - 1].0 = key.clone();
            } else {
                min_lowered = true;
            }
        }

        if fits::<C>(&node) {
            self.stage_slot(page, node)?;
            return Ok((page, None, min_lowered));
        }

        // Split an internal node: the middle key moves up to the parent.
        let mid = node.entries.len() / 2;
        if mid == 0 {
            return Err(CdcErr::EntryTooLarge);
        }
        let promoted = node.entries[mid].0.clone();
        let right_entries = node.entries.split_off(mid + 1);
        let _promoted = node.entries.pop();
        let right_children = node.children.split_off(mid + 1);

        let left = Node {
            level: node.level,
            entries: node.entries,
            children: node.children,
        };
        let right = Node {
            level: node.level,
            entries: right_entries,
            children: right_children,
        };

        if !fits::<C>(&left) {
            return Err(CdcErr::EntryTooLarge);
        }
        self.stage_slot(page, left)?;
        let right_page = self.alloc_node(io, alloc, &right)?;
        Ok((page, Some((promoted, right_page)), min_lowered))
    }

    fn remove_rec(
        &mut self,
        io: &mut impl PageWrite,
        alloc: &mut Allocator,
        idx: PageIdx,
        key: &C::Key,
    ) -> Result<(Option<PageIdx>, bool)> {
        let (page, mut node) = self.load_mut(io, alloc, idx)?;

        if node.level == 0 {
            let pos = node.entries.partition_point(|(k, _)| k < key);
            if pos == node.entries.len() || node.entries[pos].0 != *key {
                return Err(CdcErr::format("tree", "remove of absent key"));
            }
            node.entries.remove(pos);

            if node.entries.is_empty() {
                self.drop_slot(page)?;
                return Ok((None, true));
            }
            if !fits::<C>(&node) {
                return Err(CdcErr::EntryTooLarge);
            }
            self.stage_slot(page, node)?;
            return Ok((Some(page), true));
        }

        let pos = node.entries.partition_point(|(k, _)| k <= key);
        let child = *node
            .children
            .get(pos)
            .ok_or_else(|| CdcErr::format("node", "missing child pointer"))?;
        let (new_child, removed) = self.remove_rec(io, alloc, child, key)?;

        match new_child {
            Some(child_page) => node.children[pos] = child_page,
            None => {
                node.children.remove(pos);
                if pos > 0 {
                    node.entries.remove(pos - 1);
                } else if !node.entries.is_empty() {
                    node.entries.remove(0);
                }
            }
        }

        if node.children.is_empty() {
            self.drop_slot(page)?;
            return Ok((None, true));
        }
        if node.entries.is_empty() {
            // Only one child left: collapse this level.
            let only = node.children[0];
            self.drop_slot(page)?;
            return Ok((Some(only), removed));
        }

        if !fits::<C>(&node) {
            return Err(CdcErr::EntryTooLarge);
        }
        self.stage_slot(page, node)?;
        Ok((Some(page), removed))
    }

    fn scan_rec(
        &self,
        io: &impl PageRead,
        idx: PageIdx,
        start: Option<&C::Key>,
        end: Option<&C::Key>,
        visit: &mut impl FnMut(&C::Key, &C::Value),
    ) -> Result<()> {
        let node = self.peek(io, idx)?;

        if node.level == 0 {
            for (key, value) in &node.entries {
                if start.is_some_and(|start| key < start) {
                    continue;
                }
                if end.is_some_and(|end| key > end) {
                    break;
                }
                visit(key, value);
            }
            return Ok(());
        }

        for (i, child) in node.children.iter().enumerate() {
            if i > 0
                && let Some(end) = end
                && node.entries[i - 1].0 > *end
            {
                // The separator is a lower bound for this child; if it is past
                // the end of the range, every later child is too.
                break;
            }
            self.scan_rec(io, *child, start, end, visit)?;
        }
        Ok(())
    }
}

impl<C: Codec> std::fmt::Debug for BTree<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BTree")
            .field("root", &self.root)
            .field("cached_nodes", &self.cache.slots.len())
            .finish()
    }
}

const MAX_LEVEL: u8 = 32;

/// A copy-on-write node together with the page holding this transaction's copy.
type Loaded<C> = (PageIdx, Node<<C as Codec>::Key, <C as Codec>::Value>);

/// Whether a node's entries and child pointers fit in one frame.
fn fits<C: Codec>(node: &Node<C::Key, C::Value>) -> bool {
    if node.entries.len() > u16::MAX as usize {
        return false;
    }
    let entries_len: usize = node
        .entries
        .iter()
        .map(|(key, value)| C::entry_size(key, value))
        .sum();
    HEADER_LEN + entries_len + 4 * (node.entries.len() + 1) <= PAYLOAD_SIZE
}

fn pack_node<C: Codec>(node: &Node<C::Key, C::Value>) -> Result<Option<Payload>> {
    let count = node.entries.len();
    if !fits::<C>(node) {
        return Ok(None);
    }

    let mut entries = Vec::new();
    for (key, value) in &node.entries {
        C::encode_entry(&mut entries, key, value)?;
    }

    let children_len = 4 * (count + 1);
    if HEADER_LEN + entries.len() + children_len > PAYLOAD_SIZE {
        return Ok(None);
    }

    if node.level > 0 && node.children.len() != count + 1 {
        return Err(CdcErr::format(
            "node",
            "internal node has wrong child count",
        ));
    }
    if node.level == 0 && !node.children.is_empty() {
        return Err(CdcErr::format("node", "leaf has children"));
    }

    let mut payload = [0u8; PAYLOAD_SIZE];
    let header = NodeHeader {
        magic: NODE_MAGIC,
        kind: C::KIND,
        level: node.level,
        count: count as u16,
        entries_len: entries.len() as u16,
        reserved: [0; 24],
    };
    payload[..HEADER_LEN].copy_from_slice(header.as_bytes());
    payload[HEADER_LEN..HEADER_LEN + entries.len()].copy_from_slice(&entries);

    let children_off = PAYLOAD_SIZE - children_len;
    for (i, child) in node.children.iter().enumerate() {
        let off = children_off + 4 * i;
        payload[off..off + 4].copy_from_slice(&child.to_u32().to_le_bytes());
    }

    Ok(Some(payload))
}

fn read_node<C: Codec>(io: &impl PageRead, idx: PageIdx) -> Result<Node<C::Key, C::Value>> {
    let payload = io.read_payload(idx)?;
    let header = NodeHeader::read_from_bytes(&payload[..HEADER_LEN])
        .map_err(|_| CdcErr::format("node", "short header"))?;

    if header.magic != NODE_MAGIC {
        return Err(CdcErr::NotATreePage(idx.to_u32()));
    }
    if header.kind != C::KIND {
        return Err(CdcErr::format("node", "wrong tree kind"));
    }
    if header.level > MAX_LEVEL {
        return Err(CdcErr::format("node", "level out of range"));
    }

    let count = usize::from(header.count);
    let children_len = 4 * (count + 1);
    if HEADER_LEN + children_len > PAYLOAD_SIZE {
        return Err(CdcErr::format("node", "count overflows payload"));
    }
    let children_off = PAYLOAD_SIZE - children_len;
    let entries_end = HEADER_LEN
        .checked_add(usize::from(header.entries_len))
        .ok_or_else(|| CdcErr::format("node", "entry length overflows"))?;
    if entries_end > children_off {
        return Err(CdcErr::format("node", "entries overlap child pointers"));
    }

    let entries = C::decode_entries(&payload[HEADER_LEN..entries_end])?;
    if entries.len() != count {
        return Err(CdcErr::format("node", "entry count mismatch"));
    }

    let level = header.level;
    let mut children = Vec::new();
    if level > 0 {
        for i in 0..=count {
            let off = children_off + 4 * i;
            let raw = u32::from_le_bytes(
                payload[off..off + 4]
                    .try_into()
                    .expect("child slot is 4 bytes"),
            );
            let child = PageIdx::try_new(raw)
                .ok_or_else(|| CdcErr::format("node", "null child pointer"))?;
            children.push(child);
        }
    }

    Ok(Node { level, entries, children })
}

// -- chunk tree ------------------------------------------------------------

/// Keyed BLAKE3 identity of a chunk's plaintext.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ChunkKey(pub [u8; 32]);

impl ChunkKey {
    #[inline]
    pub fn new(keys: &crate::crypto::VolumeKeys, plaintext: &[u8]) -> Self {
        Self(keys.chunk_id(plaintext))
    }
}

/// Immutable placement record for a chunk. Liveness is not tracked here; GC
/// derives it from manifests so a deduplicated write never conflicts on this
/// entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChunkMeta {
    pub start: u32,
    pub frames: u32,
    pub raw_len: u32,
}

impl ChunkMeta {
    pub const ENCODED_LEN: usize = 12;
    pub const ENTRY_LEN: usize = 32 + Self::ENCODED_LEN;

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.start.to_le_bytes());
        out.extend_from_slice(&self.frames.to_le_bytes());
        out.extend_from_slice(&self.raw_len.to_le_bytes());
    }

    fn decode(buf: &[u8]) -> Self {
        Self {
            start: u32::from_le_bytes(buf[0..4].try_into().expect("4 bytes")),
            frames: u32::from_le_bytes(buf[4..8].try_into().expect("4 bytes")),
            raw_len: u32::from_le_bytes(buf[8..12].try_into().expect("4 bytes")),
        }
    }
}

pub struct ChunkCodec;

impl Codec for ChunkCodec {
    type Key = ChunkKey;
    type Value = ChunkMeta;
    const KIND: u8 = 1;

    fn decode_entries(buf: &[u8]) -> Result<Vec<(ChunkKey, ChunkMeta)>> {
        if !buf.len().is_multiple_of(ChunkMeta::ENTRY_LEN) {
            return Err(CdcErr::format("chunk node", "ragged fixed entries"));
        }
        let (entries, _padding) = buf.as_chunks::<{ ChunkMeta::ENTRY_LEN }>();
        Ok(entries
            .iter()
            .map(|entry| {
                let key: [u8; 32] = entry[..32].try_into().expect("32-byte key");
                (ChunkKey(key), ChunkMeta::decode(&entry[32..]))
            })
            .collect())
    }

    fn encode_entry(out: &mut Vec<u8>, key: &ChunkKey, value: &ChunkMeta) -> Result<()> {
        out.extend_from_slice(&key.0);
        value.encode(out);
        Ok(())
    }

    fn entry_size(_key: &ChunkKey, _value: &ChunkMeta) -> usize {
        ChunkMeta::ENTRY_LEN
    }

    fn empty_value() -> ChunkMeta {
        ChunkMeta::default()
    }
}

// -- path tree -------------------------------------------------------------

/// UTF-8 path bytes, `/`-separated. Byte ordering is the listing order.
pub struct PathCodec;

impl Codec for PathCodec {
    type Key = Vec<u8>;
    type Value = Vec<u8>;
    const KIND: u8 = 2;

    fn decode_entries(buf: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            if i + 4 > buf.len() {
                return Err(CdcErr::format("path node", "truncated entry header"));
            }
            let key_len = usize::from(u16::from_le_bytes([buf[i], buf[i + 1]]));
            let value_len = usize::from(u16::from_le_bytes([buf[i + 2], buf[i + 3]]));
            i += 4;
            if i + key_len + value_len > buf.len() {
                return Err(CdcErr::format("path node", "truncated entry body"));
            }
            let key = buf[i..i + key_len].to_vec();
            i += key_len;
            let value = buf[i..i + value_len].to_vec();
            i += value_len;
            out.push((key, value));
        }
        Ok(out)
    }

    fn encode_entry(out: &mut Vec<u8>, key: &Vec<u8>, value: &Vec<u8>) -> Result<()> {
        let key_len = u16::try_from(key.len()).map_err(|_| CdcErr::EntryTooLarge)?;
        let value_len = u16::try_from(value.len()).map_err(|_| CdcErr::EntryTooLarge)?;
        out.extend_from_slice(&key_len.to_le_bytes());
        out.extend_from_slice(&value_len.to_le_bytes());
        out.extend_from_slice(key);
        out.extend_from_slice(value);
        Ok(())
    }

    fn entry_size(key: &Vec<u8>, value: &Vec<u8>) -> usize {
        4 + key.len() + value.len()
    }

    fn empty_value() -> Vec<u8> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_size_matches_the_encoder() {
        let mut buf = Vec::new();
        let key = ChunkKey([7; 32]);
        let value = ChunkMeta { start: 1, frames: 2, raw_len: 3 };
        ChunkCodec::encode_entry(&mut buf, &key, &value).unwrap();
        assert_eq!(buf.len(), ChunkCodec::entry_size(&key, &value));

        let mut buf = Vec::new();
        let key = b"a/b".to_vec();
        let value = b"value".to_vec();
        PathCodec::encode_entry(&mut buf, &key, &value).unwrap();
        assert_eq!(buf.len(), PathCodec::entry_size(&key, &value));
    }
}
