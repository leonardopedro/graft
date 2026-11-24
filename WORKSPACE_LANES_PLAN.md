# Workspace storage lanes — implementation plan

Status: **proposal, 2026-11-21.** Scope: this repository (`graft`) plus the sibling
checkouts `../unfer`, `../australVM`, `../emthin`. It replaces the Cloudflare-specific
"parallel multi-lane workspace" draft with the same four-lane idea routed onto the storage
and runtime pieces these four repos actually have.

Binary update distribution is a separate surface built on the same FastCDC and
content-addressed storage primitives; its plan and implementation status live in
[`BINARY_UPDATES_PLAN.md`](BINARY_UPDATES_PLAN.md).

The short version:

- **`graft` is the control plane**, not a fourth storage product to invent. One Graft volume
  per workspace holds the catalog as a SQLite database through the Graft VFS; object storage
  is any S3-compatible endpoint (`RemoteConfig::S3Compatible`).
- **Live documents already exist**: `emthin` speaks a Loro version-vector/reliable-stream
  protocol over its IPC (`collab_version`/`collab_join`/`collab_delta`/`collab_ack`), and
  `unfer` already has a Loro durable store. There is no Durable Object to build.
- **Media should not be chunked by default.** Split streamable raw objects (HTTP Range) from
  content-addressed archives (`cdc-volume`) instead of forcing every video through a
  deduplicating chunk store.
- **Zenodo is already implemented** as `uz_*` in `unfer_ffi/src/zenodo.rs` with
  snapshot/delta + versioned depositions. Extend it for multi-file release bundles; do not
  write a second Zenodo client.
- **Jujutsu is the one genuinely new, risky dependency.** Ship versioned code without it
  first (tree manifests + git bundles), then add the `jj-lib` backend behind a feature flag.
- **Two blockers found before any of this can run**: `../emthin` does not compile today
  because `crates/emthin-collab` is declared as a workspace member/dependency but is missing
  on disk, and no repo has any `jj`, Graft, or workspace-catalog integration yet.

---

## 0. What changed from the source plan

| Source-plan component | What this stack actually uses | Why |
|---|---|---|
| cloudflare-os **Workspace Durable Object** as router/catalog | `graft-workspace` daemon (new crate here) + one **Graft volume** per workspace, opened through the Graft SQLite VFS | No Cloudflare dependency in scope; a volume already gives single-writer SSI, lazy replication, snapshots and an S3-backed remote |
| **Durable Object SQLite** for catalog + oplog | Catalog: SQLite inside the catalog volume. Oplog: a **separate** `loro` volume | Putting edit traffic in the catalog volume makes every keystroke contend with catalog writes |
| **R2 buckets** `ws-graft-pages` / `ws-loro-snapshots` / `ws-media-blobs` | One S3-compatible bucket, prefixes `graft/`, `loro/`, `media/raw/`, `media/cdc/`, `staging/` | `RemoteConfig::S3Compatible` only takes bucket + prefix; prefixes give the same separation |
| cloudflare-os **S3 Gatekeeper** capability tokens | `australVM` `ManifestAuthEngine` + a new `[grants] storage` namespace + opaque handles from `unfer`'s `uw_*` ABI | The choke point (`auth::check`) already exists and is already the only way module code reaches a side effect. Client-side presigned URLs leak the capability; handles do not |
| **Loro WASM in DO** + WebSocket | Loro (Rust) in `unfer_ffi::durable` and in the restored `emthin-collab`, synced over the existing `collab_*` framing | Same CRDT, same protocol shape, no broker rewrite |
| **Workers / Gadgets** | unfer modules hosted by `australVM`'s `ModuleHost` (native Austral/Rust, workerd for ECMAScript) | Already implemented and grant-gated; see `unfer/docs/PLAN_cloudflare_os_adaptation.md` |
| `jj-lib` over Graft + DO SQLite | Phase 4a: tree manifests + git bundles in `cdc-volume`; Phase 4b: feature-gated `graft-jj` | `jj-lib` is not present anywhere, is pre-1.0, and a full `OpStore` is a large, churning surface |
| Zenodo as cold/public archive only | Existing `uz_*` client extended with `uz_bundle_*`, behind deferred approval | Publishing mints an irreversible DOI; it must not be a fire-and-forget call |
| "Record DOI back with `lane = 'zenodo_ref'`" | `releases` table + `entries.release_doi`; the entry's lane does not change | A video does not stop being media because it was archived. Overwriting `lane` destroys routing |

### Additional corrections that matter in practice

1. **Content hashes are not trust boundaries.** Clients compute hashes for de-duplication,
   but the coordinator re-hashes on commit. `blake3` is what Graft and `cdc-volume` already
   use; record `sha256` too when a Zenodo/dataset consumer needs a portable digest.
2. **There is no cross-lane transaction, and there does not need to be.** Blobs and git
   objects are immutable and content-addressed; only the catalog row is mutable. Order is
   always *write bytes → fsync/verify → commit catalog row*. An orphaned blob is GC-able; a
   catalog row without bytes is not allowed.
3. **Lazy checkout is not a thing for a working copy.** Graft laziness helps `log`, `diff`,
   `annotate` and catalog reads. `jj workspace add` still materialises files. Do not promise
   instant sandboxes.
4. **One writer per volume.** Clients never write the catalog volume directly. They send
   mutations to the coordinator; they may replicate the catalog read-only through
   `graft-sqlite`.
5. **Release the GPL boundary.** `emthin` is GPL-3.0; `graft`, `unfer` and `australVM` are
   MIT/Apache-2.0. Keep `emthin-collab` a standalone crate that depends on Loro and nothing
   from this repo; integrate over the IPC/coordinator wire protocol, never by copying
   `emthin` code into Graft.

---

## 1. Target architecture

```
                       ┌─────────────────────────────────────────────┐
  emthin (document UI) │            `graft-workspace` daemon          │
  unfer modules  ─────▶│  catalog (SQLite on Graft VFS)               │
  australVM grants     │  lane router · blob ingest · release builder │
  agents (uw_* ABI)    └───────┬──────────────┬──────────────┬───────┘
                               │              │              │
                        S3-compatible object storage (one bucket, prefixes)
                               │              │              │
                     graft/ (pages)   loro/ (oplog+snap)   media/ (raw+cdc)
                               │
                        Zenodo (immutable releases, DOI)
```

Design rules:

1. **Bytes before rows.** Immutable object first, catalog commit second.
2. **One authoritative writer per workspace** (the daemon). Read replicas are cheap.
3. **No ambient credentials in modules or clients.** They receive scoped handles; the daemon
   holds the S3/Zenodo secrets.
4. **Irreversible external effects are actions**, not calls: `uk_action_submit` →
   gatekeeper approval → daemon executes (existing F2/S4 machinery).
5. **Every new C symbol is censused.** Adding `uw_*` requires updating
   `unfer/unfer_ffi/EXPECTED_SYMBOLS*.txt`; `australVM/scripts/verify-invariants` and
   `symbol_sync` are the gate.

### 1.1 Routing matrix (adapted)

| Lane | Data | Engine | Hot store | Cold / public | Client surface |
|---|---|---|---|---|---|
| **Catalog** | paths, kinds, hashes, permissions, cross-lane links | SQLite on Graft VFS | catalog volume (`ws-catalog` tag) + DO-less daemon | `graft_export` snapshot / datapackage | `uw_catalog_*`, read-only `graft-sqlite` replica |
| **1 — Live** | notes, specs, RFCs, whiteboards, agent scratchpads | Loro (`loro` 1.13 in unfer; `mathed` in emthin) | per-doc sequenced frame log in the oplog volume (`<catalog_tag>-oplog`), not one object per frame | compacted snapshot + frontier on the catalog entry, markdown export | `workspace doc *`; emthin `collab_*`; `unfer_ffi::durable` |
| **2 — Versioned code** | source, configs, schemas, module manifests | Phase 4a: tree manifests + git bundle. Phase 4b: `jj-lib` | `cdc-volume` tree objects + bundles; jj objects later | git bundle / jj export per release | `uw_code_*`; `jj` CLI locally |
| **3 — Media** | photos, video, audio, model weights, datasets | raw S3 objects (Range); `cdc-volume` for archives | `media/raw/<hash>`; `media/cdc/<tag>` | dataset/media bundles to Zenodo | `uw_blob_*` |
| **4 — Archive** | milestones, DOIs, frozen bundles | `uz_*` Zenodo client | Zenodo draft/version | published record + DOI | `uw_release_*` |

### 1.2 Storage grants

New namespace, checked by `ManifestAuthEngine` at the same choke point as
`kernel`/`effects`/`net`/`fs`:

```toml
[grants]
storage = [
  "ws:notes/catalog:read",
  "ws:notes/loro:write",
  "ws:notes/media:write",
  "zenodo:deposit:stage",      # stage a bundle; publish still needs approval
]
```

Semantics:

- `ws:<workspace>/<lane>:<read|write|admin>`; `*` allowed only as the workspace segment.
- Grants are checked on **every** `uw_*` call, not once at open (defence in depth), and the
  host re-checks on the loopback as it already does for `uk_*`.
- `zenodo:deposit:stage` is distinct from publishing. Publishing is `uk_action_submit` with
  effect `zenodo.publish`, resolved by a gatekeeper module.
- `swap` must reject storage-grant escalation exactly as it does for `net`/`fs`
  (`australVM/safestos/cranelift/src/module.rs:545-590`).

---

## 2. Verified current state

This is what exists today; the phases below build only on these seams.

### graft (this repo)

- `RemoteConfig::{Memory, Fs{root}, S3Compatible{bucket, prefix}}`; S3 credentials come from
  standard AWS env vars and `AWS_ENDPOINT_URL` (`crates/graft/src/remote.rs:85-140`).
- `setup_graft(GraftConfig{remote, data_dir, autosync})` (`crates/graft/src/setup.rs:13-42`).
- `Runtime::{volume_reader, volume_writer, volume_get, volume_push, volume_pull,
  volume_snapshot, snapshot_checksum, snapshot_missing_pages, tag_replace, tag_get}`
  (`crates/graft/src/rt/runtime.rs:112-265`); `VolumeWriter::write_page/commit`,
  `VolumeRead::snapshot/read_page`.
- `cdc-volume` is a finished, tested content-addressed blob store over Graft pages (FastCDC,
  per-page AEAD, lazy reads, GC) and `cdc-cli` exposes it (`CDC_STORE_PLAN.md`).
- `graft-sqlite` registers a VFS whose **database name is a volume tag**
  (`crates/graft-sqlite/src/vfs.rs:118-140`), with pragmas `graft_status`, `graft_push`,
  `graft_pull`, `graft_fork`, `graft_checkout`, `graft_snapshot`, `graft_export`,
  `graft_hydrate` (`crates/graft-sqlite/src/pragma.rs:58-110`) and static registration via
  `register_static(vfs_name, make_default, GraftConfig)`.
- `graft-tool` only mints ids (`crates/graft-tool/src/main.rs:22-29`); no workspace commands.

### unfer

- `LoroDurableStore` already does snapshot-on-flush with atomic tmp+rename, dirty-driven
  coalescing, frontier for interrupted-flush detection, and fail-visible corrupt-snapshot
  recovery (`unfer_ffi/src/durable/loro.rs:1-80`).
- `DurableStore` contract: `append`, `flush`, `replay`, `frontier`, `fork_at`,
  `persist_count`, `stream_len`, `backend` (`unfer_protocol/src/durable.rs:55-95`).
- `uz_*` Zenodo client: draft creation, new-version on later pushes, bucket upload, publish,
  `manifest.json` with `file_sequence` + `last_frontier`, squash after 50 deltas
  (`unfer_ffi/src/zenodo.rs:81-370`); symbols listed in
  `unfer_ffi/EXPECTED_SYMBOLS_ZENODO.txt`.
- Module manifests + grants already demonstrate the approval pattern
  (`client_module/module.toml`, `gatekeeper_module/module.toml`,
  `durable_status_module/module.toml`).

### australVM

- `ModuleManifest` parses `kernel`, `effects` (incl. `effect_kind = "observe"`), `observers`,
  `resources`, `fs`, `net` (`safestos/cranelift/src/module.rs:29-201`).
- `auth::check` / `ManifestAuthEngine` is the single choke point; `swap` diffs grants and
  rejects escalation (`module.rs:545-590`).
- `scripts/verify-invariants` enforces the symbol census; `unfer_ffi/EXPECTED_SYMBOLS*.txt`
  is the other half.
- `examples/zenodo/ZenodoStore.aui` is the pattern for typed Austral bindings to a new ABI.

### emthin

- Session persistence is a full Loro snapshot per save (`MathDoc::snapshot()` →
  `session.loro`, plus `session.json` for launch commands;
  `crates/emthin/src/session.rs:1-20,249`).
- The collaboration protocol is fully specified in `docs/ipc.md:141-190`: opaque version
  token, join with version, sequenced delta frames with buffering/dup detection, ack-driven
  retention, leave.
- **`crates/emthin-collab` exists again** (`Exchange`, `Line`, `Session`, `wire`,
  `unix::Host`/`UnixLink`, grants, anchors; ~4.4k lines) and is wired into emthin's IPC as
  `collab_*` with an end-to-end test in `crates/emthin/tests/collab_ipc.rs`. `cargo metadata`
  succeeds. It is GPL-3.0, so graft must talk to it over a socket rather than link it.
- **The compile blocker is gone.** `velysterm` was updated to `00f24a7` (2026-11-22), which
  supplies the missing `MathDoc::{version_vector, export_delta_since, create_anchor,
  resolve_anchor}` and `DocError::BadVersion`. `cargo check -p emthin-collab` succeeds, and
  `cargo test -p emthin-collab` passes 65 tests. Phase 0's compile gate is green on the emthin
  side; `cargo check -p emthin` still needs the `nix develop` shell for `glib-sys`.
- Its transport is **not** graft's framing: `wire` is big-endian length + `u8` tag with
  `Hello{peer,version}`, `Delta{seq,delta}`, `Ack`, `Presence` and v2 `HelloV2{share_id,
  epoch}`/`Resync`/`DeltaV2`, while graft's `transport` uses little-endian length with
  `Hello{doc,peer,cursors}`, `Delta`, `Ack`, `Bye`, `CaughtUp{frontier}` and `Error`. There
  is no `transport.rs` and no graft reference anywhere in emthin. One side needs an adapter
  before a client can reach `workspace serve`.

### Missing everywhere

- Jujutsu (`jj-lib`, `jj-cli`) — zero references in all four repos.
- Any Graft dependency or usage in unfer/australVM/emthin.
- Any workspace catalog, blob ingest, release bundle, or collab network transport.

---

## 3. New components

| Component | Repo | Path | Kind |
|---|---|---|---|
| `graft-workspace` | graft | `crates/graft-workspace/` | lib + `workspace` bin: catalog, lanes, ingest, releases |
| catalog schema + migrations | graft | `crates/graft-workspace/src/catalog/` | SQLite, `STRICT` tables, versioned migrations |
| workspace SQL tests | graft | `tests/sql/test_workspace_catalog.sql(.expected)` | `just run sqlite test` fixtures |
| `graft-jj` (Phase 4b, feature-gated) | graft | `crates/graft-jj/` | `jj_lib::backend::Backend` over `cdc-volume` |
| workspace protocol types | unfer | `unfer_protocol/src/workspace.rs` | `Lane`, `WorkspaceEntry`, `BlobRef`, `ReleaseBundle` |
| `uw_*` C ABI | unfer | `unfer_ffi/src/workspace.rs` | handle/capability storage API |
| workspace tool symbols | unfer | `unfer_ffi/EXPECTED_SYMBOLS_WORKSPACE.txt` | census |
| collab session crate | emthin | `crates/emthin-collab/` | restore missing crate; transport trait |
| coordinator transport | emthin | `crates/emthin-collab/src/transport.rs` | framed version/join/delta/ack over Unix socket or TCP |
| storage grants + manifest | australVM | `safestos/cranelift/src/module.rs`, `auth.rs` | `[grants] storage` parsing, subset/escalation |
| Austral bindings | australVM | `examples/workspace/WorkspaceStore.aui` | typed `uw_*` bindings |
| demo modules | unfer | `workspace_module/`, `workspace_gatekeeper_module/` | positive + deny + publish-approval paths |

### 3.1 Catalog schema

The pasted schema conflated lanes, blobs and archival references. Split them:

```sql
CREATE TABLE catalog_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;

CREATE TABLE blobs (
  hash        TEXT PRIMARY KEY,          -- 'blake3:<hex>' (canonical)
  sha256      TEXT UNIQUE,               -- set only when a portable digest is needed
  size        INTEGER NOT NULL,
  mime        TEXT,
  storage     TEXT NOT NULL CHECK (storage IN ('raw', 'cdc')),
  locator     TEXT NOT NULL,             -- S3 key or '<cdc-tag>/<path>'
  refcount    INTEGER NOT NULL DEFAULT 0,
  pinned      INTEGER NOT NULL DEFAULT 0, -- 1 once referenced by a published release
  created_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE entries (
  workspace_id TEXT NOT NULL,
  path         TEXT NOT NULL,
  lane         TEXT NOT NULL CHECK (lane IN ('loro', 'code', 'media', 'archive')),
  kind         TEXT NOT NULL CHECK (kind IN ('doc', 'file', 'dir', 'dataset', 'release')),
  blob_hash    TEXT REFERENCES blobs(hash),
  loro_doc_id  TEXT,
  loro_frontier BLOB,
  code_tree    TEXT,                     -- tree manifest hash (code lane)
  release_doi  TEXT,
  mime         TEXT,
  size         INTEGER,
  revision     INTEGER NOT NULL DEFAULT 0,
  deleted_at   INTEGER,
  updated_at   INTEGER NOT NULL,
  PRIMARY KEY (workspace_id, path)
) STRICT;

CREATE TABLE releases (
  doi          TEXT PRIMARY KEY,
  concept_doi  TEXT,
  workspace_id TEXT NOT NULL,
  manifest_hash TEXT NOT NULL,
  zenodo_record INTEGER NOT NULL,
  published_at INTEGER NOT NULL
) STRICT;

-- sequencing, frames and acks live in the oplog volume (`crates/graft-workspace/sql/oplog.sql`),
-- not in the catalog: ack traffic must not contend with catalog writes
```

Notes:

- `entries.path` is unique **per workspace**, not globally; the old `path PRIMARY KEY` would
  collide the moment two workspaces share a catalog process.
- `blobs.refcount` is advisory. Liveness is computed by walking `entries` plus pinned
  releases, because refcount drift after a crash is otherwise permanent.
- `oplog_streams` is small (one row per doc/peer). The actual frames live outside the
  catalog volume; putting a hot oplog in SQLite here would reintroduce the source plan's
  contention bug.
- Never store thumbnails or dimension JSON in `entries`; put them in a sidecar blob or a
  `blob_meta` table added later.

### 3.2 `uw_*` ABI (unfer)

Same buffer protocol as `uk_*`/`uz_*`: `>= 0` bytes written, `< 0` = `-code`, probe with
`buf = null, cap = 0`. Opaque handles, never keys.

| Symbol | Purpose |
|---|---|
| `uw_workspace_open(cfg_json) -> handle` | bind workspace id, catalog tag, S3 prefix; no credentials exposed to the caller |
| `uw_workspace_close(handle)` | release |
| `uw_entry_put(handle, entry_json) -> revision` | upsert catalog row (requires `catalog:write`) |
| `uw_entry_get(handle, path, buf, cap)` | read row JSON |
| `uw_entry_list(handle, prefix, buf, cap)` | list rows under a prefix |
| `uw_blob_begin(handle, size, hash, mime) -> ticket_json` | returns an upload ticket/handle; for raw media, a short-lived presigned PUT |
| `uw_blob_commit(handle, ticket) -> blob_json` | verify size+hash, canonicalise, insert `blobs` row |
| `uw_blob_read(handle, hash, offset, len, buf, cap)` | Range read through the daemon |
| `uw_code_snapshot(handle, path) -> tree_hash` | Phase 4a tree manifest + optional git bundle |
| `uw_release_stage(handle, bundle_json) -> staging_id` | assemble files + datapackage |
| `uw_release_publish(handle, staging_id) -> action_handle` | submits a `zenodo.publish` action; returns pending |
| `uw_release_status(handle, action_handle, buf, cap)` | merged pending/approved/rejected result |
| `uw_last_error(buf, cap)` | thread-local diagnostic |

`uw_release_publish` deliberately does **not** publish. It stages and submits. Only a
gatekeeper resolution (`uk_action_apply`) lets the daemon call `uz_*` publish.

---

## 4. Phases

Sizes are relative: **S** ≈ a day, **M** ≈ a week, **L** ≈ multi-week. Every phase must keep
its own repo green before the next starts.

### Phase 0 — unblock the workspace, then the catalog (M)

Goal: `cargo metadata`/build works across the four repos, the coordinator exists, and the
catalog is queryable through Graft SQLite.

> **Status (2026-11-22):** implemented in `crates/graft-workspace` — schema, migrations,
> catalog API, `workspace` CLI (`init`, `status`, `entry`, `doc`, `blob`, `sync`, `pull`,
> `share`, `clone`), unit/e2e tests and the `tests/sql/test_workspace_catalog.sql` fixture.
> `workspace serve` now binds a Unix socket and relays framed `Hello`/`Delta`/`Ack`/`Bye`
> messages: a joining client receives the oplog tail after its cursors, then live frames;
> gaps are reported without dropping the connection. `tests/transport.rs` covers broadcast,
> reconnect-from-cursor and gap handling. The emthin client that speaks this framing is
> still Phase 1 work.
>
> Documentation lives in `docs/src/content/docs/docs/workspace/` (`quickstart.md`,
> `clone.md`, `index.md`) and `concepts/layout.md`. One captured run backs the guides, and
> `crates/graft-workspace/tests/doc_examples.rs` replays it: it asserts the stable
> transcripts, checks that every executed command and output still appears on its page, and
> verifies that all pages show the same workspace id and reference. `layout.md` is checked
> against the real schema files and a live workspace. `tests/cli_docs.rs` regenerates the
> CLI reference and fails if it is stale. CI runs the doc-example test in its own ungated
> `docs-examples` job, so even a docs-only change is checked for guide drift, while
> `docs-check.yml` builds the site and link-checks the built pages.

Work:

1. **Restore `emthin/crates/emthin-collab`.** Minimum viable crate implementing exactly the
   call-site contract in §2: `Session`, `Outbound`, `ReceiveOutcome`, `SessionError`, Loro
   delta export/import, per-peer sequenced buffering, duplicate rejection, gap reporting,
   ack-based retention (`retain_ms`). No transport yet; it is a pure state machine.
   Gate: `nix develop -c cargo test -p emthin-collab`; `cargo metadata` succeeds; existing
   `emthin` tests still pass.
2. **`graft-workspace` skeleton**: `setup_graft` from a `workspace.toml`
   (`remote = { S3Compatible = { bucket, prefix } }` or `Fs` for tests), register the VFS
   with `register_static("graft", false, cfg)`, open the catalog database under the
   `ws-catalog` tag, apply migrations, expose `status`/`entry`/`blob` subcommands in the
   `workspace` binary.
3. **Schema + SQL fixtures**: the §3.1 tables; `tests/sql/test_workspace_catalog.sql` with
   `just run sqlite test` coverage of upsert, per-workspace path uniqueness, lane checks,
   and FK behaviour.
4. **Unfer protocol types** (`Lane`, `WorkspaceEntry`, `BlobRef`, `ReleaseBundle`) with
   serde round-trip tests; no ABI yet.

Exit gate: a local `Fs` remote; create catalog, insert/read rows, kill and reopen, verify
`graft_status`/`graft_snapshot` and snapshot checksum stability. No network, no Zenodo.

Deliberately not in this phase: `uw_*` symbols, S3, Loro sync, media.

### Phase 1 — Lane 1: live documents (M)

Goal: two emthin instances (or emthin + an agent) edit the same document through the
coordinator, with the same framing the IPC already defines, and survive restarts/offline.

> **Status (2026-11-22):** the storage half is implemented in `crates/graft-workspace`:
> `Oplog` (sequenced per-producer frames, `Applied`/`Duplicate`/`Gap`, cursors, monotonic
> clamped acks, explicit prune, summaries) in its own Graft volume, and `Workspace::join` /
> `submit` / `ack` / `note_snapshot` with `workspace doc *` commands. The coordinator side of
> the socket transport is built (`workspace serve`, see Phase 0). Compaction is now
> ack-guarded: `workspace doc compact --producer P --through N` deletes frames only when
> every known consumer has acked through N, refuses a frontier the coordinator never
> applied, and is covered end-to-end (`tests/cli.rs`) as well as in unit tests. Compaction
> now materializes a Loro snapshot and stores it in the oplog before deleting anything;
> joining clients behind its frontier receive a `Snapshot` frame, and backups carry the
> same snapshot plus the remaining frame tail. Snapshots larger than one transport frame
> use bounded begin/chunk/end frames plus a validated client-side `SnapshotAssembler`, so
> compaction is not limited by document size. The tests cover a client that drops after
> two chunks and reconnects from empty cursors, receiving a fresh complete stream; shutdown
> now wakes and joins active connection threads. `workspace serve` runs that pass after 300
> seconds without a committed frame or ack
> (`--compact-after-secs`, `0` disables), and the oplog schema migrates v1 databases in
> place. Not yet built: emthin's two-tier session persistence and client transport, and
> WebSocket bridging.

Work:

1. **Two-tier persistence.** Keep `emthin`'s standalone `session.loro` for the no-workspace
   case. In workspace mode, after `local_edit`, append the exported delta to an append-only
   oplog file (`loro/oplog/<doc>/<seq>.bin`, length-prefixed, fsync before ack) and update
   `oplog_streams`. On open, read the newest compacted snapshot from
   `loro/snap/<doc>/<frontier>` then replay only the tail. Compact when the oplog exceeds
   N frames or M minutes of idle; delete only after the snapshot commit.
   Reuse `unfer_ffi::durable::LoroDurableStore` semantics (dirty coalescing, atomic rename,
   corrupt-snapshot preservation) rather than inventing a second snapshot path.
2. **Transport.** Add `emthin-collab/src/transport.rs`: framed frames
   `Hello{peer, version}`, `Snapshot{frontier, bytes}`, `Delta{peer, seq, bytes}`,
   `Ack{peer, seq}`, `Bye`. The coordinator is a relay + durable oplog; ordering is per
   peer stream, exactly as `docs/ipc.md` requires. Authentication is a workspace grant,
   not an ambient socket path.
3. **Coordinator sync loop.** `graft-workspace` gets a `loro` lane: `uw_doc_join`,
   `uw_doc_delta`, `uw_doc_ack` (internal for now; not in the public ABI until Phase 3).
   Broadcast to other peers, persist to the oplog, ack after fsync. Clients reconnect with
   their last frontier and receive only the missing deltas.
4. **Catalog integration.** A document save updates `entries` (`lane='loro'`,
   `loro_frontier`) at snapshot boundaries only, not per keystroke.
5. **Promotion out.** `uw_doc_export(doc, format=markdown|json)` produces the cold artifact
   used later by releases.

Exit gate:

- Two emthin clients converge after 100 random edits with one client disconnected for part
  of the run; no duplicate application; replay after `SIGKILL` of the coordinator loses no
  acked frame and produces a byte-identical document.
- Frontend cost: reconnecting with a current frontier transfers 0 frames.
- `cargo test -p emthin-collab` covers duplicate/gap/ack-clamp/retention.

Not in this phase: WebSocket/HTTP transport, presence UI, agent scratchpads beyond a second
peer.

### Phase 2 — Lane 3: media (M)

Goal: upload a 2 GB video, stream it back with Range requests, dedup a re-upload, and keep
it out of both the CRDT and the catalog volume.

> **Status (2026-11-22):** raw ingest and range reads are implemented in
> `crates/graft-workspace` (`[media]` config → its own object store, streaming ingest with
> hash-while-writing, content-addressed canonical objects, catalog dedup, `blob put|get|ls|
> show|sweep-staging|gc`). `blob gc` removes raw canonical objects that no entry references
> and that no release pins, deleting the catalog row before the object so an interrupted
> sweep leaves an orphan rather than a row whose bytes are gone. The `cdc` storage class is
> now implemented: `[archive] key_file` opens a third, encrypted `cdc-volume` tagged
> `<catalog_tag>-archive`; `blob put --storage cdc` chunks, deduplicates and AEAD-encrypts
> the payload through that store; `blob get` range-reads `cdc` rows via the chunk index;
> archive volume ids/logs are included in `share`, `clone`, `sync` and `pull`. Tests cover
> dedup, ranges, archive-without-media, malformed keys, and an end-to-end
> sync/share/clone round trip. `workspace blob gc-cdc` removes unreferenced `cdc` rows and
> manifests, then rewrites the archive through `cdc-volume`'s mark-and-sweep and repoints
> the tag at the copy; the old volume is kept for a grace period until the next sync or
> clone. The rewrite re-keys chunk ids (the same shape a master-key rotation would take,
> though the command keeps the configured key). Presigned ingest is exposed as
> `blob presign` / `blob commit`: opendal's S3 backend issues a short-lived PUT, the client
> supplies size and blake3, and commit accepts only this workspace's staging keys, checks
> size, re-hashes while copying to canonical storage, drops staging, and dedups. Backends
> without presign return `PresignUnsupported` and keep the streaming `blob put` path. Not
> yet built: the `uw_blob_*` ABI over these operations, and scheduling GC automatically.

Work:

1. **Raw ingest protocol.** `uw_blob_begin` → short-lived presigned PUT to
   `staging/<workspace>/<hash>` (via opendal's presign; verify provider support) or, when
   presigning is unavailable, a daemon-streamed upload. Client computes `blake3`; the daemon
   re-verifies size and hash in `uw_blob_commit`, copies to `media/raw/<workspace>/<hash>`,
   inserts `blobs(storage='raw')`, then deletes staging.
2. **Dedup.** `uw_blob_commit` first checks `blobs.hash`; an existing row creates a new
   `entries` path pointer without re-upload. Refcount is recomputed, not blindly incremented.
3. **Range reads.** `uw_blob_read` streams `offset/len`; document that clients may be handed
   a short-TTL presigned GET instead of proxying, and that the capability is scoped to one
   object. Never expose the bucket key.
4. **Archive class.** Datasets and bundles where dedup/encryption pay off go through
   `cdc-volume` (`storage='cdc'`, `locator='<cdc-tag>/<path>'`). `cdc-volume` already has GC
   and per-page AEAD; use it rather than reimplementing chunking.
5. **Sweeper.** A catalog-driven pass removes staging objects older than TTL and raw objects
   whose `blobs` row has no entries and is unpinned. Do not add a second, independent GC for
   CDC data; call `cdc-volume`'s.

Exit gate:

- Upload/interrupt/resume produces exactly one canonical object; a second upload of the same
  bytes performs zero byte copies.
- Range read of `[1 GiB, 1 GiB+64 KiB)` returns the expected bytes and a correct
  `content-range`; a wrong hash is refused and leaves no catalog row.
- Crash between upload and commit leaves only staging garbage, which the sweeper removes.

Not in this phase: thumbnails, EXIF extraction, transcoding, CDN/CORS policy.

### Phase 3 — Lane 4: archival publishing (M)Goal: one command turns a workspace milestone into a citable Zenodo record with a
 datapackage, and external readers can fetch ranges without the daemon in the path.

> **Status (2026-11-22):** bundle assembly is implemented in `crates/graft-workspace`:
> `workspace release stage --entry <PATH> --out <DIR>` copies media blobs and every file in
> selected code trees into `files/`, verifies each blake3 while copying, and writes
> `datapackage.json` with blake3 + sha256, byte count, lane and entry path per resource. The
> datapackage hash is the manifest hash; staging refuses a non-empty directory and paths
> that escape it. Not built: the Zenodo bundle/deposit calls, gatekeeper approval, DOI rows
> in `releases`, blob pinning on publish, and archive range reads. Those need `uz_*` work in
> unfer, australVM bindings, and a sandbox token.


Work:

1. **Extend `uz_*` instead of forking it.** Add:
   - `uz_bundle_begin(meta_json)` — new deposition or new version from a concept record;
   - `uz_attach(name, bytes)` — arbitrary file into the current draft;
   - `uz_attach_file(name, path)` — stream a local/daemon-materialised file in bounded
     chunks (used for large media pulled from S3);
   - `uz_publish() -> doi_json` — existing publish path, returns DOI + concept DOI + links;
   - keep `uz_push`/`uz_pull`/`uz_manifest_json` untouched for the Loro-only flow.
   Update `EXPECTED_SYMBOLS_ZENODO.txt` and the Austral bindings together.
2. **Bundle assembly** (`uw_release_stage`):
   - Loro lane → markdown/JSON exports + a frozen `.loro` snapshot;
   - code lane → `git bundle create repo.bundle --all` (Phase 4a output) so the record is
     self-contained without Graft;
   - media lane → only files reachable from the selected entries, streamed from S3;
   - catalog → `datapackage.json` with `name`, `path`, `bytes`, `hash` (`sha256` when
     computed), `lane`, and the workspace/revision ids.
   Stage into `release/<staging-id>/` and record `manifest_hash`.
3. **Approval path.** `uw_release_publish` submits `ActionRecord { effect:
   "zenodo.publish", payload: { manifest_hash, staging_id } }` and returns the action
   handle. `workspace_gatekeeper_module` lists pending actions, inspects the manifest, and
   applies or rejects. The daemon publishes only on `approved`, using the secret held
   outside module reach.
4. **DOI → catalog.** On success insert `releases` (`doi`, `concept_doi`, `zenodo_record`,
   `manifest_hash`), set `entries.release_doi` for the release entry, and pin every blob in
   the bundle (`pinned=1`) so GC cannot invalidate a DOI.
5. **Range reader.** `uw_archive_read(doi, name, offset, len)` resolves the record file link
   and issues an HTTP Range request; verify against the datapackage digest. This mirrors
   Graft's lazy read for frozen archives.

Exit gate:

- Sandbox Zenodo round-trip: stage → reject (no record) → stage → approve → publish → DOI
  retrievable; a second publish of the same staging id is refused as already resolved.
- `datapackage.json` lists exactly the deposited files and every hash matches on download.
- Pull of a 100 MB record via two range reads equals the full file; a corrupted local copy
  fails verification.
- Removing `zenodo:deposit:stage` or `uk_action_apply` from the module manifests produces
  UK-4001/denial, not a partial upload.

Not in this phase: automatic release on every save, ORCID/metadata templates, citation
rendering.

### Phase 4 — Lane 2: versioned code (L, split)

Goal: code checkpoints are versioned, deduplicated, restorable, and optionally editable with
Jujutsu. This is the phase to keep small: the pasted plan's `jj-lib` requirement is the
largest risk in the whole design and is not needed for value.

#### 4a — code snapshots without jj (M)

> **Status (2026-11-22):** implemented as `workspace code snapshot` / `code restore` in
> `crates/graft-workspace`. A checkpoint is a JSON tree manifest (path, blake3, size, mode)
> stored in the encrypted `cdc-volume`; unchanged files dedup across checkpoints at chunk
> level. Git trees are enumerated with `git ls-files --cached --others --exclude-standard`
> and get a `git bundle` of all refs; other trees skip `.git` and honour
> `.workspaceignore`. Restore streams in 64 KiB chunks, verifies each file's blake3 and size
> before renaming it into place, and rejects `..`/absolute manifest paths. Checkpoint blobs
> are pinned so `blob gc-cdc` cannot collect them. The archive volume is part of
> `WorkspaceRef`/sync/clone, so checkpoints survive adoption; `[archive]` is now required
> for this lane. 4b (`graft-jj`) is still gated and not started.

1. `uw_code_snapshot(handle, path)`: walk a working tree, exclude via `.gitignore` plus an
   explicit `workspaceignore`, write each file to `cdc-volume` (dedup by content), write a
   tree manifest blob (sorted path → hash → mode/size), commit `entries(kind='file'|'dir',
   code_tree=...)`.
2. When the tree is a git repo, also produce `repo.bundle` (`git bundle create --all`) as a
   blob and record its hash. This is both the release artifact for Phase 3 and the required
   backup for the alpha-storage risk in §5.
3. Restore: `workspace restore <tree> <dir>` materialises files from `cdc-volume`, verifying
   hashes; a partial restore is allowed and resumable. No lazy checkout claims.
4. Catalog rows are written only after all blobs commit; an interrupted snapshot leaves
   unreferenced blobs, not a half-valid tree.

Gate: snapshot/restore a real tree (e.g. this repo), `git bundle verify`, mutate one file,
snapshot again, prove only the changed blob was added; restore reproduces byte-identical
content.

#### 4b — `graft-jj` backend (L, optional until 4a is in use)

Only start when 4a is green and there is a concrete workflow that needs jj's change model.

1. Pin an exact `jj-lib` version. Its library API is pre-1.0 and churns; the crate must be
   its own workspace member behind a non-default feature so the default build never depends
   on it.
2. Implement `jj_lib::backend::Backend` for immutable objects: `read_file`/`write_file`,
   trees, commits, symlinks, copy records. Map object ids to `cdc-volume` paths
   (`jj/objects/<id>`); reads are lazy (one chunked object at a time), writes are
   content-addressed and idempotent.
3. Op log: prefer jj's `SimpleOpStore` over the same backend instead of implementing
   `OpStore` from scratch. **Verify this against the pinned version before committing to the
   phase** — the module name/layout is the one detail here not checked against a checkout.
4. Workspaces: `jj workspace add` in a sandbox uses the normal local checkout; Graft
   laziness applies to `log`/`diff`/`annotate` object reads, not to materialising files.
5. Promotion bridge (Lane 1 → Lane 2): `uw_promote(doc, repo_path, message)` writes the
   exported markdown into the working copy and runs a local jj/git commit. Never pushes.
   Requires `catalog:read` + `loro:write`; audited.

Gate: create a repo, write commits, read them back through the backend; `jj log`, `jj diff`
and `jj show` work against the Graft-backed object store; a truncated object read is a clean
error (`BackendError::ObjectNotFound`), not a panic. CI compiles `graft-jj` in a separate
job, because the dependency will be large and may break between releases.

Not in this phase: a hosted jj remote, `jj git push` automation, stacked reviews UI, op-log
garbage collection across clients.

### Phase 5 — offline, backup, hardening (M)

> **Status (2026-11-22):** the restore drill is in place as
> `crates/graft-workspace/tests/restore_drill.rs`. It initialises a workspace, ingests a
> media blob, checkpoints a code tree, syncs and shares, copies the media root, clones into
> a fresh data directory, then reads the blob and restores the tree byte-for-byte from the
> new store. It runs against `RemoteConfig::Fs`. `workspace backup` now exports a
> consistent catalog snapshot, raw blob bytes, one verified git bundle per code checkpoint,
> one ordered frame log per document, and a materialized Loro snapshot per document to a
> separate `[backup]` store, writing `manifest.json` with object sizes and hashes last.
> `workspace restore <prefix>` refuses a non-empty workspace, restores the catalog, streams
> blobs to their original locators, replays frames, and verifies every object and snapshot.
> Encrypted `cdc` blobs are skipped unless `[backup.archive]` names a separate Graft
> remote: then each cataloged cdc file is decrypted in-process, re-encrypted into a backup
> `cdc-volume`, verified, and referenced by `archive.json` plus the manifest; restore copies
> it back and verifies the hash, so code checkpoints survive the drill. A compacted document
> carries its stored oplog snapshot plus the frame tail, so restore
> imports the snapshot and replays only newer frames; only a gap with no stored snapshot is
> marked partial and skipped with a warning. The
> in-repo security pass added regression tests for entry paths, backup/restore prefixes,
> manifest keys, blob locators, presign tickets, and malicious code manifests. `workspace
> backup --every SECS --keep N` is a foreground scheduled runner that applies retention to
> complete backup prefixes and removes older/incomplete runs; `--keep` also works after a
> one-shot backup. Retention also collects the encrypted backup archive when
> `[backup.archive]` is configured: CDC locators from every retained run's catalog form the
> live set, unreferenced files are deleted, and the remaining files are rewritten into a
> fresh volume whose reference replaces `archive.json`. `workspace service generate`
> writes systemd service/timer units or a launchd plist for one-shot scheduled backups,
> using an absolute config path and the generator's working directory; CI validates the
> files with `systemd-analyze verify` on Linux and `plutil -lint` on macOS. The transport
> suite now kills a coordinator subprocess mid-chunked-snapshot and proves a relaunched
> process reopens the same data directory and serves the snapshot from the beginning.
> `workspace metrics` prints Prometheus text: oplog gauges,
> per-storage blob gauges, persisted ingest and sweep counters, and release-stage latency
> totals. Not built: the emthin offline outbox and catalog read replica, and the external
> half of the security pass (`uw_*` audit and safestos grants).

1. **Offline clients.** emthin keeps a local Loro doc plus an outbox of unacked local
   deltas; on reconnect it sends `Hello{version}` and replays. Catalog reads use a read-only
   `graft-sqlite` replica; catalog writes queue as intents and are applied by the
   coordinator on reconnect (they may be rejected on conflict, which is fine and visible).
   Media cache is an LRU of raw objects keyed by hash.
2. **Backup job.** Scheduled: export the catalog (`graft_snapshot` + `graft_export`), a
   `git bundle` per code repo, and a Loro snapshot per active doc, into a separate
   `backup/` prefix and an off-site S3 target. Record the snapshot checksum. This is the
   mitigation for Graft's alpha status, not an alternative to it.
3. **Restore drill.** Restore catalog + one doc + one code tree into an empty bucket; verify
   catalog row hashes against restored blobs. Run it in CI with `RemoteConfig::Fs`.
4. **Security pass.** Confirm: no S3/Zenodo secret in module memory or blueprint archives;
   every `uw_*` audited with caller tag; storage grants re-checked on loopback; path
   traversal rejected on every user-supplied path; Zenodo publish always action-gated.
5. **Metrics.** Reuse unfer's existing audit/metrics surfaces for lane counters: oplog
   frames fsynced, compaction count, blob bytes ingested/swept, release latency. No new
   monitoring stack.

---

## 5. Failure modes and safeguards

| Failure | Behaviour | Mitigation |
|---|---|---|
| Blob written, catalog commit fails | Orphan object in `media/raw` | TTL sweeper; catalog is the source of truth |
| Catalog row committed, object missing | Should be impossible (write-bytes-first); detected on read | Read returns typed `MissingBlob`, entry flagged `revision+1`; restore drill catches systematic cases |
| Concurrent catalog writers | Graft rejects stale snapshot (`VolumeConcurrentWrite` → `SQLITE_BUSY_SNAPSHOT`) | Only the daemon writes; retry rebuilds the mutation from a fresh writer |
| Loro delta gaps / duplicates | Existing `Gap{expected}` buffering and duplicate drop | Ack-driven retention; reconnect with frontier |
| Oplog grows unbounded | Compaction threshold + idle timer | Snapshot then truncate; snapshot commit is the barrier |
| Presigned PUT never committed | Staging garbage | Staging GC; presign TTL |
| jj backend object corruption | Bad read | Content-addressed verification; git bundle fallback |
| Zenodo publish fails after upload | Draft/version left unpublished | Retry staging id; no DOI minted; action stays pending |
| Zenodo publish succeeds, catalog write fails | DOI exists without a row | Reconcile job scans deposits by manifest hash; DOI is immutable, so never delete |
| jj-lib API break | Build failure confined to `graft-jj` | Non-default feature + separate CI job; 4a remains the supported path |
| emthin GPL contamination | Licensing problem | `emthin-collab` depends only on Loro; no permissive-repo code copied from emthin |

Encryption policy (state it, don't hand-wave): `cdc-volume` pages are AEAD-encrypted.
**Raw** media is stored unencrypted at rest so HTTP Range works; use the `cdc` class for
anything sensitive. Document this per-entry rather than implying blanket encryption.

---

## 6. Verification matrix

| Repo | Command | What it proves |
|---|---|---|
| graft | `just test` (`cargo nextest run`, doctests, `just run sqlite test`) | engine + VFS + new catalog SQL fixtures |
| graft | `just build-all` | static/dynamic extension builds still work |
| graft | new `workspace` integration test with `RemoteConfig::Fs` | ingest/commit/sweep/restore without network |
| graft | `cargo test -p graft-workspace --test doc_examples` | guide transcripts replay; every example command and output is still on its page, and the pages agree on one captured run |
| unfer | `cargo test -p unfer_ffi` | `uw_*` buffer protocol, handle table, negative paths |
| unfer | `EXPECTED_SYMBOLS_WORKSPACE.txt` census + `symbol_sync` | ABI surface did not drift |
| unfer | `bash workspace_module/run_demo.sh` | grant allow/deny, publish-approval flow |
| australVM | `nix develop -c bash -c 'dune build @install && dune runtest'` | grant parsing, subset/escalation, JIT unchanged |
| australVM | `scripts/verify-invariants` | symbol census + auth default + archetype surface |
| emthin | `cargo metadata` (currently failing) | Phase 0 unblocked the build |
| emthin | `nix develop -c cargo test -p emthin-collab` | session machine: dup/gap/ack/retention |
| emthin | `./scripts/e2e/run.sh --config xdotool` | no regression in the compositor/E2E path |
| cross | two-client convergence + `SIGKILL` recovery test | Lane 1 durability claim |
| cross | sandbox Zenodo round-trip (token via env, never committed) | Lane 4 approval + DOI + range read |

Anything added to the C ABI also touches `unfer_ffi/EXPECTED_SYMBOLS*.txt`,
`australVM/examples/**/*.aui`, and the module manifests. Treat that as one atomic change with
three gates, not three independent edits.

---

## 7. Decisions and open questions

**Recommendations (defaults I would implement):**

1. **One catalog volume per workspace**, tag `ws-catalog-<workspace-id>`. Isolation and
   independent replica bootstrapping outweigh the convenience of one big catalog. A small
   registry volume (workspace id → catalog tag) can be added later.
2. **Media defaults to raw; archives use CDC.** Threshold suggestion: `cdc` for
   datasets/archives or explicit `sensitive=true`; raw for image/audio/video.
3. **Zenodo: one deposition per milestone**, versions linked by `concept_doi`. Never one
   giant mutable record per workspace; that makes "publish milestone 3" re-upload everything
   and makes DOIs ambiguous.
4. **Ship 4a, gate 4b.** No `jj-lib` in the default build.
5. **Coordinator is a local/daemon process, not a Worker.** It needs `jj`, SQLite and
   filesystem access; forcing it into an isolate buys nothing and costs the whole design.

**Open questions that need an owner decision:**

- Is a single coordinator per workspace acceptable operationally (single point of failure),
  or do we need a hot standby with the same Graft volume? Graft supports replicas, but only
  one writer.
- Do media uploads need to work from browsers without the daemon in the path? If yes, the
  presign capability must be minted per object and logged; this is the only place the source
  plan's CORS/Range requirement changes the security model.
- Is Zenodo's per-record file count/size limit acceptable for large datasets, or do large
  datasets need an external host with Zenodo holding only the manifest? Verify current limits
  before Phase 3; do not assume.
- Does `emthin-collab` need to be a general public crate, or is it an internal workspace
  crate? This decides how much of the transport belongs in it.
- Are agent scratchpads separate Loro docs or a subtree of the user's doc? Separate docs
  avoid accidental publication of scratch work, and match `loro_doc_id` in the schema.

---

## 8. Ordered first tasks

1. Restore `emthin/crates/emthin-collab` to satisfy its call sites; make `cargo metadata`
   pass. *(S)*
2. Add `crates/graft-workspace` with `workspace.toml`, `setup_graft`, VFS registration and
   the catalog schema/migrations. *(M)*
3. Add `tests/sql/test_workspace_catalog.sql` + expected output; wire into `just run sqlite
   test`. *(S)*
4. Add `unfer_protocol::workspace` types and serde tests. *(S)*
5. Wire the emthin collab state machine into the existing IPC handlers and prove the
   standalone (no network) path still passes. *(S)*
6. Add the `loro` lane oplog to the daemon + `uw_doc_*`; two-client convergence test. *(M)*
7. Implement `uw_blob_begin/commit/read` with `RemoteConfig::Fs` and presign behind a
   feature; dedup + Range + sweeper tests. *(M)*
8. Add `uz_bundle_begin/attach/publish` to `unfer_ffi`; update census + Austral bindings. *(M)*
9. Build `uw_release_stage` + `datapackage.json`, then the approval-gated publish path with
   the workspace gatekeeper module. *(M)*
10. Implement `uw_code_snapshot` + restore + git bundle; make it the backup mechanism. *(M)*
11. Only then: spike `graft-jj` against a pinned `jj-lib`, timeboxed, behind a feature. *(L)*

If only one thing is funded: tasks 1–6. That delivers the collaborative document lane on
durable infrastructure, which is the only lane where all four repos already meet.
