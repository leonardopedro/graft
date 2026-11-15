# CDC Store on Graft: FastCDC content-addressed volume — design plan

Status: Phases 1–5 implemented — `crates/cdc-volume` for the engine, `crates/cdc-cli` for the
`cdc` binary (52 tests passing). Measured end to end against a real S3-compatible server, and a
killed push is covered by a real `SIGKILL` test. Every Graft fact below was checked against this checkout
(`crates/graft/src/**` at `b07d931`), with the file named where it matters.

Scope: a deduplicating, encrypted, lazily-read blob store built directly on Graft's page API,
with no SQLite and no per-file read amplification beyond one chunk.

## 1. What changed from the first draft

The draft was directionally right — variable-size chunks over fixed 4 KiB pages, copy-on-write
index pages, per-page AEAD — but several details cannot work as written. These are corrected
below rather than carried forward.

| Draft claim | Reality / correction |
| --- | --- |
| "Page 0: Volume Superblock" | `PageIdx` is `NonZero<u32>`, so **page 0 does not exist** (`core/pageidx.rs:34`). The superblock lives at `PageIdx::FIRST` (page 1). |
| Chunks "padded to a multiple of 4096"; later "align to multiples of 4056" | Contradictory, and neither is needed. A chunk occupies `ceil(raw_len / 4056)` pages with only the tail padded. Alignment is already guaranteed because a chunk always starts a fresh page. |
| AAD = `PageIdx + generation` | The reader cannot know a page's write generation, so it cannot rebuild that AAD. Use AAD that is derivable at read time: format tag + `PageIdx`. Replay across volumes is prevented by per-volume keys instead. |
| Salt/KDF parameters stored in the superblock | Chicken-and-egg: the superblock is encrypted. Derive the salt from the public `VolumeId` instead, and fix the KDF parameters per format version. |
| File index keyed by `blake3(path)` | Then `ls` and prefix queries are impossible, and a rename is a rewrite. Key the path tree by **path bytes, ordered**, which also gives directory listing for free. |
| "3 levels for millions of chunks" | With ~80 keys/node a 3-level tree holds ~0.5M chunk entries; 10M needs 4 levels and 4–5 page reads per lookup. Still cheap, but do not size the fanout on the wrong assumption. |
| `refcount` in `ChunkMeta`, bumped on every hit | Turns a deduplicated write into a write-write conflict on the chunk index, and churns CoW pages. Make `ChunkMeta` **immutable**; derive liveness during GC by walking manifests. |
| "Serializable Snapshot Isolation ... retry `commit()`" | Retrying the commit is not enough: the writer is bound to a snapshot, and a stale one is rejected with `LogicalErr::VolumeConcurrentWrite` (`local/fjall_storage.rs:687`). The whole logical mutation must be rebuilt from a fresh writer. |

Also worth keeping from the draft: CoW index pages, deterministic keyed chunk IDs, per-page
encryption, and 4 KiB-aligned chunk storage. Those hold up.

## 2. Graft API surface this design relies on

Verified signatures, so the plan can be implemented without guesswork:

- `Runtime::volume_writer(vid) -> Result<VolumeWriter>`, `Runtime::volume_reader(vid) ->
  Result<VolumeReader>` (`rt/runtime.rs:208`).
- `VolumeRead::{snapshot, page_count, read_page}`; `read_page` returns a whole 4096-byte
  `Page` (`volume_reader.rs`).
- `VolumeWriter::write_page(idx, page)` only stages pages in a `BTreeMap`; `commit(self)` writes
  them as one segment and returns a reader (`volume_writer.rs`). All dirty pages of a transaction
  are therefore resident in memory until commit.
- `commit` refuses a writer whose snapshot is no longer latest (`VolumeConcurrentWrite`).
- `VolumeWriter::soft_truncate(PageCount)` changes the logical page count without writing pages.
- `Page` is exactly 4096 bytes (`PAGESIZE` in `core/page.rs`), constructible from `&[u8]` or
  `Bytes`, and `AsRef<[u8]>` for reading.
- `PageIdx::saturating_next()` / `saturating_add(n)` and `PageIdx::pages()` give the range helpers
  an allocator needs, with no zero-index hole.
- `Runtime::volume_open(Option<VolumeId>, Option<LogId>, Option<LogId>)` creates the volume if it
  is absent locally, and **mints a fresh random remote `LogId`** when none is passed
  (`local/fjall_storage.rs:622`). A second machine therefore cannot attach by volume id alone: the
  remote log id is part of what has to be shared (§9 Phase 4).
- `Runtime::volume_pull(vid)` = `fetch_log(volume.remote)` + `sync_remote_to_local`, and the sync is
  just a `SyncPoint` update (`local/fjall_storage.rs:875`) — pulls move no pages, which is what keeps
  an edge lazy.
- `Runtime::volume_get(&vid)` returns the `Volume`, whose `remote` field is the log id `cdc share`
  prints.
- Setup for tests: `setup_graft(GraftConfig { remote, data_dir, autosync })` builds a runtime over
  local fjall storage plus a remote.

## 3. Crates

Add two members to the existing workspace instead of a separate nested workspace, so
`cargo test` at the repo root covers the new code and the shared `Cargo.lock` stays consistent:

```text
crates/
├── cdc-volume/        # engine: layout, crypto, allocator, CoW B-tree, chunker, store, gc
│   └── src/{lib,layout,crypto,superblock,alloc,io,btree,chunker,store,gc,volume}.rs
└── cdc-cli/           # init, put, get --offset/--length, ls, stat, rm, sync, pull, share, attach, gc
    └── src/{lib,main}.rs
```

Dependencies: `graft` (path), `fastcdc` v3, `blake3`, `chacha20poly1305`, `hkdf`, `sha2`,
`zerocopy`, `thiserror`, `clap`, `tracing`. `tokio` is not needed in the engine: Graft's runtime
owns its own event loop, and the engine is synchronous.

## 4. On-volume layout

The volume is one flat page array. Page 1 is the superblock; everything else is either an index
node, a chunk frame, or a manifest page. Every page is a 4096-byte AEAD frame (§5), so the
*logical* page is 4056 bytes of payload.

### 4.1 Superblock (page 1)

Fits comfortably in 4056 bytes, `#[derive(FromBytes, IntoBytes, KnownLayout, Immutable)]`,
little-endian:

| field | type | notes |
| --- | --- | --- |
| `magic` | `[u8; 8]` | `b"GRAFTCDC"` |
| `format_version` | `u16` | starts at 1; dictates KDF and frame layout |
| `page_bytes` | `u16` | 4056, asserted against the constant |
| `generation` | `u64` | app-level commit counter |
| `chunk_root` | `u32` | `PageIdx` of the chunk index root |
| `path_root` | `u32` | `PageIdx` of the path index root |
| `next_page` | `u32` | bump watermark; pages below it are allocated or free-listed |
| `free_head` | `u32` | head of the free-page chain, 0 for none |
| `chunk_count` | `u64` | live chunk entries, for accounting only |
| `file_count` | `u64` | live manifests |
| `live_bytes` | `u64` | sum of `raw_len` over live chunks |

`next_page` is a watermark, not an allocation bitmap: Graft pages are never reclaimed in place,
so a free page is one whose index is below `next_page` and which no live B-tree references.

### 4.2 Chunk frames

A chunk of `raw_len` bytes is split into `ceil(raw_len / 4056)` consecutive payloads and written
to consecutive `PageIdx`s. `ChunkMeta` records start and count; reads decrypt the run in order
and truncate the last page to `raw_len`. Nothing is shared between pages of different chunks, so
reading one chunk touches only its own pages.

### 4.3 Index nodes

Both trees use the same CoW node format: a 32-byte header (`magic`, `kind`, `level`,
`count: u16`, `entries_len: u16`), then `count` entries, then `count + 1` child page pointers
stored at the tail of the payload (0 for leaves). `entries_len` is required because
variable-length entries leave padding before the child pointers, so the entry region cannot be
derived from `count` alone.
Chunk entries are fixed size (key 32 + start 4 + frames 4 + raw_len 4 = 44 bytes), which gives
83 keys per node and 4 levels at 30M chunks. Path entries are variable length, so the path
tree stores `(key_len, key, value)` and splits on entry count and byte budget.

Internal node keys are lower bounds: `key[i]` is a lower bound for `child[i + 1]`. Deleting the
minimum only raises a subtree's true minimum, which leaves the stored lower bound conservatively
low and therefore safe. Inserting below a subtree minimum lowers it, so the insert path rewrites
the separator it just invalidated and propagates a new minimum upward.

Path keys are the UTF-8 path bytes with `/` as separator, so an in-order scan is a lexicographic
listing and `ls dir/` is a range scan over `dir/`. Values are a version byte, then
`file_size: u64, mtime_ns: u64, chunk_count: u32`, then either the inline refs or a recipe pointer:

```text
inline:  (chunk_hash: [u8;32], raw_len: u32) * min(chunk_count, 48)   // 36 bytes each
overflow: recipe_page: u32, recipe_pages: u32, recipe_len: u32
```

48 inline refs (1728 bytes) keep small files to a single tree entry. Recipe pages are ordinary
encrypted frames carrying 112 refs (4032 bytes) each. Each ref stores the chunk's plaintext length
alongside its hash: that is what lets a range read pick the overlapping chunks from the recipe page
alone, without looking up every earlier chunk in the chunk tree.

## 5. Encryption and integrity

One key hierarchy, no secrets in Graft metadata:

```text
master_key (32B, from key file or env)
  └─ HKDF-SHA256(salt = VolumeId bytes (16-byte GID), info = "graft-cdc v1")
       ├─ chunk_id_key  (BLAKE3 keyed hashing of plaintext chunks)
       └─ page_key      (XChaCha20-Poly1305 per page)
```

Deriving the salt from the public `VolumeId` is what makes the encrypted superblock readable on a
cold edge; a random salt would have to live somewhere in the clear. Passphrase-protected volumes
run Argon2id over the passphrase *before* this step, with parameters fixed by `format_version`.

Page frame, exactly 4096 bytes:

```text
0..24     nonce, 24 random bytes, fresh on every page write
24..4080  ciphertext of the 4056-byte payload
4080..4096 Poly1305 tag
```

AAD is `b"GRAFTCDC1" || page_idx.to_be_bytes()`, both derivable from the read location. That binds
a frame to its slot, so a page cannot be relocated or swapped between positions in the same volume.
Cross-volume replay is out of scope for the AAD because the page key is volume-specific. Binding a
generation would require storing the generation in cleartext per page, which costs payload for a
property the key hierarchy already provides.

Per-page random nonces are safe here: 192-bit nonces, and each page is written at most once
because writes are CoW. The chunk ID is `blake3::keyed_hash(&chunk_id_key, plaintext)`, so dedup
works without exposing a plaintext-correlatable hash to the remote, and re-encrypting an identical
chunk with a new nonce does not break dedup, because identity lives in the index key.

Note the deliberate consequence: identical chunks across files share a `chunk_hash`, so an observer
of the index can tell that some chunk repeats. That is inherent to dedup and should be stated in
the threat model.

## 6. Transactions

`VolumeWriter` is snapshot-bound, so a write path is a closure that is replayed wholesale on
conflict rather than a retried commit:

```rust
loop {
    let writer = rt.volume_writer(vid.clone())?;          // fresh snapshot
    let mut txn = Txn::open(writer, &keys)?;              // reads + verifies superblock
    match txn.run(&mut mutation) {                        // stages pages, rewrites superblock
        Ok(v) => break v,
        Err(Mutation::Conflict) => continue,              // VolumeConcurrentWrite
    }
}
```

Rules that keep replay cheap and correct:

- The mutation is a pure function of its input bytes and the transaction's reads; every replay
  sees the newer snapshot and must reach the same logical result.
- Only the pages of one path-mutation are copied; untouched subtrees are shared with the old root.
- Nothing is written to chunk frames until the chunk is known to be absent from the index, and the
  index entry is inserted in the same transaction as its frames.

Large uploads are committed in bounded batches rather than one enormous writer (a 500 GB file must
not become a 500 GB `BTreeMap`). Chunk batches commit first, manifest last:

1. Commit chunk frames plus their chunk-index entries in batches of ≤64 MiB of staged pages.
2. Walk the path tree in one final short transaction and insert the manifest.

A crash between 1 and 2 leaves unreachable chunk entries and frames: wasted space, never a partial
file. GC reclaims them, which is why liveness must be reachability-based (§8) rather than
refcount-based.

## 7. Operations

`put_file(path, reader)` — read the superblock, stream `fastcdc::v2020::StreamCDC` with
`min=8192, avg=32768, max=131072`, key-hash each chunk, keep an in-transaction
`HashMap<ChunkHash, ChunkMeta>` so repeats inside the same upload are written once, look absent
chunks up in the index, allocate `ceil(len/4056)` pages from the allocator, encrypt, stage. Then
write the manifest and superblock.

`get_file(path)` / `read_range(path, range)` — read page 1, search the path tree (1–5 page reads),
read the recipe (inline or its pages), then resolve only the chunks the range overlaps through the
chunk tree and decrypt those. Offsets come from the recipe's `raw_len` fields, so chunks before the
range need no chunk-tree lookup at all; skipping inside a chunk is `raw_len` arithmetic. No
whole-file download, no full-index scan.

`delete_file(path)` — remove the path entry and rewrite the superblock. Chunk entries and frames
stay until GC, because other files may still reference the same chunk hash.

`ls(prefix)` — range scan over the path tree.

`attach(vid, log, remote)` — register the volume in an empty local index, pull the remote log, open
the store (which reads page 1) and only then write the state file. Every later read is lazy: nothing
but the pages a command touches is ever fetched.

## 8. Space reclamation

Chunk entries are immutable, so dead chunks are only discovered by reachability: mark from all
manifests, sweep everything else. Because Graft never reclaims pages inside a volume, sweep means
rewrite:

1. Open a read snapshot and mark live pages by walking both trees and every manifest: node pages,
   recipe pages, and the frames of chunks a manifest references. Chunks no manifest mentions are
   counted as `dead_chunk_frames` and dropped.
2. Create a fresh volume and copy each live chunk (re-keyed for the new volume, see Phase 5), then
   rebuild both trees and rewrite each manifest with the new chunk ids and its recipe's new pages.
3. Verify by re-marking the new volume: the live file, chunk and byte counts must match the source
   exactly, otherwise the rewrite fails and no swap happens. Then swap the pointer (the CLI renames
   its state file) and keep the old volume for a grace period.

In-place compaction (rewriting pages behind the same superblock) is rejected: it doubles the
failure surface of an interrupted run and cannot be made atomic, while a volume swap is a single
tag write.

Allocator: `next_page` bump allocation with first-fit reuse from `free_head`. CoW means freed
index pages are never immediately reusable by the transaction that freed them, so the chain is only
consumed by later transactions; if that turns out too coarse, GC's rewrite is the fallback.

## 9. Phases and acceptance tests

Phase 1 — frame, crypto, allocator. **Implemented** (`src/{crypto,superblock,alloc,io,volume}.rs`).
`PageCipher` with the §5 frame, zerocopy superblock at page 1, bump allocator with a sorted
first-fit free chain that can satisfy contiguous runs. Accept: write pages, commit, read back via a
second reader; flip one ciphertext byte and one page index and assert both are rejected; assert a
fresh runtime can open the volume with only `VolumeId` and the master key. All covered in
`tests/phase1.rs`, plus free-list reuse across transactions.

Phase 2 — CoW B-tree. **Implemented** (`src/btree.rs`, `src/volume.rs`). `get/insert/remove` with
path copying, chunk and path node layouts, range scan, conflict replay loop in `CdcVolume::update`,
and frame-read counters on `FrameIo`. Accept: 100k random chunk keys over 10 commits, point lookups
read ≤5 pages (3 in practice, the tree height for 100k keys is 2), a reader pinned before a commit
still sees the old tree, and a second writer on a stale snapshot gets `VolumeConcurrentWrite` and
succeeds on replay. All covered in `tests/phase2.rs`.

Implementation notes worth carrying forward:

- Nodes are written per transaction, not per operation: the first touch of a node allocates its CoW
  page immediately, but the frame is sealed once by `BTree::commit`. That took the 100k-key test
  from 206 s to 19 s by removing three full-frame encryptions per insert.
- `BTree::commit` is mandatory, because the new root points at pages that only exist in the
  transaction cache until it runs.
- The free list is consumed by later transactions only, since the allocator's state is loaded at
  transaction start and persisted at commit.

Phase 3 — chunker and store. **Implemented** (`src/chunker.rs`, `src/store.rs`).
`put_file/get_file/read_range/delete_file/ls/stat`, 32 MiB chunk batches with manifests written
last, 48-ref inline manifests with recipe-page overflow, and a `Snapshot` whose frame reads are
counted. `fastcdc` v2020 with min/avg/max = 8/32/128 KiB. Accept: two images with ~90 % shared
content grow the volume by roughly their new content; a range read touches only the chunks it
overlaps. Measured in `tests/phase3.rs`:

  | Case | Result |
  | --- | --- |
  | 16 MiB image, re-uploaded with a 4 KiB insertion and a rewritten 512 KiB tail | 169 new pages, 16 of 405 chunks new (~4 % growth) |
  | 10 KiB read from a 4 MiB file | 19 frame reads out of 1035 data frames |
  | 4 KiB read from a 3 MiB file with recipe overflow | 11 frame reads |

  The 500 MiB/50 MiB acceptance scales from the same ratio; the test runs 16 MiB so it stays under
  ten seconds.

Phase 4 — remote and lazy edge. **Implemented** except the real-S3 cold run. `crates/cdc-cli`
provides the `cdc` binary: `init`, `put`, `get --offset/--length` (or `-o FILE`), `ls [PREFIX]`,
`stat`, `rm`, `sync`, `pull`, `share`, `attach --vid --log`, `gc`. Remote specs are `memory`,
`fs:DIR`, and `s3:BUCKET[/PREFIX]`; the S3 path uses Graft's S3-compatible remote, so credentials,
region and endpoint come from the standard `AWS_*` environment variables (including
`AWS_ENDPOINT_URL` for MinIO/R2).

Local state is two files in `--data` (default `./cdc-data`): `volume.key` (32-byte hex, mode 0600)
and `cdc-volume` (volume id, remote spec, key path), so later commands only need `--data`. `sync`
pushes local commits to the remote and `pull` fast-forwards the local volume to the remote's latest
LSN (`Runtime::volume_pull`). Neither moves pages: `pull` only advances the sync point, and page
reads fetch segments on demand, so an edge stays lazy.

Attaching a second machine needs **two ids**, which is why `share` exists: `volume_open` mints a new
remote `LogId` when none is given, so opening by volume id alone would attach to an empty log.
`cdc share` prints the volume id, the remote log id, the remote spec and the key path; `cdc attach
--vid --log --remote --key` registers that pair on a fresh data directory, pulls, and then proves
the key works by reading the superblock. A wrong key leaves no state file behind. `gc` rewrites into
a fresh volume and renames the state file over the old one; see Phase 5.

`pull` (and attach's pull step) retries four times with 50/100/200 ms backoff. Attach is the one
moment an edge has no local copy to fall back on, so a throttled or briefly unreachable bucket must
not end the setup; `volume_pull` only moves the sync point, so repeating it is safe. When the remote
stays down the command still fails, naming the attempt count rather than the last transient error
alone, and attach writes no state file.

Accept: cold cache, a 10 KiB range from a huge file downloads no more than the measured frame floor
(§10): one 262 KiB data frame plus whatever the index path costs. Test against the measured numbers,
not the original <100 KiB guess.

Measured two ways. First, page reads on an `fs` remote with an empty edge cache
(`tests/phase4_edge.rs`): a 10 KiB range at 2 MiB of a 4 MiB file costs **19 page reads**, the same
figure the earlier read-path measurement produced.

Second, **bytes on the wire against a real S3-compatible server**, which is what the budget is
actually about. Graft reads a page as an HTTP range request for the frame containing it, so the
proxy log shows one `206` per frame and the response size is the whole frame. Setup: `s3rver`
(file-backed S3 API) behind a request-logging HTTP proxy, origin in one data directory writing a
4 MiB random file and pushing, edge in a **fresh** data directory with its own empty index,
`AWS_ENDPOINT_URL` pointing at the proxy. The whole run is `graft/scripts/s3-wire-measure/measure.sh`
(`npm install s3rver`, then one command), so the table can be reproduced; the chunk count moves
between runs (113–115 chunks on the same 4 MiB), which moves the numbers slightly.

  | Step | S3 requests | Bytes down | What it is |
  | --- | --- | --- | --- |
  | origin push | 2 | – | one ~4.5 MB segment `PUT` + one 266 B log `PUT` (≈4.5 MB up) |
  | `cdc attach` | 7 | 263,596 | 1 frame (262,163 B) + log head 288 B + five `404` LSN probes, 1,110 B |
  | `cdc get --offset 2 MiB --length 10 KiB` | 2 | 282–287 KB | one 262,163 B data frame + a 20 KB index frame |
  | same `get` again, cache warm | 0 | 0 | the fetched pages are now local |

So a cold attach plus a 10 KiB range read is **~0.55 MB down in 9 requests**, and the read alone is
~285 KB for 10,240 useful bytes. The largest single frame observed is 262,163 B, exactly the §10
figure, and the frame holding the index shows a segment's last frame is only as big as it needs to
be. `attach` reading exactly one frame is deliberate: it verifies the key by decrypting page 1 and
nothing else, because walking the file index there would download the whole path tree on a store
with many files (the first version of `attach` did exactly that and cost 3 frames, 562,706 B, for
an index walk nobody asked for).

### Index placement is the lever, not the commit

§10 predicted that pushing index pages in small commits would cut the index path to ~3 × 16 KiB.
That is not how Graft uploads: `RemoteCommit` collects *every* outstanding page across the pushed
LSNs, sorts them by page index and frames that stream 64 pages at a time (`rt/action/remote_commit.rs
::build_segment`). One push is one segment, so local commit boundaries do not set frame boundaries —
page order does.

What did make the difference is where the index pages sit. `put_file` used to insert chunk entries
as it wrote data, so index pages interleaved with data pages; now `flush_chunks` writes only data
and `flush_chunk_index` inserts the batch's entries sorted, after its data pages exist. Sorted
insertion only splits the rightmost leaf, so the whole index block (chunk tree pages, the recipe
pages and the path entry, ~6–7 pages) is contiguous at the end of the segment. A 10 KiB range read
then needs its data frame plus one small index frame instead of two full frames and a tail:

  | Layout | Requests | Bytes down |
  | --- | --- | --- |
  | index interleaved with data | 3 | 532–578 KB |
  | index written after the data | 2 | 277–320 KB |
  | …and pushed to the next frame boundary (12 runs) | 2 | **282–287 KB** |

Contiguity alone still lost the third request in ~1 run in 8: a 64-page frame boundary landing
inside the 5–7 page index block splits it over two frames. Frames begin at the segment's first page,
which is always the superblock — every commit rewrites page 1 — so the grid is fixed at pages 1, 65,
129, … regardless of what else the push contains. `flush_chunk_index` therefore pads with filler
pages until the index block starts a frame (`frame_pad`), and the read is one data frame plus one
small index frame every time. Filler pages are encrypted, so they do not compress: `PAD_RATIO`
caps the waste at 1 page per 8 the batch wrote, which keeps a 4 MiB upload to a few hundred KB of
padding at worst and a 40 KiB file to none at all (its index shares its data's frame anyway).
`tests/index_frame.rs` asserts the invariant directly: the chunk root and path root must live in a
last frame that begins at `1 + 64k` and is smaller than a full frame.

Index writes stay per batch, not per file: a committed batch's entries let a retry after a failed
upload deduplicate what already reached the volume
(`interrupted_upload_leaves_only_garbage` covers it).

Two costs are visible in the log that the page-read counter hid:

- Attaching probes LSNs backwards from the log head, and each absent one is a `404`. Here 5 × 222 B
  = 1,110 B, bounded by the log's length, so it does not grow with volume size.
- The index block is 5–7 pages and now always starts a frame, so it costs exactly one request. The
  padding that guarantees it is write-side only: a reader never touches a filler page.

### Latency

No cloud credentials exist on this machine and the catalog has no provisionable S3-compatible
store, so the cloud half is measured against a **modelled** RTT: the proxy takes `DELAY_MS` of
one-way delay in each direction. Same 4 MiB file, same two requests, wall-clock for the process:

  | One-way delay | RTT | `attach` (7 requests) | cold 10 KiB read (2 requests) | warm read (0 requests) |
  | --- | --- | --- | --- | --- |
  | 0 ms (loopback) | ~0 | 382 ms¹ | 93 ms | 67 ms |
  | 60 ms | 120 ms | 772 ms | 345 ms | 76 ms |

¹ Includes process start, Graft setup and the 262 KB frame transfer; the fixed cost is ~250 ms.

The deltas follow the request graph, not the byte count: a cold read is two dependent fetches, so
it costs ~2 RTT (252 ms over loopback at 120 ms RTT). Attach is ~3 sequential hops — log head, then
the `404` probes in parallel, then the superblock frame — so it costs ~3 RTT.

**Attach's three hops are all load-bearing**, which was worth checking rather than assuming: with
its `pull` removed, attach fails with `page 1 is outside the volume` — the head commit is only
persisted locally by the pull, so the superblock read cannot even address page 1 without it. That
leaves the probe fan (one hop of five parallel `404`s: Graft lists a log by scanning LSNs and stops
at the first gap, and there is no object in S3 to ask for a newest-commit pointer) and the frame
fetch. Getting to two hops needs a log-format change in graft — a parent-LSN pointer or a mutable
`head` object per log — not a change in the CDC layer. The bytes are a separate, cheaper win: page 1
shares frame 0 with the push's first 63 data pages, so attach downloads 262 KB to read 4 KiB of
superblock; pushing the superblock in a small commit of its own would cut that to ~4 KB.

The real-bucket run is one command once credentials exist:

  ```sh
  AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=… \
    AWS_ENDPOINT_URL=https://… scripts/s3-wire-measure/measure.sh 4194304
  ```

That prints the same per-request byte table plus timings; what it would add over the model is
real transfer time for the 262 KB frame (bandwidth, not just RTT) and TLS handshakes.

Phase 5 — GC and resilience. **Implemented** (`src/gc.rs`, `cdc gc`). `gc::stats` marks reachable
pages from the superblock; `gc::rewrite` copies the live set into a fresh volume and the caller
swaps to it.

Two deviations from the sketch above, both forced by the checkout:

- **No `tag_replace` for the swap.** Graft tags live in the local fjall index only
  (`local/fjall_storage.rs`; nothing in `remote/` mentions tags), so a tag write is a local pointer
  with no remote meaning. The CLI instead writes the new volume id to `cdc-volume.tmp` and renames
  it over `cdc-volume`, which is the same atomic local switch without a second indirection. The old
  volume is kept for a grace period and named in the output.
- **The copy is a re-keying.** Chunk ids are keyed BLAKE3 under keys derived from the `VolumeId`
  (`crypto.rs::chunk_id`), so copying a chunk into a new volume would give it a different id than
  every copied manifest expects. GC therefore recomputes each chunk's id under the target volume's
  keys and rewrites every manifest reference, which keeps dedup working after the swap (test:
  `dedup_survives_the_rewrite`) and makes GC the key-rotation path. The copy also verifies each
  chunk's plaintext against its source id and refuses to rewrite a volume it cannot account for.

`gc::rewrite` sets the superblock's `chunk_count`, `file_count` and `live_bytes` from the mark, so
those fields are exact again after a GC — before it, they are stale (§10).

Accept, measured in `tests/phase5.rs`:

  | Case | Result |
  | --- | --- |
  | 2 files deleted/overwritten, 3 MiB live | 1649 pages (822 live) → 819 pages (819 live); 823 dead frames dropped; 83 chunks / 3 MiB copied |
  | upload interrupted after 34 MiB of a 34 MiB stream | 8698 pages, 8681 dead frames → 1 page (superblock only); no file entry ever existed |
  | dedup after the rewrite | storing the same 4 MiB content again adds 0 chunks and 0 live bytes |
  | one flipped ciphertext byte | `CdcErr::Auth(page)`; GC refuses to copy the volume |

A push that fails is covered from the CLI side: `a_failed_push_leaves_the_local_volume_intact`
points `sync` at an unreachable remote and asserts the local volume still serves its files and a
later sync succeeds. Conflict replay is the `CdcVolume::update` loop exercised in Phase 2.

A process **killed mid-push** is covered too, without a fault layer:
`a_push_killed_mid_upload_leaves_the_remote_without_a_partial_file` runs a real `cdc sync` as a
child process in an 8 MiB incompressible upload, waits for the first object to appear under the
remote and `SIGKILL`s it. The commit object is written only after its segment, so the interrupted
file never becomes visible: a fresh edge attached to that remote lists the previously pushed file
and nothing else, the origin's local volume still serves the large file, and a retried `sync`
followed by `pull` gives the edge the complete content. This is a timing race by construction, but
in the safe direction — the kill is triggered by the first segment byte hitting disk, and it would
take a push that finishes 8 MiB between two 2 ms polls to lose.

## 10. Open risks

- **Remote read amplification — measured.** `read_page` is per page at the local API, but the
  remote stores pages inside commit segments, and a segment is a sequence of zstd frames of at
  most 64 pages (`remote/segment.rs`, `FRAME_MAX_PAGES`). `read_page` fetches the whole frame that
  holds the page (`Runtime::read_page` → `frame_for_pageidx`), so the frame is the cold-read unit.

  Measured with `tests/remote_frames.rs` (Fs remote, encrypted/incompressible pages, one push per
  transaction; run `cargo test -p cdc-volume --test remote_frames -- --nocapture`):

  | Commit | Pages | Frames | Bytes on the wire | Cold read of one page |
  | --- | --- | --- | --- | --- |
  | bulk batch | 1001 | 16 | 4,100,397 | **262,163 B** (full 64-page frame) |
  | tail frame | 41 | 1 | 167,952 | 167,952 B |
  | small index-only | 4 | 1 | 16,397 | 16,397 B |

  Consequences for the Phase 4 budget:

  - The floor for a cold page read is 262,163 bytes, i.e. 256 KiB plus 19 bytes of zstd/checksum
    overhead. A 10 KiB range also needs the superblock, the chunk root and a leaf, each in its own
    frame: roughly **786 KiB** when those pages sit in bulk-batch frames. The "<100 KiB cold read"
    in the Phase 4 line is not achievable against a bulk-batch segment and must be replaced by that
    measured figure.
  - A commit's frames are only small when the commit itself is small. Pushing an index-only
    transaction of a few pages costs 16 KiB, which is where a <100 KiB target can be met.
  - **Corrected:** frames are per *push*, not per commit. `build_segment` collects every outstanding
    page across the pushed LSNs and frames that stream in page order, so committing the index
    separately changed nothing until the index pages were also *allocated* separately. What works
    is laying the index out contiguously after the data (§9, Phase 4); 256 KiB stays the per-chunk
    frame cost and the index adds one small frame. Shrinking chunk batches does not help, because
    the frame cap, not the batch size, sets the floor once a segment exceeds 64 pages.
- **Manifest memory and recipe continuity.** `put_file` keeps one 36-byte ref per chunk in memory so
  the manifest can be written last, and an overflowing recipe is one contiguous run allocated in
  that final transaction. At the 32 KiB target that is about 576 MB of refs for a 500 GiB file, and
  the run is capped by what one transaction can stage (millions of refs today). Segmented recipe
  trees are the fix if multi-hundred-GiB uploads become a requirement.
- **Superblock accounting fields are only exact after GC.** `chunk_count`, `file_count` and
  `live_bytes` are not maintained by `put_file`/`delete_file`; `gc::rewrite` sets them from its mark.
  `stat` and `ls` read manifests, so nothing depends on them, but do not treat them as authoritative
  between GCs.
- **Path tree ordering vs. Unicode.** Byte ordering is stable but not collation order; document
  that `ls` is bytewise. Suffix/prefix scans must use sentinel ranges, not string concatenation.
- **Key rotation.** Re-encrypting a volume means the GC rewrite path anyway, so rotate by rewriting
  and swapping the state file; per-volume keys keep that bounded. Because the rewrite re-keys chunk
  ids, a rotation also changes every chunk id, which is fine (manifests are rewritten) but means the
  new volume shares nothing byte-for-byte with the old one.
- **GC holds the live set in memory.** `gc::mark` keeps a `BTreeSet` of live pages and a `BTreeMap`
  of chunk hash to length, so a volume with tens of millions of live chunks needs gigabytes to GC.
  A streaming two-pass sweep (mark hashes to disk, then copy) is the fix if that becomes real.
- **Chunk ID key compromise.** Keyed hashing hides plaintext correlation from the remote, but
  anyone with the key and a candidate plaintext can confirm presence. This is a dedup property, not
  a bug.
