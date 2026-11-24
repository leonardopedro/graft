---
title: Storage layout
description: How a workspace splits into a catalog, an oplog and media, and where each lane's bytes live.
---

A workspace is not one database with some files attached. It is an index over immutable
objects, and the index is split from the objects on purpose:

```text
            data_dir/                      remote (Graft)
        ┌────────────────┐          ┌─────────────────────────────┐
        │ local index    │  pushes  │ logs/<logid>/commits/<LSN>  │
        │ and page cache │ ───────▶ │ segments/<segment id>       │
        └────────────────┘          └─────────────────────────────┘
                 │
                 ├── volume `ws-catalog`        SQLite catalog on a Graft VFS
                 └── volume `ws-catalog-oplog`  per-producer frame sequencing

        media store (bucket or directory)
        ├── media/raw/<workspace>/<blake3 hex>
        └── staging/<workspace>/<ticket>
```

Local state is a cache of what the volumes have written; the remote holds the durable
segments. Media deliberately bypasses Graft.

## Volumes

| Volume | Tag | Contents |
| --- | --- | --- |
| Catalog | `ws-catalog` | SQLite schema: `entries`, `blobs`, `releases`, `update_releases`, `catalog_meta` |
| Oplog | `ws-catalog-oplog` | SQLite schema: `docs`, `producers`, `frames`, `acks`, `oplog_meta` |

The catalog tables:

| Table | Key | Holds |
| --- | --- | --- |
| `catalog_meta` | `key` | `workspace_id` and `created_at`; the schema version is `PRAGMA user_version` |
| `entries` | `(workspace_id, path)` | Lane, kind, cross-lane pointers, `revision`, `deleted_at`, `updated_at`; the only mutable table |
| `blobs` | `hash` | `sha256`, `size`, `mime`, `storage` (`raw` or `cdc`), media `locator`, advisory `refcount`, `pinned`, `created_at` |
| `releases` | `doi` | `concept_doi`, `workspace_id`, `manifest_hash`, `zenodo_record`, `published_at` |
| `update_releases` | `(workspace_id, store, prefix, name, release)` | Binary-update repository, manifest hash, file/chunk/byte counts, and the pin that keeps `gc` from collecting its manifest |

And the oplog tables:

| Table | Key | Holds |
| --- | --- | --- |
| `oplog_meta` | `key` | Reserved key/value metadata; no rows are written yet |
| `docs` | `doc_id` | One row per live document, with `created_at` |
| `producers` | `(doc_id, peer)` | `next_seq`, the frame the coordinator expects next, and `updated_at` |
| `frames` | `(doc_id, peer, seq)` | Opaque `data` plus `received_at` |
| `acks` | `(doc_id, producer, consumer)` | `acked_seq` and `updated_at` for one consumer's view of a producer |

A volume tag **is** the SQLite database name in `graft-sqlite`, so the tag always resolves
to the same volume inside a data directory. The oplog tag is derived from the catalog tag
(`<catalog_tag>-oplog`), so two workspaces with different catalog tags never share an
oplog.

The split exists because traffic differs. Catalog writes happen when a path is created or
its metadata changes. Oplog writes happen per edit and per ack. Putting both in one SQLite
database would make every keystroke contend with catalog writes and, worse, make the catalog
sync on every frame. The only lane-1 pointer the catalog keeps is
`entries.loro_frontier`.

## Lanes

| Lane | `lane` value | Objects | Hot store | Client surface |
| --- | --- | --- | --- | --- |
| Live | `loro` | Notes, specs, whiteboards | Sequenced frames in the oplog | `workspace doc *` |
| Code | `code` | Trees, git bundles | Encrypted `cdc-volume` | `workspace code *` |
| Media | `media` | Photos, video, datasets | Content-addressed objects | `workspace blob *` |
| Archive | `archive` | Frozen releases and DOIs | Zenodo record | not built yet |

An entry names its lane, so a path is routed without guessing from its name or MIME type.
`kind` is orthogonal: `doc` for text documents, `file` for a single object, `dir` for a
manifest, `dataset` for a collection, `release` for a published bundle.

Cross-lane pointers are columns on `entries`, and they are only hints to immutable objects:

- `blob_hash` → `blobs.hash`, which carries the media locator.
- `loro_doc_id` → a document in the oplog volume.
- `code_tree` → a tree manifest.
- `release_doi` → a row in `releases`.

## Catalog

`entries` is the only mutable table. Its primary key is `(workspace_id, path)`, and an
upsert increments `revision` rather than inserting a second row. Deletes are soft
(`deleted_at`), so a release can still resolve a path after it was removed from the working
set. Paths are workspace-relative, slash-separated, with no empty segments and no `..`.

Two invariants keep the catalog safe to rebuild:

1. **Bytes before rows.** A blob or frame is durable before the row pointing at it is
   committed. A crash therefore leaves unreferenced objects, never dangling references.
2. **Refcounts are advisory.** `blobs.refcount` can lag after a crash, so liveness is
   recomputed from `entries` and `pinned`. Nothing critical depends on the counter being
   exact.

`releases` rows pin immutable bundles by DOI and manifest hash. `update_releases` rows do
the same for binary-update manifests until `prune-refs` forgets the release. Pinning is what
lets `blob gc` and update `gc` delete unreferenced objects safely while catalog rows still
name them.

## Oplog

Frames are sequenced per `(doc, producer)`, not globally. A producer numbers its own frames
from 1, and the coordinator accepts them only in order, writing the frame before updating
`producers.next_seq`:

| Table | Row |
| --- | --- |
| `docs` | One row per document, with creation time |
| `producers` | Per-peer `next_seq` — the expected next frame |
| `frames` | `(doc_id, peer, seq) → data`, the opaque payload |
| `acks` | Per-consumer `acked_seq` for a producer's stream |

A reconnecting client sends its cursors and receives every frame after them, so a frame
that was acked cannot be lost. Acks are monotonic and clamped to what the coordinator
applied; they are computed, not cached. Documents live only here — the catalog stores a
frontier pointer, never the frames.

## Media

Media objects are content addressed inside a store of their own:

```text
media/raw/<workspace>/<blake3 hex>     committed object
staging/<workspace>/<ticket>           in-flight upload
```

- The hash is blake3 of the bytes; sha256 is recorded alongside for portable manifests.
- `locator` on the blob row is the canonical key, not a URL.
- Ingest streams to staging while hashing, then checks the catalog again, promotes the
  object, and commits the row. A duplicate upload copies nothing.
- Reads address ranges within a raw object; offsets are clamped to the object size.
- Only the `raw` storage class is produced today. `cdc` is reserved for chunked archives
  and is rejected by the range reader.

The store is configured separately (`[media]`) because Graft's remote root already owns
`logs/` and `segments/` and its lifecycle rules are about segments, not user media. Sharing
a bucket prefix would put user data under Graft's object layout.

## Graft layer

Graft stores volumes as:
`logs/<logid>/commits/<CBE64 LSN>` and `segments/<segment id>`, behind a local Fjall index
in `data_dir`. Writes go through the log; segments are immutable pages. With `autosync`
set, a running process pushes commits and segments to the remote.

Graft is built for lazy partial replicas: a reader that is missing a page fetches just that
page. `workspace share` prints a reference containing both volume ids **and** both remote
log ids; `workspace clone` attaches the volumes at those ids, pulls, and only then opens
SQLite. The log ids are in the reference because they are not derivable from a volume id:
opening by volume id alone would create a fresh remote log and an empty workspace.
`workspace sync` pushes local commits and `workspace pull` fast-forwards from the remote.
Pages stay on the remote until a query reads them, so a clone is cheap to make and slow on
first access.

Two data directories are not a merge protocol: they can follow the same volumes in
sequence, but concurrent writes would diverge and Graft will not reconcile them without
intervention. Media bytes are also outside Graft, so a clone needs its own access to the
same media store.

## Lifecycle

| Object | Collected by |
| --- | --- |
| Abandoned staging objects | `workspace blob sweep-staging` |
| Old oplog frames | `workspace doc compact` |
| Unreferenced canonical blobs | `workspace blob gc` |
| Unreferenced archived blobs | `workspace blob gc-cdc` |
| Graft segments | Graft's own snapshot checksum and GC |

These collectors use catalog entries and pins, acknowledgements, or Graft's snapshot state to
decide liveness before deleting anything. Archive collection rewrites the volume; keep the
old volume until after `workspace sync` publishes the replacement.
