# Binary updates with FastCDC deltas — implementation plan

Status: **Phases 1–2 core implemented, 2026-11-23.** Scope: `graft`, primarily
`crates/graft-workspace`, using the FastCDC implementation in
`crates/cdc-volume/src/chunker.rs` and content-addressed object storage.

The short version:

- **The repository is an ostree-shaped object store**, not a patch server. A
  release is an immutable manifest; files are split with FastCDC into
  content-addressed chunks. Unchanged chunks are reused across files and
  releases.
- **Clients compute their own delta.** The client already has the previous
  release's manifest and files. It maps every chunk in its installed tree to the
  local file range that contains it, then fetches only target chunks that are
  absent. The server never computes per-client deltas and never sees the
  client's tree.
- **FastCDC is the delta algorithm.** The repository uses the existing
  `chunk_stream` (8 KiB minimum, 32 KiB target, 128 KiB maximum). An insertion
  near the start of a file only disturbs the chunks around the edit; the rest of
  the file keeps its boundaries and hashes.
- **Every layer is verified.** `ref → manifest hash → signature → file hash →
  chunk hash`. A wrong chunk is discarded, not installed. With `--public-key`,
  an unsigned or wrongly signed release is refused.
- **Updates are staged, then installed file by file**, and the installed-state
  file is written last. A crash leaves the old state and a re-runnable update,
  never a state file claiming files that were not written. Atomic deployments
  instead build a whole tree and switch a `current` symlink last.
- **Reachability is explicit.** GC reads every ref and manifest before deleting
  an unreachable manifest or chunk, with a grace period for concurrent
  publishes.

## 1. Why this is ostree-like, and where it differs

| ostree concept | This implementation |
|---|---|
| content-addressed object store | chunks and manifests in any `MediaConfig` store (`fs:` or `s3:`) |
| commit object | JSON release manifest, named by its blake3 hash |
| ref | `refs/<release>` holds the manifest hash |
| file objects / pack files | FastCDC chunks under `chunks/<blake3>` |
| local object cache | the installed tree or the previous deployment; chunks are reused from old files |
| deployment switch | in-place staging, or a versioned tree plus an atomic `current` symlink |

The important difference is scope. `update apply` updates a directory in place
and is the simplest path. `update deploy` also provides ostree-style immutable
deployment trees, an atomic symlink switch and rollback. In-place apply remains
available on every platform; deployments require Unix symlinks.

The other difference is compression. Ostree serves compressed static objects.
This version stores chunks raw by default and offers per-chunk zstd compression
with `publish --compress` or `--compress-level N` (default 3, range 1-22).
Compression never changes chunk identity or the delta algorithm: hashes and
deduplication still use the plaintext.

## 2. Repository format

Object keys, under a repo prefix (default `updates`):

```
<prefix>/<name>/refs/<release>              blake3 hash, or signed ref JSON
<prefix>/<name>/manifests/<manifest-hex>.json
<prefix>/<name>/chunks/<2>/<chunk-hex>      raw FastCDC chunk
<prefix>/<name>/chunks/<2>/<chunk-hex>.zst  zstd-compressed chunk
```

An unsigned ref is the plain `blake3:<manifest-hex>` string. A signed ref is:

```json
{
  "schema": 1,
  "manifest": "blake3:<manifest-hex>",
  "created_at": 1790000000,
  "signatures": [
    {
      "algorithm": "ed25519",
      "public_key": "ed25519:<public-key-hex>",
      "signature": "<signature-hex>"
    }
  ]
}
```

A release manifest is deterministic for a tree and release:

```json
{
  "schema": 1,
  "name": "my-app",
  "release": "1.4.0",
  "files": [
    {
      "path": "bin/my-app",
      "size": 48234496,
      "mode": 493,
      "hash": "blake3:<whole-file>",
      "chunks": [
        { "hash": "blake3:<chunk>", "size": 27118 }
      ]
    }
  ]
}
```

A compressed chunk adds `"encoding": "zstd"`; raw chunks omit the field.

Rules:

1. Names and releases are single safe path segments.
2. File paths are relative, slash-separated, and may not contain `.`, `..`, a
   NUL byte, or a backslash.
3. A file's chunk sizes must sum to its size; each chunk is at most
   `MAX_CHUNK`.
4. The manifest is written and hashed before the ref is updated. A manifest
   without a ref is garbage, never a visible release.
5. The ref is fetched first and pins the manifest hash. A manifest that does
   not hash to the ref is rejected even if its JSON looks valid.
6. Refs are append-only in practice: publishing an existing name/release with
   different bytes is refused. Republishing identical bytes is idempotent.
   Rolling a ref back or sideways is a separate, explicit operation.
7. A signature covers `graft-update-manifest-v1\0` followed by the manifest
   JSON. The ref pins the manifest hash, and the signature is verified before
   the JSON is parsed. Public-key verification is opt-in with `--public-key`;
   without it, integrity is still checked but authenticity is not.
8. A chunk's `encoding` describes its stored object, never its identity. The
   hash and `size` always cover plaintext; compressed and raw copies can coexist
   under the same hash with different keys.

## 3. Delta algorithm

On `update apply`:

1. Load the target manifest for `name/release`.
2. Load the installed-state manifest written by the previous successful apply.
3. Build `old_chunks: chunk_hash → [(path, offset, len)]` from the old
   manifest. Offsets are the running sum of each file's chunk sizes.
4. For each target file:
   - If path and whole-file hash match the old manifest, skip it.
   - Otherwise assemble the target file in a staging directory. For each chunk:
     - try every local old range for that hash, read it, and verify its blake3;
     - if no local range verifies, download the chunk from the repository and
       verify its blake3 and length;
     - write the verified bytes to the staged file.
   - Re-hash the assembled file and compare it with the manifest's file hash.
5. After every staged file verifies, create parent directories and rename the
   staged files into the target tree. With `--delete`, remove old files that the
   new release no longer contains.
6. Write the new installed-state manifest atomically, last.

This gives the delta property directly: a target chunk is downloaded only when
the bytes are not already present anywhere in the installed release. The
client's old chunks are verified before reuse, so a modified local file causes a
fallback download rather than silently corrupting the result.

`update deploy` runs the same staging and verification, but writes into a new
deployment directory and treats the previous deployment as the local chunk
source. It switches `current` only after the entire tree and state are in place.

Deletions are opt-in because removing files is destructive. Without
`--delete`, obsolete files remain and are reported by `update plan`.

## 4. Current state and phases

### Phase 1 — repository, deltas, apply (implemented)

- `update publish` walks a directory (`.gitignore`-aware for git trees), chunks
  files with FastCDC, uploads missing chunks, writes a manifest and ref.
- `update plan` compares a release with an installed-state manifest and reports
  changed/new/removed files, total chunks, missing chunks and missing bytes.
- `update state` scans an installed directory and writes the state manifest used
  by `plan`/`apply` without contacting a repository.
- `update apply` reuses locally available chunks, downloads the rest, verifies
  every layer, stages files, installs them, and records state. Staging is
  deterministic, so an interrupted run resumes verified chunk prefixes and
  cached chunks. Existing parent symlinks are rejected so a manifest cannot
  redirect a write outside the root.
- `update ls` lists refs in the repository.
- Repositories use the existing `fs:` and `s3:` object-store configuration; no
  new service or network protocol is introduced.

Exit gate: publish two releases where a large file is unchanged and a small
file changes; the second apply downloads only the small file's chunks. A chunk
with a bad hash is rejected. A path that escapes the target tree is rejected.

### Phase 2 — production distribution

Implemented:

1. **Signing.** `update keygen` writes Ed25519 keys. `publish --signing-key`
   signs the ref; `plan`, `apply` and `deploy` accept `--public-key` and verify
   before parsing the manifest. Unsigned refs remain readable for local use.
2. **Atomic deployments and rollback.** `update deploy` builds a complete
   versioned tree under `<root>/deployments/`, records the predecessor in the
   deployment metadata, and switches `<root>/current` by atomic symlink rename.
   `update rollback` switches back and can toggle again. Old deployment trees are
   retained.
3. **Garbage collection.** `update gc` reads every ref and manifest first, then
   deletes manifests and chunks that no ref reaches. A grace period protects
   chunks uploaded by an in-progress publish that has not written its ref yet.
   `--dry-run` reports without deleting.
4. **Static HTTP repository.** `http2:`/`h2c:` use cleartext HTTP/2 prior
   knowledge, `https:` negotiates HTTP/2 through ALPN, and `quic:`/`h3:` use
   HTTP/3 over QUIC. Reads use `Range`; HTTP repositories are read-only and do
   not support listing.
5. **Safe pruning.** `update prune-deployments` keeps the active and previous
   deployment plus newer history, rewrites predecessor links around removed
   trees, and tombstones before deletion. `update prune-refs --keep N` removes
   older refs by signed creation or object-store modification time; `gc` then
   reclaims their manifests and chunks.
6. **Catalog integration.** In a workspace, `update publish` records the store,
   repository, release and manifest in `update_releases`. Local store labels are
   canonical paths, so `fs:./repo` and `fs:repo` share pins. The row pins the
   manifest and its chunks during `update gc`; `prune-refs` forgets the row.
   `workspace status` counts update releases with other releases.
7. **Resumable downloads and staging.** `.graft-update` is deterministic. Each
   staged file is verified chunk by chunk on retry and resumes at the last
   complete chunk; downloaded chunks live in its cache. The tree is removed only
   after the state file is written.
8. **Private CAs and local HTTP/3 tests.** `--ca-file` adds a PEM CA to HTTPS
   and QUIC. Integration tests run both an h2c repository and an HTTP/3 server
   with a generated certificate, verifying signed manifests, ranges and file
   installation end to end.
9. **Compressed chunks.** `publish --compress` uses zstd level 3;
   `--compress-level N` selects 1-22. Chunks are stored as `<hash>.zst` only when
   smaller. `apply` decompresses from the manifest encoding into the plaintext
   chunk cache and verifies the plaintext hash. Raw chunks remain readable and
   compressed/raw variants of the same chunk coexist. The level is not part of
   the manifest or object key, so an existing compressed object is reused.

### Phase 3 — incremental and fleet concerns (later)

- Chunk-level prefetch and bounded parallelism.
- Range-based delta transfer when a changed chunk is large and mostly similar
  (bsdiff/xdelta inside a chunk), after measuring that FastCDC alone is not
  enough.
- Multiple architectures/channels as separate release names or manifest
  selectors.
- Mirroring into Graft volumes and lazy page replication for clients that
  already speak Graft.

## 5. Failure modes

| Failure | Behaviour |
|---|---|
| Chunk upload fails | Manifest/ref are not written; retry reuses already-uploaded chunks |
| Manifest write fails | Ref still points at old release; orphan manifest is harmless |
| Ref write fails | Old release remains selected |
| Client lacks a chunk locally | Downloads it; no server-side delta computation |
| Local file was modified | Chunk hash check fails; falls back to repository |
| Downloaded chunk corrupted | Rejected before staging the file |
| Compressed chunk fails to decode | Rejected before the plaintext hash is used |
| Crash during install | State file still describes the old release; rerun resumes verified staged chunks and files |
| Path traversal in manifest | Rejected before staging |
| Ref moved to another manifest | A client requiring a public key rejects a missing or mismatched signature; unsigned local reads still verify the manifest hash |
| Unreachable object during GC | Refs and catalog pins decide liveness; broken refs or pinned manifests abort before deletion |

## 6. Verification

Implemented commands:

```sh
workspace update keygen --private-key FILE --public-key FILE
workspace update --repo fs:./repo [--ca-file CA.pem] publish <NAME> <RELEASE> <DIR> [--signing-key FILE] [--compress | --compress-level LEVEL]
workspace update --repo fs:./repo plan <NAME> <RELEASE> --state STATE [--public-key FILE]
workspace update --repo fs:./repo apply <NAME> <RELEASE> <DIR> --state STATE [--delete] [--public-key FILE]
workspace update --repo fs:./repo deploy <NAME> <RELEASE> <ROOT> [--public-key FILE]
workspace update rollback <ROOT>
workspace update deployments <ROOT>
workspace update prune-deployments <ROOT> --keep N [--dry-run]
workspace update --repo fs:./repo prune-refs <NAME> --keep N [--grace-secs SECS] [--dry-run]
workspace update --repo fs:./repo gc <NAME> [--grace-secs SECS] [--dry-run]
workspace update --repo fs:./repo ls <NAME>
workspace update state <NAME> <RELEASE> <DIR> --out STATE
```

- `cargo test -p graft-workspace --test updates` covers publish, plan, apply,
  repository chunk reuse, local chunk reuse, resuming an interrupted apply,
  zstd chunks alongside raw chunks, corrupt-chunk rejection, manifest path/mode
  validation, Ed25519 verification, reachable-object GC, and catalog-pinned
  manifests surviving ref pruning.
- `cargo test -p graft-workspace --test deployments` covers atomic switching,
  idempotent redeploy, delta reuse from the previous deployment, rollback, and
  signed deployment verification.
- The two-release test starts with a 600 KB tree, changes only small files, and
  asserts the second apply downloads less than 32 KiB. A captured CLI run over a
  400 KB tree reported `chunks: 11 total, 2 missing; bytes: 400024 total, 24
  missing` and downloaded exactly 24 bytes on the second apply.
- `cargo test -p graft-workspace --test cli` kills a real `apply` after its
  staged file becomes partial, reruns it, checks the `resumed:` report and
  compares the installed bytes.
- `cargo test -p graft-workspace --test http_updates` also installs a signed
  release whose manifest records compressed chunks over HTTP/3 with a private
  CA.
- `cargo test -p graft-workspace --test doc_examples` runs the signed and
  compressed walkthrough commands and checks their manifest hashes, transcripts,
  wrong-key rejection and `.zst` object against `updates.md`. Its layout check
  also verifies that `update_releases` exists in the schema and on the page.
- The full `cargo test -p graft-workspace` suite passes (112 tests), and
  `cargo clippy -p graft-workspace --all-targets --no-deps -- -D warnings`
  and `cargo fmt --check` pass.
- `npx --yes pnpm@10.27.0 build` in `docs/` passes (82 pages, including the
  generated `update keygen`, `deploy`, `rollback`, `deployments`, `gc`,
  `prune-refs`, and `prune-deployments` reference pages).
- A captured signed deployment run downloaded 400,010 bytes for release 1.0.0
  and only 24 bytes for release 2.0.0, then rolled back and forward between the
  two deployment directories.
- `cargo test -p graft-workspace --test http_updates` serves signed repositories
  over a local cleartext HTTP/2 server and an HTTP/3 server with a generated
  certificate trusted through `--ca-file`. Both paths verify signatures, read
  ranges and install files; the HTTP/2 test also plans a delta and checks that
  publish and listing are refused. A manual QUIC smoke test against
  `cloudflare-quic.com` completed the HTTP/3 handshake and reported the missing
  release ref.
- Pruning tests cover deployment predecessor rewriting, rollback after prune,
  ref removal, subsequent GC, tampered signatures and GC grace periods.
- Catalog tests cover recording and forgetting update releases, counting them in
  `status`, and keeping pinned manifests and chunks when no ref remains.

## 7. Deliberate non-goals for this change

- No HTTP server or static-site generator. Serve an `fs:` directory or an
  `s3:` bucket with a normal web server; clients only need range reads.
- No automatic pruning. Old deployments and refs are removed only by explicit
  `prune-deployments` and `prune-refs` commands.
- No server-side binary diff. FastCDC chunk reuse is the delta mechanism.
- Compression is optional per publish, not negotiated by release or client;
  readers always follow the encoding recorded in the manifest.
- Deployments require Unix symlinks (`current` and `previous`). In-place
  `apply` remains available on every platform.

## 8. Downstream integration: OpenShell microVMs

`integrations/openshell/` consumes `update publish`/`deploy` to share one
avm-pkg store across the host and every OpenShell guest:

- `graft-avm-sync` copies the host store into a staging tree, publishes it,
  deploys it, and prints the resolved deployment path. The copy is intentional;
  avm-pkg packfiles are append-only and must not be hardlinked into a snapshot.
  A size/mtime signature is checked around the copy and the sync retries if a
  writer changed the store, so mixed pack/index generations are not published.
  Deployment trees and update refs are pruned to the same retention count, then
  GC removes unreferenced chunks after Graft's grace period (`--gc-grace 0` for
  immediate deletion); `--no-prune-refs` keeps full update history.
- `openshell-avm` mounts that deployment read-only at `/opt/graft-packages`,
  mounts `/nix` read-only for the dynamic linker, and mounts the workspace
  read-write. Those are the driver's three x86_64 shares.
- `verify-avm-store` creates a store, publishes and deploys it, makes the
  deployment read-only, extracts packages from it, then adds a second package
  and requires nonzero chunk reuse on the next sync.
- `avm-uploadd` is an authenticated staging cache for guest-produced packages.
  Its token is a 64-hex path prefix (the avm-pkg client cannot send auth
  headers), chunks are BLAKE3-verified on PUT, manifests are staged raw, and
  `graft-avm-import` verifies the manifest's Ed25519 signature against an
  allowlist before pulling through avm-pkg, so the client re-verifies manifest
  identity and every chunk. The verifier mirrors avm-cas manifest formats 1 and
  2 and checks the signature over the identity digest. Unsigned, tampered, and
  non-allowlisted manifests stay staged. The shared store is never written by
  the upload service.
- `avm-uploadd signer-pubkey --seed HEX` derives the allowlist entry for a
  builder. `graft-avm-init` creates `upload-allowed-signers`; the launcher
  forwards an optional `AVM_PKG_SIGNING_SEED` with `--env-from` rather than in
  argv.
- `avm-pkg-upload` and the launcher's `upload`/`shell` modes expose the path in
  the guest. The token is passed with `--env-from`, not in the sandbox command
  line, and the policy allows exactly `host.openshell.internal:18090` for the
  two upload binaries.
- `install-systemd-user` renders and enables `graft-avm-uploadd.service`,
  `graft-avm-sync.service`, and the 15-minute `graft-avm-sync.timer`. The sync
  imports pending uploads before publishing.
- `ci-smoke` runs avm-uploadd tests (including signature rejection), shell
  checks, both host verification scripts, and a systemd render. The upload
  acceptance test signs with a trusted key, then requires unsigned and
  wrong-signer uploads to be rejected. The GitHub workflow checks out public
  `australVM` and builds the real `avm-pkg` rather than a mock.
- A live OpenShell 0.1.5 VM test extracted a 46-byte package from the read-only
  deployment and the host-side output compared equal to the source package.
  The second sync reused 282 Graft chunks. A signed live test produced a package
  inside a VM with `put --sign`, uploaded it through the tokenized cache,
  verified the signature against the allowlist, imported it, republished it,
  and extracted identical bytes in another VM; that sync uploaded 283 new
  chunks (11914812 bytes) and reused 2.
