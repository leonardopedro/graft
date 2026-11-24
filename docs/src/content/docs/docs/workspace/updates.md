---
title: Binary updates
description: Publish signed, content-addressed releases, download only missing FastCDC chunks, and switch deployments atomically.
---

`workspace update` distributes directory trees as immutable, content-addressed
releases. A release repository is a set of blobs in an `fs:` or `s3:` object
store or a static HTTP server, and a client only needs that store plus the
previous installed-state manifest. When run from a workspace, `publish` also
records the release in the catalog, where it is counted by `status` and pins
its manifest against garbage collection.

Files are chunked with the same FastCDC implementation used by `cdc-volume`
(8 KiB minimum, 32 KiB target, 128 KiB maximum). A release manifest lists each
file's whole-file blake3 and its ordered chunks. Applying an update reads chunks
from the files already installed, verifies each one, and downloads only chunks
that are not available locally.

:::caution
Hashes prove integrity, not authenticity. Publish with `--signing-key` and
require the matching `--public-key` on clients that cross a trust boundary.
Without `--public-key`, a client still verifies the ref, manifest, files and
chunks, but does not verify who published them.
:::

## Repository layout

```text
<prefix>/<name>/refs/<release>            plain hash or signed ref JSON
<prefix>/<name>/manifests/<hash>.json
<prefix>/<name>/chunks/<2>/<hash>         raw FastCDC chunk
```

An unsigned ref is `blake3:<manifest-hash>`. A signed ref is JSON containing the
manifest hash, creation time, and Ed25519 signatures. The ref is written last
and pins the manifest hash; the manifest pins each file's hash and each chunk's
hash.

## HTTP repositories

A static file server can serve a repository read-only. The server must return
the exact object keys and support `Range` requests for chunks. `HEAD` supplies
sizes; if it returns 405 or 501, `GET` with a one-byte range is used instead.

```sh
# HTTPS negotiates HTTP/2 via ALPN.
workspace update --repo https://updates.example.com/root apply demo 1.0.0 ./install \
  --state state.json --public-key public.key

# A private CA for HTTPS or QUIC.
workspace update --repo quic://updates.internal/root --ca-file ca.pem \
  apply demo 1.0.0 ./install --state state.json

# Cleartext HTTP/2 prior knowledge (h2c).
workspace update --repo http2://127.0.0.1:8080 apply demo 1.0.0 ./install \
  --state state.json

# HTTP/3 over QUIC (TLS with the `h3` ALPN).
workspace update --repo quic://updates.example.com/root apply demo 1.0.0 ./install \
  --state state.json --public-key public.key
```

`https://` uses HTTP/2 when the server negotiates it. `http2://` and `h2c://`
force cleartext HTTP/2 prior knowledge. `quic://`, `h3://`, and
`https+quic://` use HTTP/3. HTTPS and QUIC use the platform-independent Mozilla
root set. Pass `--ca-file ca.pem` to add a private CA for HTTPS or QUIC.

```sh
workspace update --repo http2://127.0.0.1:36581 apply demo 1.0.0 install --state state.json
```

```text
applied demo/1.0.0 to install
files: 2 written, 0 unchanged, 0 deleted
chunks: 3 downloaded (40004 bytes), 0 reused (0 bytes)
state: state.json
```

HTTP repositories are read-only. Publishing, listing refs, pruning and garbage
collection require `fs:` or `s3:` because a static server has no directory
listing or write API.

```text
error: HTTP update repositories are read-only; publishing requires fs: or s3:
```

## Signing keys

```sh
workspace update keygen --private-key private.key --public-key public.key
workspace update --repo fs:./repo publish demo 1.0.0 ./v1 --signing-key private.key
```

```text
private key: private.key
public key: public.key
published demo/1.0.0: blake3:b4a0dba429328b6248c8da0c04d9cbbc3d31df5236d5100120ed485eac6ca8c0
signed: ed25519
files: 2, chunks: 13, bytes: 400010
uploaded chunks: 13 (400010 bytes), reused: 0
```

The private key file is created with mode 0600. Publishing an existing release
with different bytes is refused; publishing the same tree again is idempotent.
Use `--prefix` to put repositories elsewhere in the store, and
`--repo s3:BUCKET[/PREFIX]` for S3-compatible storage. `AWS_*` environment
variables are read the same way as for media.

If `workspace.toml` is present and the workspace is initialized, `publish`
records the repository, release and manifest hash in the catalog. `workspace
status` counts it with other releases, and `update gc` keeps the manifest and
its chunks pinned. Running outside a workspace leaves the repository
self-contained; the catalog then has no record to pin.

### End-to-end signed example

This is a complete local repository: generate a key pair, publish a signed
release, then install it while requiring the public key.

```sh
mkdir -p v1/bin
printf '#!/bin/sh\necho demo 1.0.0\n' > v1/bin/demo
printf 'version 1\n' > v1/version.txt
chmod +x v1/bin/demo

workspace update keygen --private-key private.key --public-key public.key
workspace update --repo fs:./repo publish demo 1.0.0 ./v1 \
  --signing-key private.key
workspace update --repo fs:./repo apply demo 1.0.0 ./install \
  --state state.json --public-key public.key
```

```text
private key: private.key
public key: public.key
published demo/1.0.0: blake3:a0782adaf919d8729e20472299d541333b1c37bf2ad719f0b3b5f510ca6a9120
signed: ed25519
files: 2, chunks: 2, bytes: 36
uploaded chunks: 2 (36 bytes), reused: 0
applied demo/1.0.0 to ./install
files: 2 written, 0 unchanged, 0 deleted
chunks: 2 downloaded (36 bytes), 0 reused (0 bytes)
state: state.json
```

A different key is rejected before any file is staged:

```sh
workspace update keygen --private-key other-private.key --public-key other-public.key
workspace update --repo fs:./repo apply demo 1.0.0 ./wrong-install \
  --state wrong.json --public-key other-public.key
```

```text
private key: other-private.key
public key: other-public.key
error: release demo/1.0.0 has no valid signature from the required key
```

Keep `private.key` out of the repository. `public.key` is the value clients pin
and pass to `plan`, `apply` or `deploy`.

## Install and state

`apply` writes an installed-state manifest after every file verifies. The state
file is also the input to the next update; it is what makes the second download
a delta rather than a full tree.

```sh
workspace update --repo fs:./repo apply demo 1.0.0 ./install \
  --state state.json --public-key public.key
```

```text
applied demo/1.0.0 to install
files: 2 written, 0 unchanged, 0 deleted
chunks: 13 downloaded (400010 bytes), 0 reused (0 bytes)
state: state.json
```

A missing state file means a first install. Files are assembled in a
`.<destination>.graft-update` staging directory and moved into place only after
every file's whole-file hash matches. The state file is written last, so a crash
leaves the old state and a re-runnable install. Staging is deterministic: an
interrupted apply keeps verified chunks and the prefix of each staged file, and
the next run resumes from the last complete chunk. A successful apply removes
the staging tree. Existing parent symlinks in the target tree are rejected, so a
manifest cannot redirect a write outside the destination.

## Update

Publish a new release. Unchanged chunks already in the repository are reused:

```sh
workspace update --repo fs:./repo publish demo 2.0.0 ./v2 \
  --signing-key private.key
```

```text
published demo/2.0.0: blake3:3e2dbd36562315eec9dbe0e00a75c0f4d8561b3b2a465a1ee64302fd258bf2ea
signed: ed25519
files: 3, chunks: 14, bytes: 400024
uploaded chunks: 2 (24 bytes), reused: 12
```

Plan against the installed state:

```sh
workspace update --repo fs:./repo plan demo 2.0.0 \
  --state state.json --public-key public.key
```

```text
release: demo/2.0.0
files: 3 total, 1 new, 1 changed, 1 unchanged, 0 removed
chunks: 14 total, 2 missing; bytes: 400024 total, 24 missing
```

Apply it:

```sh
workspace update --repo fs:./repo apply demo 2.0.0 ./install \
  --state state.json --public-key public.key
```

```text
applied demo/2.0.0 to install
files: 2 written, 1 unchanged, 0 deleted
chunks: 2 downloaded (24 bytes), 0 reused (0 bytes)
state: state.json
```

The 400 KB binary was unchanged, so only the two small files' chunks crossed
the network. If a local file was modified, its chunk hashes fail and those
chunks fall back to the repository instead of being trusted.

### Compressed chunks

Pass `--compress` to store zstd-compressed chunks at the default level 3, or
`--compress-level N` to choose a level from 1 through 22. The level implies
compression. The hash still covers the plaintext, so chunk reuse and local state
matching are unchanged.

```sh
mkdir -p v4
for i in $(seq 50); do printf 'compressible release data\n'; done > v4/notes.txt
workspace update --repo fs:./repo publish demo 4.0.0 ./v4 --compress-level 9
```

```text
published demo/4.0.0: blake3:50700a36c9b818608659a8b1a0a59057934dc79405bff53af61245982be4d11b
files: 1, chunks: 1, bytes: 1300
uploaded chunks: 1 (44 bytes), reused: 0
compressed chunks: 1 (zstd level 9)
```

A compressed object is stored as `<hash>.zst` beside the raw `<hash>` object.
The level is a publishing preference, not part of the manifest or object key; an
already-published compressed chunk is reused rather than recompressed at a new
level. Publishing the same chunk without compression adds the raw object
without breaking releases that already use the compressed one. `apply` reads
the encoding from the manifest, decompresses into the verified staging file,
and stores plaintext in its chunk cache.

## Atomic deployments

`deploy` installs a complete versioned tree and switches a symlink only after
the whole tree verifies. It also lets the next deploy reuse chunks from the
previous deployment.

```sh
workspace update --repo fs:./repo deploy demo 1.0.0 ./deploy --public-key public.key
workspace update --repo fs:./repo deploy demo 2.0.0 ./deploy --public-key public.key
```

```text
deployment: 1.0.0-b4a0dba42932
current: deploy/current
files: 2 written, 0 unchanged; chunks: 13 downloaded (400010 bytes)

deployment: 2.0.0-3e2dbd365623
current: deploy/current
previous: 1.0.0-b4a0dba42932
files: 3 written, 0 unchanged; chunks: 2 downloaded (24 bytes)
```

Layout:

```text
deploy/deployments/<release>-<manifest-prefix>/
deploy/states/<release>-<manifest-prefix>.json
deploy/current  -> deployments/<id>
deploy/previous -> deployments/<id>
```

The active deployment records its predecessor in
`.graft-deployment.json`. `current` is switched by renaming a temporary symlink
over it, so readers see either the old tree or the new one.

```sh
workspace update deployments ./deploy
workspace update rollback ./deploy
```

```text
 	1.0.0-b4a0dba42932	1.0.0	blake3:b4a0dba429328b6248c8da0c04d9cbbc3d31df5236d5100120ed485eac6ca8c0
*	2.0.0-3e2dbd365623	2.0.0	blake3:3e2dbd36562315eec9dbe0e00a75c0f4d8561b3b2a465a1ee64302fd258bf2ea

rolled back from 2.0.0-3e2dbd365623 to 1.0.0-b4a0dba42932
current: deploy/current
```

Running `rollback` again toggles to the deployment it just left. Deployments
are retained until an explicit prune.

`prune-deployments` keeps at least the active deployment and its rollback
target. `--keep` counts those two plus any newer historical trees. Predecessor
links in retained deployments are rewritten to skip removed trees, so rollback
continues to work across the gap.

```sh
workspace update prune-deployments ./deploy --keep 3 --dry-run
workspace update prune-deployments ./deploy --keep 3
```

```text
would remove 1 deployment(s), 1182 bytes; kept 3
  1.0.0-c422a2d8bdb3
removed 1 deployment(s), 1182 bytes; kept 3
  1.0.0-c422a2d8bdb3
```

Removed trees are renamed to hidden tombstones before recursive deletion, and
their state files are removed after the directory. `--keep` must be at least 2.

## Garbage collection

`gc` reads every ref and manifest before deleting anything. Only manifests and
chunks unreachable from a current ref or a catalog pin are removed. The grace
period protects chunks uploaded by a publish that has not written its ref yet.

```sh
workspace update --repo fs:./repo gc demo --grace-secs 86400 --dry-run
workspace update --repo fs:./repo gc demo --grace-secs 86400
```

```text
would remove 0 manifest(s) and 0 chunk(s), 0 bytes
removed 0 manifest(s) and 0 chunk(s), 0 bytes
```

Use `--grace-secs 0` only when no publish can be in flight.

## Pruning old refs

`prune-refs` deletes refs older than the `N` most recently published, while
keeping every ref inside the grace period. It never edits manifests or chunks;
run `gc` afterwards to reclaim their objects. Refs have no semantic version
ordering: recency is the ref's signed creation time, or its object-store
modification time for unsigned refs.

```sh
workspace update --repo fs:./repo prune-refs demo --keep 2 --grace-secs 0 --dry-run
workspace update --repo fs:./repo prune-refs demo --keep 2 --grace-secs 0
workspace update --repo fs:./repo gc demo --grace-secs 0
```

```text
would remove 2 ref(s); kept 2
  1.0.0
  2.0.0
removed 2 ref(s); kept 2
  1.0.0
  2.0.0
removed 2 manifest(s) and 4 chunk(s), 3468 bytes
```

Pruning a ref makes that release unavailable to new installs. In a workspace it
also removes the matching catalog row, so a following `gc` can reclaim the
manifest. Already-deployed trees and rollback metadata do not need the ref.

## Inspect

```sh
workspace update --repo fs:./repo ls demo
```

```text
1.0.0	blake3:b4a0dba429328b6248c8da0c04d9cbbc3d31df5236d5100120ed485eac6ca8c0	signed	1
2.0.0	blake3:3e2dbd36562315eec9dbe0e00a75c0f4d8561b3b2a465a1ee64302fd258bf2ea	signed	1
```

`update state` writes a manifest for a directory without contacting a
repository. It is useful when bringing an already-installed tree under update
management:

```sh
workspace update state demo 1.0.0 ./install --out installed.json
workspace update --repo fs:./repo plan demo 2.0.0 --state installed.json
```

## Deletions

In-place `apply` replaces and adds files by default; it does not remove files
that the new release omits. Pass `--delete` to remove files named by the
previous state and absent from the target. Deletion runs only after all new and
changed files have verified. Deployments do not delete: each one is a complete
tree.

## Limits

- Deployments need Unix symlinks. In-place `apply` works everywhere.
- Pruning is explicit. `prune-deployments` and `prune-refs` must be run by an
  operator; `gc` then reclaims objects that no remaining ref reaches.
- HTTP repositories are read-only and cannot list refs. Use `fs:` or `s3:` for
  publishing, `ls`, `prune-refs` and `gc`.
- An interrupted apply keeps a hidden `.<destination>.graft-update` staging
  tree until it succeeds or the operator removes it. It is a cache, not a
  second installed copy.
- Chunks are raw unless published with `--compress` or `--compress-level N`.
  Compression is per chunk and applies only when it reduces the stored size;
  use an object store or web server for transport compression as well.
- Delta reuse comes from FastCDC boundaries. There is no server-side binary
  patch, so a wholly rewritten small file re-downloads that file's chunks.
