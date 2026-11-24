---
title: Quick start
description: Create a workspace, add a live document and a photo, read a byte range back, and see where everything landed.
---

This walks through one workspace end to end: a catalog entry for a live document with a
sequenced frame in the oplog, a media file ingested into object storage, a ranged read of
that file, and the resulting state on disk. Every transcript below is real output.

## 1. Create the project

```sh
mkdir field-notes && cd field-notes
mkdir drafts
printf '# Field notes\n\nFirst draft.\n' > drafts/notes.md
printf 'photo-bytes-0123456789' > drafts/photo.jpg
```

Add `workspace.toml`. `autosync` makes Graft push the volumes to the remote while a command
runs, but commands here exit quickly, so use `workspace sync` when you need a push to
happen for sure. Without either, the remote stays unused and everything lives only in
`data_dir`.

```toml
data_dir = "./data"
autosync = 1

[remote]
type = "fs"
root = "./remote"

[media]
type = "fs"
root = "./media-store"
```

Media is deliberately a separate store from the Graft remote. Graft owns `segments/` and
`logs/` under its remote root; media objects use their own root (or bucket).

## 2. Initialize the workspace

```sh
workspace init
```

```text
workspace: w-Q7PC828WuhwSN6jB
catalog tag: ws-catalog
oplog tag: ws-catalog-oplog
data: ./data
remote: fs:./remote
```

Two volumes are created: `ws-catalog` for the SQLite catalog and `ws-catalog-oplog` for
frame sequencing. The workspace id is stored in the catalog, so it survives restarts; a
second `init` keeps it.

## 3. Add a live document

The catalog entry says the path is a Loro document called `doc-notes`. The frame payload
is the document's first Loro update, sequenced by its producer:

```sh
workspace entry put notes/field \
  --lane loro --kind doc --loro-doc doc-notes --mime text/markdown
# put notes/field: revision 1

workspace doc append doc-notes --peer alice --seq 1 --file drafts/notes.md
# applied alice:1
```

Fetch the frames a client is missing. It has no cursor yet, so it starts at sequence 1:

```sh
workspace doc frames doc-notes --out-dir frames
# alice 1 frames/alice-1.bin
```

Frames are opaque. The coordinator never parses Loro updates; it stores them in order and
hands them back.

When the document is compacted into a snapshot, record its frontier on every entry that
points at the document:

```sh
printf 'frontier-bytes' > frontier.bin
workspace doc snapshot doc-notes --file frontier.bin
# snapshot doc-notes: updated 1 entries

workspace entry get notes/field
```

```text
path: notes/field
lane: loro
kind: doc
revision: 1
updated_at: 1795346290
blob: -
loro_doc: doc-notes
loro_frontier: 66726f6e746965722d6279746573
code_tree: -
release_doi: -
mime: text/markdown
size: -
```

`loro_frontier` is the frontier blob in hex — whatever bytes the snapshot call gave us.

## 4. Add media

`blob put` streams the file into media storage and points an entry at the resulting hash:

```sh
workspace blob put media/photo.jpg --file drafts/photo.jpg --mime image/jpeg
# put media/photo.jpg: blake3:852d942d… 22 bytes raw rev 1 (uploaded)
```

Run the same command again. The bytes are already stored, so nothing is copied:

```sh
workspace blob put media/photo.jpg --file drafts/photo.jpg --mime image/jpeg
# put media/photo.jpg: blake3:852d942d… 22 bytes raw rev 2 (deduplicated)
```

Now read a range — bytes 6 through 10 of `photo-bytes-0123456789`:

```sh
workspace blob get media/photo.jpg --out thumb.bin --offset 6 --limit 5
# read media/photo.jpg: 5 bytes at offset 6

cat thumb.bin
# bytes
```

`--offset` past the end of the object is an error. A range that runs past the end is clamped,
and a zero-length range writes an empty file.

## 5. Inspect the workspace

```sh
workspace entry ls
# media/photo.jpg	media	file	2
# notes/field	loro	doc	1

workspace blob ls
# blake3:852d942d…	22	raw	-

workspace doc ls
# doc-notes	1	1	28

workspace status
```

```text
workspace: w-Q7PC828WuhwSN6jB
catalog tag: ws-catalog
oplog tag: ws-catalog-oplog
schema: 1
remote: fs:./remote
entries: 2
docs: 1
blobs: 1 (pinned 0)
releases: 0
```

The blob's full record shows its locator and digests:

```sh
workspace blob show blake3:852d942d0c9bc1c233cd09dfbc91b6ca0a564a14d48435be165ff3a97ea4319a
```

```text
hash: blake3:852d942d0c9bc1c233cd09dfbc91b6ca0a564a14d48435be165ff3a97ea4319a
sha256: 4ac4b337c77b99e9898733d63edb767717fe22496d786ba032abce369da7f6d0
size: 22
mime: image/jpeg
storage: raw
locator: media/raw/w-Q7PC828WuhwSN6jB/852d942d0c9bc1c233cd09dfbc91b6ca0a564a14d48435be165ff3a97ea4319a
refcount: 0
pinned: false
created_at: 1795346290
```

`refcount` is advisory and can lag; liveness is recomputed from `entries` and pinned blobs.
The `locator` is the canonical key inside the media store.

## 6. Where the bytes went

Push the volumes to the remote:

```sh
workspace sync
```

```text
pushed catalog 5rMKZjMKvz-3N7frv6Fi5wbQ
pushed oplog 5rMKZjMKvz-2qH6BJz1wpp3P
```

```text
remote/
  logs/74ghR5neqg-3PaCAezZvSp7c/commits/fffffffffffffffe
  logs/74ghR5neqg-2vaRfvmeHi9HM/commits/fffffffffffffffe
  segments/8H25GSDynd-2oEAsEXfFjP9k
  segments/8H25GSDyo9-3w3Rc4RA2BT8D
  segments/8H25GSDyqw-3Ew6QsdbSrzfn
  segments/8H25GSDyrA-3J8wrVAF32QBp

media-store/
  media/raw/w-Q7PC828WuhwSN6jB/852d942d0c9bc1c233cd09dfbc91b6ca0a564a14d48435be165ff3a97ea4319a
  staging/w-Q7PC828WuhwSN6jB/
```

The catalog and oplog live in `data/` locally; Graft stores them remotely as
`logs/<logid>/commits/<lsn>` and immutable `segments/`. Media went straight to its own
store, under `media/raw/<workspace>/<blake3 hex>`. The staging directory is empty after a
successful ingest; it only holds objects from uploads that were interrupted, which
`blob sweep-staging` removes.

## 7. Every command is a fresh process

Each `workspace` invocation opens the volumes, does one thing, and exits, so running
`workspace status` or `workspace blob get …` later reopens everything from `data/` and
`./remote`. If a command fails with a catalog error, check that you are in the directory
with the right `workspace.toml` or pass `--config`.

To read this workspace from another data directory, push first and then follow
[Clone a workspace](/docs/workspace/clone/).

## Next steps

- [CLI usage](/docs/workspace/) — every command with its options and semantics.
- [CLI reference](/docs/workspace/cli/) — argument reference generated from the command
  definitions.
- [Storage layout](/docs/concepts/layout) — how the catalog, oplog and media fit together.
