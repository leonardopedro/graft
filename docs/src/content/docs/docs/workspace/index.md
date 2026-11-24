---
title: Workspace CLI
description: Manage a Graft-backed workspace from the command line — catalog entries, the live-document oplog, and media blobs.
---

New here? The [quick start](/docs/workspace/quickstart/) walks through a full example and
shows where the bytes land. The transcripts on this page come from that same project and
continue it, so the ids, hashes and paths match the other guides.

The `workspace` command manages a workspace stored on Graft. A workspace is three
things, and they are deliberately stored apart:

| Part | Volume or location | Holds |
| --- | --- | --- |
| Catalog | Graft volume tagged `ws-catalog` | SQLite index of entries, blobs and releases |
| Oplog | Graft volume tagged `ws-catalog-oplog` | Sequenced frames for live documents |
| Media | A separate object store (`[media]`) | Raw bytes, content addressed |

The catalog is an index, not a peer of the data it points at. Entries record where an
object lives (a blob hash, a Loro document id, a frontier) while the bytes live in media
storage or in the oplog. The oplog has its own volume because acks arrive per edit; keeping
them in the catalog would make every keystroke contend with catalog writes. Media lives in
its own store because Graft owns its configured bucket prefix (`logs/`, `segments/`), so
media keys must not share it.

:::caution
Graft is alpha software. Treat the catalog as a convenience index over durable objects, and
keep the backup path (bundles, snapshots, exported files) working before you rely on a
workspace.
:::

## Build

```sh
cargo build --package graft-workspace
# binary: target/debug/workspace
```

Or install it:

```sh
cargo install --path crates/graft-workspace
```

## Configuration

`workspace` reads `workspace.toml` from the working directory unless `--config <FILE>` names
another path. Environment variables prefixed with `WORKSPACE_` override the file (`__`
separates nesting), and `--data` / `--remote` override both.

```toml
# Local Graft state: the Fjall index and page cache.
data_dir = "./data"

# Sync with the remote in the background (seconds).
autosync = 1

[remote]
type = "fs"          # "memory", "fs", or "s3_compatible"
root = "./remote"

# Optional. Without it, raw blob commands are unavailable.
[media]
type = "fs"
root = "./media-store"

# Optional for media, required for code checkpoints. Chunked, encrypted
# archive blobs in their own Graft volume.
[archive]
key_file = "./archive.key"   # 32 bytes as 64 hex characters

# Optional. Off-site target for `workspace backup`.
[backup]
prefix = "backup"
[backup.store]
type = "fs"
root = "./backup-store"

# Optional. Encrypted CDC copy on its own Graft remote; needed to restore
# code checkpoints and cdc media. Keep it separate from the live remote.
[backup.archive]
data_dir = "./backup-archive-data"
[backup.archive.remote]
type = "fs"
root = "./backup-archive-remote"
```

An S3-compatible remote uses the usual AWS environment:

```toml
[remote]
type = "s3_compatible"
bucket = "my-graft-bucket"
prefix = "workspaces"
```

```sh
export AWS_ACCESS_KEY_ID="..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_REGION="us-east-1"
# export AWS_ENDPOINT_URL="..."   # MinIO, R2, and other S3-compatible services
```

Use a **different bucket** for `[media]` than for `[remote]`. Graft writes `logs/` and
`segments/` under the remote's prefix; media objects belong in their own bucket so
lifecycle rules cannot reach Graft's segments.

## Quick start

```sh
workspace init
workspace status
```

```text
workspace: w-Q7PC828WuhwSN6jB
catalog tag: ws-catalog
oplog tag: ws-catalog-oplog
data: ./data
remote: fs:./remote
```

`init` creates the two volumes and assigns a workspace id. Running it again keeps the
existing id. `status` prints the id, schema version, remote and row counts; before `init` it
reports ``workspace: (none; run `workspace init`)``.

## Entries

Entries are paths with a lane and a kind:

| Lane | Meaning |
| --- | --- |
| `loro` | A live document (notes, specs, whiteboards) |
| `code` | Versioned code: tree manifests, bundles |
| `media` | Photos, video, audio, datasets |
| `archive` | Frozen releases |

Kinds are `doc`, `file`, `dir`, `dataset` and `release`.

```sh
workspace entry put notes/field \
  --lane loro --kind doc --loro-doc doc-notes --mime text/markdown
# put notes/field: revision 1

workspace entry get notes/field
```

`entry get` prints every field, including the Loro frontier as hex:

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

The frontier is `-` until a snapshot records one; the quick start's `doc snapshot` is what
fills it in above.

```sh
workspace entry ls
# media/photo.jpg	media	file	2
# notes/field	loro	doc	1
```

The [clone tutorial](/docs/workspace/clone/) adds `notes/from-desktop` to this workspace.
Removing it is a soft delete:

```sh
workspace entry rm notes/from-desktop
# removed notes/from-desktop

workspace entry ls notes/
# notes/field	loro	doc	1

workspace entry ls --all
# media/photo.jpg	media	file	2
# notes/field	loro	doc	1
# notes/from-desktop	loro	doc	2
```

Notes:

- Paths are workspace-relative and use `/`. Absolute paths, `..`, empty segments and
  backslashes are rejected.
- An upsert increments `revision`; it never creates a second row for the same path.
- `entry rm` is a soft delete: the row stays (so a release can still reference it) but
  disappears from `ls` unless `--all` is passed. The delete bumps `revision` too.
- `--blob`, `--code-tree` and `--release-doi` point at other lanes. A blob hash must already
  be recorded, so ingest the bytes first.

## Live documents

The oplog stores opaque Loro frames, sequenced per producer. Numbering starts at 1 for each
`(document, peer)` and the coordinator accepts frames strictly in order:

```sh
workspace doc append doc-notes --peer alice --seq 1 --file drafts/notes.md
# applied alice:1

workspace doc append doc-notes --peer alice --seq 3 --file drafts/frame-3.bin
# gap alice: expected 2

workspace doc append doc-notes --peer alice --seq 2 --file drafts/frame-2.bin
# applied alice:2

workspace doc append doc-notes --peer alice --seq 1 --file drafts/notes.md
# duplicate alice:1 (already applied)
```

A gap is reported, not applied, and the command still exits successfully: it is a protocol
state, not a failure. The producer resends from the expected sequence.

A client that reconnects asks for everything after its cursors:

```sh
workspace doc frames doc-notes
# alice 1 28
# alice 2 9

workspace doc frames doc-notes --cursor alice=1
# alice 2 9

workspace doc frames doc-notes --cursor alice=1 --out-dir frames-extra
# alice 2 frames-extra/alice-2.bin
```

Frame 3 was rejected as a gap, so only the two accepted frames appear. Without `--out-dir`,
each line is `peer seq bytes`; with it, payloads are written to `<DIR>/<peer>-<seq>.bin` and
the path replaces the byte count.

Acks record how far a consumer has read a producer's stream. They are monotonic and clamped
to what the coordinator actually applied:

```sh
workspace doc ack doc-notes --producer alice --consumer bob --seq 3
# acked alice through 2 for bob

workspace doc stats doc-notes
# doc: doc-notes
# peers: 1
# frames: 2
# bytes: 37
# frontier: alice=2

workspace doc ls
# doc-notes	1	2	37
```

The ack asked for 3 and recorded 2: only applied frames count.

Compaction keeps the document state and drops the acknowledged history. It first
materializes a Loro snapshot and records the per-producer sequence it covers; only then
are frames every known consumer has acked deleted:

```sh
workspace doc compact doc-notes --producer alice --through 2
```

It prints how many frames were deleted, or which consumer is still behind. A fresh client
then receives the stored snapshot instead of the deleted prefix. `doc frames` prints
`snapshot <bytes> bytes` (or writes the snapshot with `--snapshot-out <FILE>`) before any
frames that follow it:

```sh
workspace doc frames doc-notes --snapshot-out compacted.snapshot
```

`workspace serve` repeats this automatically after 300 seconds without a committed frame
or ack. `--compact-after-secs <SECS>` changes the idle window; `0` disables it. The ack
guard still applies, so a lagging consumer blocks pruning rather than losing frames.

When an external snapshot is recorded, its Loro frontier can also be written to the
catalog entries:

```sh
workspace doc snapshot doc-notes --file frontier.bin
# snapshot doc-notes: updated 1 entries
```

That updates every entry whose `loro_doc_id` matches. Metadata upserts never clear it.

## Media

`blob put` streams a file into media storage and points an entry at it:

```sh
workspace blob put media/photo.jpg --file drafts/photo.jpg --mime image/jpeg
# put media/photo.jpg: blake3:852d942d… 22 bytes raw rev 1 (uploaded)
```

The file is hashed while it is written: blake3 for the content address and sha256 for
portable manifests. If the bytes are already stored, nothing is copied:

```sh
workspace blob put media/photo.jpg --file drafts/photo.jpg --mime image/jpeg
# put media/photo.jpg: blake3:852d942d… 22 bytes raw rev 2 (deduplicated)
```

Read the whole object, or a range of it:

```sh
workspace blob get media/photo.jpg --out thumb.bin --offset 6 --limit 5
# read media/photo.jpg: 5 bytes at offset 6
```

`--offset` past the end is an error; a range that runs past the end is clamped to the object
size, and a zero-length range produces an empty file.

Inspect and clean up:

```sh
workspace blob ls
# blake3:852d942d…	22	raw	-

workspace blob show blake3:852d942d0c9bc1c233cd09dfbc91b6ca0a564a14d48435be165ff3a97ea4319a
workspace blob sweep-staging --older-than 86400
# swept 0 staging objects
```

The full record names the object's place in the media store:

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

Ingest writes to `staging/<workspace>/…` and then moves the object to
`media/raw/<workspace>/<blake3 hex>`. An upload interrupted mid-stream leaves a staging
object; `sweep-staging` removes those older than the given number of seconds (default one
day). `blob gc` deletes canonical objects that no entry references and no release pins;
it removes the catalog row before the object, so an interrupted sweep can leave an orphan
object but never a row whose bytes are gone.

### Archive blobs

With `[archive]` configured, `--storage cdc` chunks, deduplicates and encrypts the file
into a dedicated `cdc-volume` instead of the raw media store:

```sh
workspace blob put datasets/run-01.bin --file run-01.bin --storage cdc
# put datasets/run-01.bin: blake3:… 734003200 bytes cdc rev 1 (uploaded)
```

A direct upload avoids streaming the payload through the daemon, but only S3-compatible
backends offer presigned writes. Ask for a ticket, upload the bytes to the URL, then commit:

```sh
workspace blob presign blake3:<hex> --expires 900
# ticket: staging/<workspace>/<id>
# method: PUT
# url: https://…
# header: …
# expires: 900

curl -X PUT --data-binary @run-01.bin \
  -H 'content-type: application/octet-stream' \
  '<url from the ticket>'

workspace blob commit datasets/run-01.bin --ticket staging/<workspace>/<id> \
  --hash blake3:<hex> --size 734003200 --mime application/octet-stream
```

`commit` accepts only a ticket under this workspace's `staging/` prefix. It checks the
size, streams the staging object into canonical storage while recomputing blake3, rejects a
hash mismatch, removes staging, and creates the entry. A hash already in the catalog skips
the copy and drops the staging object. On a filesystem or other backend without presign,
`blob presign` fails with `this media backend cannot presign uploads`; use `blob put`.

The hash is required: the ticket is issued before the bytes exist, and a client-supplied
hash must be verified rather than trusted. Presigned requests expire; the staging sweeper
removes uploads that are never committed.

Range reads work the same way; `blob get` dispatches to the chunk index for `cdc` rows:

```sh
workspace blob get datasets/run-01.bin --out slice.bin --offset 1073741824 --limit 65536
```

The key file holds exactly 32 bytes as 64 hex characters and is read on every open; it is
never written to `workspace.toml` or the catalog. The archive volume is named
`<catalog_tag>-archive`, is pushed by `workspace sync`, and is included in `share`/`clone`
when configured.

`blob gc` only covers raw objects. Archived data is collected by rewriting the volume:

```sh
workspace blob gc-cdc
```

This forgets `cdc` rows that no entry references and that no release pins, deletes manifests
no catalog row names, then copies the live set into a fresh encrypted volume and repoints
`<catalog_tag>-archive` at it. Chunk ids are re-keyed during the copy, so the old volume
is no longer needed once `workspace sync` publishes the new one; it is left on disk for a
grace period. The key file is unchanged and is still required to open the new volume. `share` and `clone` after the sync use the new volume.

## Code checkpoints

`workspace code snapshot <DIR> --path <PATH>` writes one tree manifest and stores every
file, the manifest and any `git bundle` in the encrypted `cdc-volume`. Code therefore
requires `[archive]`; unchanged files are deduplicated at chunk level, and restore streams
and verifies each file through the archive's range reader:

```sh
workspace code snapshot ./repo --path code/repo
workspace code restore blake3:<tree> --out ./restored
```

A git repository also gets one bundle of all refs. Checkpoint blobs are pinned so `blob
gc-cdc` cannot collect them. Raw media is not required for this lane, but `share` and
`clone` carry the archive volume and its key requirement. Configure `[backup.archive]` if
you need `workspace backup` to preserve code checkpoint contents for restore.

## Backup

`workspace backup` writes a self-contained backup to the `[backup]` target, under
`<prefix>/<workspace>/<timestamp>-<pid>/`:

| Object | Contents |
| --- | --- |
| `catalog.db` | Consistent export through Graft's `graft_export` pragma |
| `blobs/<hash>.bin` | Raw blob bytes, content addressed and verified by blake3 |
| `code/<hash>.bundle` | One verified git bundle per distinct code checkpoint |
| `docs/<doc>.jsonl` | The remaining oplog frames for a document, ordered by peer and sequence, with hex-encoded bytes |
| `docs/<doc>.snapshot` | A materialized Loro snapshot; a compacted document uses the snapshot stored in its oplog |
| `archive.json` | Volume and log ids for the encrypted CDC copy, written beside the run prefixes |
| `manifest.json` | Workspace id, timestamp, object keys, sizes, blake3 hashes and the optional archive reference |

The manifest is written last, so a prefix without one is incomplete. A backup target is a
separate bucket or root from media; the prefix keeps both from colliding if they share one.

```sh
workspace backup
# backup: backup/w-Q7PC828WuhwSN6jB/1795346290-12345
# catalog: .../catalog.db 40960 bytes blake3:…
# code bundles: 1
# blobs: 5
# document logs: 1
# doc-1 (1 frames) 73 bytes blake3:…
#   snapshot: .../docs/doc-1.snapshot 120 bytes blake3:…
# manifest: .../manifest.json 512 bytes blake3:…
```

`--prefix` overrides only the base prefix for that run; the workspace and timestamp are
still appended. `--keep N` applies retention after a one-shot backup: the newest N complete
runs (those with a `manifest.json`) are kept, and older or incomplete runs are deleted.

With `[backup.archive]`, retention also collects the encrypted backup archive. It reads the
catalog of every retained run, treats those `cdc` locators as live, deletes files no retained
run names, copies the remaining files into a fresh encrypted volume, pushes it, and updates
`archive.json`. Older backup runs can therefore still restore while pages used only by a
pruned run are reclaimed. The previous volume is left in place for a grace period.

Run the same command in scheduled mode with `--every SECS`; it takes one backup
immediately, then sleeps between runs. Scheduled mode defaults to `--keep 7`:

```sh
workspace backup --every 3600 --keep 7
```

The runner stays in the foreground, so a service manager should supervise it. A failed run
is logged and retried after the interval; it does not exit. Retention only touches valid
`<timestamp>-<pid>` run prefixes under the configured workspace prefix and leaves unrelated
objects alone.

For systemd or launchd, generate one-shot units instead of supervising `--every`. The
service manager supplies the schedule and records missed runs:

```sh
workspace service generate --kind systemd --out ./units --interval 3600 --keep 7
workspace service generate --kind launchd --out ./units --interval 3600 --keep 7
```

The generated command uses an absolute `--config` path and sets the working directory to
the directory where you ran the generator. systemd gets a `.service` plus `.timer`;
launchd gets a `com.graft.<name>.plist`. Override the executable with `--exec` and the
unit base name with `--name`. CI validates generated files with `systemd-analyze verify`
on Linux and `plutil -lint` on macOS.

Encrypted archive blobs are **never** copied in plaintext. Without `[backup.archive]`,
`backup` skips every `cdc` row and prints the count, leaving those bytes only in the live
archive volume. With `[backup.archive]`, it decrypts each cataloged `cdc` file inside the
backup process, re-encrypts it into a separate `cdc-volume` on the configured Graft remote,
verifies the plaintext blake3, and records the volume/log ids in `archive.json` and the
manifest. An unencrypted backup target must still be treated as public for every raw object
it carries. Code checkpoints are `cdc`, so their file and manifest bytes stay encrypted;
their git bundles are also copied as separate backup objects.

A compacted document is complete when its oplog holds a snapshot: the log contains only the
frames newer than that snapshot, and restore imports the snapshot before replaying them. A
document whose remaining frames have a gap and no snapshot is marked `partial`; its
snapshot is omitted and restore leaves it out with a warning.

### Restore

```sh
workspace restore backup/w-Q7PC828WuhwSN6jB/1795346290-12345 --data fresh-data
```

The command uses the same `[backup]` store and the destination `[media]`/`[archive]`
configuration. It refuses a workspace that already has entries, blobs or releases, then
replaces the catalog, streams each backed-up blob to its original locator, replays document
frames, and verifies every downloaded object and materialized snapshot against the
manifest. Run `workspace share` after restore to get a reference for the restored volumes.

The catalog, documents and raw media are restored. With `[backup.archive]`, every
cataloged `cdc` blob is copied from the encrypted backup archive into the restored archive
volume and verified against its blake3 hash; `code restore` then works for checkpoint
trees. Without an archive backup, `cdc` blobs are reported as skipped and need a separate
archive-level copy. Compacted documents restore from their snapshot plus the remaining
frames; a document marked partial is skipped rather than replayed from a truncated log.
`workspace sync` publishes the restored volumes.

Document ids may contain dots, so backup keys encode anything outside `[A-Za-z0-9-]` as
`_xx`; a document named `..` is written as `docs/_2e_2e.jsonl`, never outside the prefix.
Restore applies the same rule to manifest keys and blob locators, and rejects traversal in
`--prefix` before reading the manifest.

## Metrics

`workspace metrics` prints Prometheus text from the catalog and oplog:

```text
workspace_oplog_docs 1
workspace_oplog_frames 1
workspace_oplog_bytes 5
workspace_blobs{storage="raw"} 1
workspace_blob_bytes{storage="raw"} 14
workspace_pinned_blobs 0
workspace_releases 0
workspace_blob_ingested_total 1
workspace_blob_ingested_bytes_total 14
workspace_blob_swept_total 0
workspace_blob_swept_bytes_total 0
workspace_release_stage_seconds_count 1
workspace_release_stage_seconds_total 0.005
```

That is the output after one appended frame, one 14-byte blob, and one staged release.

Oplog and storage numbers are current gauges. Ingest and sweep counters are cumulative and
persist in `catalog_meta`; deduplicated uploads are not counted as new bytes. Release stage
latency is measured by `workspace release stage` and survives restarts. There is no network
listener: run the command from a cron job or scrape it through your own wrapper.

## Sharing and cloning

`share` prints a reference that another data directory can adopt. It includes each volume
id and remote log id (catalog, oplog, and optionally the archive), because a volume id alone
would make Graft mint a new remote
log and show an empty workspace:

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

```sh
REF=$(workspace share | sed -n 's/^ref: //p')
workspace clone "$REF" --data data-laptop
workspace pull --data data-laptop
```

`sync` pushes local commits, `pull` fast-forwards from the remote, and `clone` attaches
both volumes, pulls, then opens the catalog. Clone refuses a data directory that already
holds a workspace. There is no merge: do not write to the same volumes from two data
directories at once. Media bytes are not part of Graft, so a clone needs access to the same
`[media]` store. The [clone tutorial](/docs/workspace/clone/) walks through a full example.

## Status and storage layout

`status` is the quick health check:

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

The soft-deleted `notes/from-desktop` is not counted; `entries` reports live rows.

With an S3-compatible remote, both volumes sync through `autosync`, `workspace sync`, or
Graft's pragmas. `autosync` only ticks while a process is alive, so one-shot commands
should push with `sync`. The volume tags are the SQLite database names, so the same tag
always resolves to the same volume inside a data directory.

## Current limits

- **No emthin-compatible client yet.** `workspace serve` speaks the coordinator transport,
  but emthin's framing differs; one side still needs an adapter.
- **Compaction is idle-driven while `serve` runs.** The snapshot is stored in the oplog and
  delivered to joining clients. `workspace doc compact` does the same for one producer,
  but there is no compaction while no coordinator is running. Snapshots larger than one
  transport frame are sent as bounded begin/chunk/end frames, so document size is not the
  compaction limit.
- **Live archive GC is manual.** `workspace blob gc-cdc` rewrites the live archive volume;
  nothing schedules it, and old volumes are left for you to delete after a grace period.
  The encrypted backup archive is collected automatically when retention removes runs, but
  its previous volumes are also left in place.
- **Backup scheduling is opt-in.** `workspace backup --every SECS --keep N` runs in the
  foreground, and `workspace service generate` only writes unit files; nothing installs
  them. The backup covers the catalog, raw blobs, git bundles, oplog frames and Loro
  snapshots. `cdc` bytes are excluded unless `[backup.archive]` is configured, in which
  case they are copied only into the separate encrypted Graft remote.
- **Metrics are a CLI snapshot.** `workspace metrics` has no HTTP endpoint and no
  authentication; wrap it if you need scraping.
- **One writer per data directory.** Graft enforces a single writer per volume; opening the
  same `data_dir` from two processes will fail rather than merge.
- **Alpha.** Object storage is the source of truth; keep snapshots and bundles for
  anything you cannot regenerate.
