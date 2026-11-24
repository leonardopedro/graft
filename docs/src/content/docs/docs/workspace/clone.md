---
title: Clone a workspace
description: Adopt an existing workspace in a second data directory with `share` and `clone`, then keep it current with `sync` and `pull`.
---

A workspace lives in two Graft volumes plus an optional media store. A second data
directory can adopt it instead of starting a new one: `workspace share` prints a reference,
`workspace clone` consumes it, and `workspace sync` / `workspace pull` move later commits.

This page continues the project from the [quick start](/docs/workspace/quickstart/): the
same `workspace.toml`, the same catalog and oplog volumes, and `./remote` as the remote.

## 1. Push and share

`init` and the write commands run in short-lived processes, so `autosync` may never tick.
Push explicitly before expecting another machine to see the data:

```sh
workspace sync
workspace share
```

```text
pushed catalog 5rMKZjMKvz-3N7frv6Fi5wbQ
pushed oplog 5rMKZjMKvz-2qH6BJz1wpp3P
workspace: w-Q7PC828WuhwSN6jB
ref: wref:1:5rMKZjMKvz-3N7frv6Fi5wbQ:74ghR5neqg-3PaCAezZvSp7c:5rMKZjMKvz-2qH6BJz1wpp3P:74ghR5neqg-2vaRfvmeHi9HM
remote: fs:./remote
next: workspace clone <ref> --data <DIR>
```

The `ref` carries four ids: the catalog volume and its remote log, then the oplog volume
and its remote log. A volume id alone is not enough to attach — without the remote log id,
Graft would create a new log and the copy would look empty. Copy the whole line; the four
fields are positional and all are required.

## 2. Clone

Run from the same project directory, so the config's remote and media settings apply, but
point `--data` somewhere new:

```sh
REF=$(workspace share | sed -n 's/^ref: //p')
workspace clone "$REF" --data data-laptop
```

```text
workspace: w-Q7PC828WuhwSN6jB
ref: wref:1:5rMKZjMKvz-3N7frv6Fi5wbQ:74ghR5neqg-3PaCAezZvSp7c:5rMKZjMKvz-2qH6BJz1wpp3P:74ghR5neqg-2vaRfvmeHi9HM
remote: fs:./remote
entries: 2
docs: 1
```

`clone` attaches every volume named in the reference (catalog, oplog, and the encrypted
archive when present) at the given ids, pulls their commits, and only then opens
SQLite — so the catalog schema and rows are already there. It refuses a data directory
that already holds a workspace rather than mixing two catalogs, and refuses a reference
whose catalog has no commits on the remote (forget `sync` and the copy would silently be
empty). A failed attempt leaves the data directory reusable.

The copy is a real workspace. Rows, frames and frontiers are readable immediately:

```sh
workspace status --data data-laptop
workspace entry ls --data data-laptop
workspace doc frames doc-notes --data data-laptop
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
media/photo.jpg	media	file	2
notes/field	loro	doc	1
alice 1 28
```

Pages are fetched from the remote on demand, so the first reads of a large catalog or oplog
are slower than later ones.

## 3. Keep both sides current

Write on the original, push, and pull on the copy:

```sh
workspace entry put notes/from-desktop \
  --lane loro --kind doc --loro-doc doc-2 --mime text/markdown
# put notes/from-desktop: revision 1

workspace sync
# pushed catalog 5rMKZjMKvz-3N7frv6Fi5wbQ
# pushed oplog 5rMKZjMKvz-2qH6BJz1wpp3P

workspace pull --data data-laptop
# pulled catalog 5rMKZjMKvz-3N7frv6Fi5wbQ
# pulled oplog 5rMKZjMKvz-2qH6BJz1wpp3P

workspace entry ls --data data-laptop
# media/photo.jpg	media	file	2
# notes/field	loro	doc	1
# notes/from-desktop	loro	doc	1
```

`sync` pushes local commits; `pull` fetches the remote's. Both operate on the whole
workspace, not one lane.

## Things to know

- **Media is not in Graft.** `clone` adopts catalog rows and oplog frames, but a photo's
  bytes live in the `[media]` store, which is not part of the remote. The example above
  works because both data directories use the same `./media-store`. For two machines,
  point `[media]` at shared object storage (an S3-compatible bucket), or copy the objects
  before running `blob get`. A cloned row whose bytes are missing fails at read time.
- **One writer at a time.** These commands fast-forward; they do not merge diverged
  histories. If both data directories commit to the same volume, Graft cannot reconcile
  them without intervention. Clone to read or to continue elsewhere, not to edit in
  parallel.
- **A remote is required.** Clone and pull refuse the in-memory remote; a new process
  starts with an empty one. Pass `--remote` or set `[remote]` in the config.
- **The reference is a capability.** Anyone who can read the remote and has the reference
  can read the workspace. Treat it like a URL to the data, not a secret, but do not publish
  it either.
