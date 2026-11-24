# OpenShell + avm-pkg + Graft

This integration gives every OpenShell microVM the **same read-only avm-pkg
store** as the host, mounted over virtiofs. Packages are content-addressed, so
the guest mounts the host's deployed tree instead of copying packages into each
VM image. Graft supplies the durable FastCDC store and immutable deployment
history.

```text
host                                      OpenShell microVM (per sandbox)
┌─────────────────────────────┐           ┌──────────────────────────────┐
│ host avm-pkg store          │           │ /opt/graft-packages  (ro)    │
│   packs/ index/ manifests/  │           │   store/  <- same tree       │
│          │                  │           │   bin/avm-pkg                │
│          ▼                  │           │                              │
│ graft-avm-sync              │ virtiofs  │ /nix                 (ro)    │
│   workspace update publish  │──────────▶│ /workspace           (rw)    │
│   workspace update deploy   │           │                              │
│          │                  │           │ avm-pkg --store              │
│          ▼                  │           │   /opt/graft-packages/store  │
│ Graft repo (FastCDC dedup)  │           └──────────────────────────────┘
└─────────────────────────────┘
```

## Why this shares bytes

- The VM driver gives each share a DAX window and the guest mounts it read-only;
  package files are served from the host page cache, not copied into the guest.
- All VMs mount the **same deployment path**. The host avm-pkg store is copied
  once into the deployment, never once per VM.
- Graft publishes the deployment as FastCDC chunks. A later sync uploads only
  chunks that changed; unchanged package packs and the avm-pkg binary are reused.

This is the same model as OpenShell's read-only `/nix` share, applied to
avm-pkg's store layout.

Packages produced inside a guest return through a separate authenticated
staging path: `avm-pkg-upload` pushes to `avm-uploadd` on the host, the host
imports through `avm-pkg pull`, and the next Graft sync makes the package
visible to every VM. The upload cache is not part of the shared store.

## Prerequisites

- OpenShell with the `vm` driver, `allow_driver_config = true`,
  `enable_bind_mounts = true`, and resource admission disabled. The
  `../OpenShell/openshell-nixos` setup already provides these.
- A built `workspace` binary from this repository
  (`cargo build -p graft-workspace --bin workspace`).
- `avm-pkg` from `../australVM/safestos/pkg`, or `AVM_PKG_BIN` pointing at it.
- For guest uploads, build `avm-uploadd` (`cargo build -p avm-uploadd`).
- `/nix` on the host, because the host-built `avm-pkg` is dynamically linked to
  the Nix store. The guest mounts the same `/nix`.
- Three x86_64 mounts are used: `/nix`, `/opt/graft-packages`, `/workspace`.

## Host sync

```sh
export PATH="$PWD/target/debug:$PATH"
export GRAFT_AVM_STORE="$HOME/packages/avm-store"

integrations/openshell/graft-avm-sync
```

The command prints only the deployed path on stdout:

```text
/home/you/.local/state/graft/avm/runtime/deployments/20261123T122237Z-416418-782552e91b2d
```

It publishes the current store, deploys an immutable tree containing
`store/` and `bin/avm-pkg`, prunes old deployment trees and update-repository
refs (default keep 3, followed by GC), and prints progress on stderr. Useful
variables:

| Variable | Default |
| --- | --- |
| `GRAFT_AVM_STORE` | `$AVM_PKG_STORE` or `./avm-store` |
| `GRAFT_AVM_REPO` | `fs:$XDG_STATE_HOME/graft/avm/repo` |
| `GRAFT_AVM_DEPLOY_ROOT` | `$XDG_STATE_HOME/graft/avm/runtime` |
| `GRAFT_AVM_NAME` | `avm` |
| `GRAFT_AVM_KEEP` | `3` |
| `GRAFT_AVM_IMPORT_ROOT` | `$XDG_STATE_HOME/graft/avm/upload-cache` (`--no-import` disables) |
| `GRAFT_AVM_PRUNE_REFS` | `1` (prune old refs and GC; `--no-prune-refs` keeps them) |
| `GRAFT_AVM_GC_GRACE_SECS` | `86400` (Graft will not delete younger objects) |
| `GRAFT_AVM_SIGNING_KEY` | unsigned |
| `GRAFT_AVM_PUBLIC_KEY` | no required signature |

Run the sync while no `avm-pkg` writer is mutating the source store. avm-pkg
appends to packfiles in place and does not expose a shared lock. The sync takes
a real copy (never a hardlink), records file size and mtime before and after the
copy, and retries up to three times if the store changed. If a writer is still
active, the sync fails rather than publishing a mixed pack/index generation.

## Run a VM

```sh
OPENSHELL_GATEWAY=local \
PROJECTS_DIR="$HOME/Projects" \
SANDBOX_NAME=avm \
integrations/openshell/openshell-avm get <manifest-hex> -o /workspace/package.bin
```

Without `GRAFT_AVM_DEPLOYMENT`, the launcher runs `graft-avm-sync` first. To pin
an already verified deployment:

```sh
GRAFT_AVM_DEPLOYMENT=/absolute/deployment/path \
OPENSHELL_GATEWAY=local \
integrations/openshell/openshell-avm get <manifest-hex> -o /workspace/package.bin
```

Inside the guest, `AVM_PKG_STORE` defaults to the read-only shared store, so
read commands work directly. Writes need an explicit writable store:

```sh
avm-pkg --store /workspace/.avm-store put ./new-package --name new --version 1
```

The shared tree is never writable from a guest. Uploads go through the
authenticated cache below instead.

## Guest-produced packages

Start the upload cache and create its token:

```sh
export GRAFT_AVM_STATE="$HOME/.local/state/graft/avm"
cargo build -p avm-uploadd
integrations/openshell/graft-avm-init
```

`graft-avm-init` prints a `local_url` (host importer) and a `guest_url`
(`host.openshell.internal`) containing the 256-bit token. The launcher reads the
token file automatically and passes it to the guest with `--env-from`, so it
does not appear in the sandbox command line. To run the cache under systemd:

```sh
integrations/openshell/install-systemd-user
```

Guest uploads must be signed. Configure the signing seed on the builder and add
its public key to the host allowlist:

```sh
export AVM_PKG_SIGNING_SEED=<64-hex-seed>
export GRAFT_AVM_STATE="$HOME/.local/state/graft/avm"
./target/debug/avm-uploadd signer-pubkey --seed "$AVM_PKG_SIGNING_SEED" \
  >> "$GRAFT_AVM_STATE/upload-allowed-signers"
```

`graft-avm-init` creates an empty allowlist at `upload-allowed-signers`; add one
64-hex public key per trusted builder. `GRAFT_AVM_UPLOAD_SIGNERS` accepts a
comma- or space-separated list instead, and `GRAFT_AVM_UPLOAD_SIGNERS_FILE`
overrides the file path. The seed stays with the builder and is passed to the
sandbox with `--env-from` when set, never in the command line.

Inside a VM:

```sh
avm-pkg --store /workspace/.avm-store init
avm-pkg --store /workspace/.avm-store put ./new-package --name new --version 1 \
  --sign "$AVM_PKG_SIGNING_SEED"
AVM_PKG_STORE=/workspace/.avm-store avm-pkg-upload <manifest-hex>
```

The host importer verifies the Ed25519 signature over the manifest identity
*before* pulling, requires the signing key to be in the allowlist, and then
runs `avm-pkg pull` to verify the manifest identity and every chunk digest. A
missing allowlist, an unsigned manifest, a bad signature, or an untrusted key
leaves the upload staged and refuses the import.

```sh
integrations/openshell/graft-avm-import \
  --store "$HOME/packages/avm-store" \
  --upload-root "$GRAFT_AVM_STATE/upload-cache"
integrations/openshell/graft-avm-sync
```

`graft-avm-sync` runs this import automatically when the upload cache exists
(`--no-import` disables it). The upload service binds to `127.0.0.1:18090`; the
OpenShell supervisor reaches it through `host.openshell.internal`. The policy
allows that one host/port for `avm-pkg` and `avm-pkg-upload` only.

The token authenticates cache access; signatures authorize packages. Rotate
`upload.token` (and restart `graft-avm-uploadd.service`) if a sandbox is
untrusted, and remove a builder key from the allowlist to revoke it.

## Automatic sync

`install-systemd-user` renders and enables three user units:

- `graft-avm-uploadd.service` — the authenticated cache
- `graft-avm-sync.service` — import pending uploads, publish, deploy, prune
- `graft-avm-sync.timer` — runs the sync every 15 minutes and on boot

Set `GRAFT_AVM_STORE` before installing if the host store is not
`~/packages/avm-store`. The timer and upload service are independent: stopping
the timer does not break read-only package mounts.

## CI smoke

```sh
AVM_PKG_BIN=../australVM/safestos/pkg/target/release/avm-pkg \
  integrations/openshell/ci-smoke
```

CI runs `cargo test -p avm-uploadd` (including signature parser tests), shell
syntax/ShellCheck, the two host verification scripts, and a systemd unit
render. `verify-avm-upload` proves that signed trusted uploads import while
unsigned and wrong-signer uploads are rejected. The GitHub workflow checks out
the public `australVM` repository and builds the real `avm-pkg` binary, so the
push/import/signature path is not mocked.

## Verify

```sh
integrations/openshell/verify-avm-store
```

The check:

1. creates an avm-pkg store and package;
2. publishes/deploys it through Graft;
3. makes the deployed store read-only and extracts the package again;
4. adds a second package, syncs again, and requires nonzero Graft chunk reuse;
5. verifies both packages from the second read-only deployment.

The live VM path was exercised with OpenShell `0.1.5` and this gateway:

```sh
OPENSHELL_GATEWAY=local \
GRAFT_AVM_DEPLOYMENT=<verified-deployment> \
integrations/openshell/openshell-avm get <manifest> -o /workspace/avm-test.out
```

Captured on 2026-11-23 (paths shortened):

```text
graft-avm-sync: publishing avm/<release> from <host-store>
published avm/<release>: blake3:<manifest-hash>
files: 5, chunks: 283, bytes: 11950413
uploaded chunks: 281 (11914551 bytes), reused: 2
graft-avm-sync: deploying avm/<release> to <deploy-root>
files: 5 written, 0 unchanged; chunks: 281 downloaded (11914551 bytes)
verify-avm-store: read-only store served the first package
verify-avm-store: second sync reused chunks: reused: 282
verify-avm-store: read-only deployment served both packages
```

The live guest then reported:

```text
Created sandbox: avm-graft-test
{"wrote":"/workspace/avm-test.out","bytes":46,"name":"second","hot_usage_bytes":46,"hot_cells":1}
live guest extraction matches host package
```

The host-side file compared equal to the source package with `cmp`.

The upload path was exercised in the same live setup. The guest reported:

```text
{"manifest":"6b8b6b1fbeb115e9f54a127b743e5dda90263aa42e56caf246f3f6d7e8c9c854","name":"signed-guest","version":"1.0.0","bytes":30,"chunks_total":1,"chunks_sent":1,...}
signed guest uploaded 6b8b6b1fbeb115e9f54a127b743e5dda90263aa42e56caf246f3f6d7e8c9c854
```

After signature verification, import, and publish:

```text
graft-avm-import: importing 6b8b6b1fbeb115e9f54a127b743e5dda90263aa42e56caf246f3f6d7e8c9c854 (signature verified)
graft-avm-import: imported 1 manifest(s)
files: 7, chunks: 285, bytes: 11950674
uploaded chunks: 283 (11914812 bytes), reused: 2
```

A second VM then extracted that manifest and the host-side `cmp` passed.

## Production constraints

- **Read-only by construction.** The package share is always mounted
  `read_only: true` and the policy only grants read access. A compromised guest
  cannot poison the package store.
- **Bounded storage.** A sync creates a new deployment; running sandboxes keep
  their old tree. Old deployments are pruned to `GRAFT_AVM_KEEP`, and the same
  count is applied to repository refs before GC. Deployments are full copies, so
  GC cannot affect a running or retained deployment. Graft's 24-hour GC grace
  protects recent objects; set `GRAFT_AVM_GC_GRACE_SECS=0` for immediate
  deletion when this script is the only publisher. Pass `--no-prune-refs` to
  keep update history instead. The upload cache retains imported chunk bytes;
  rotate/clear `upload-cache` separately when decommissioning a token.
- **One narrow network path.** Package reads use virtiofs, not the network. The
  only allowed egress is `host.openshell.internal:18090` for the two upload
  binaries; all other destinations remain denied.
- **Quiesce writers.** The sync detects changes during its copy and retries,
  but continual writes make it fail. Run it between package mutations, or place
  the store on a filesystem with atomic snapshots and modify the script to
  snapshot before publishing.
- **Two gates.** The token authenticates cache writes; an allowlisted Ed25519
  signature authorizes import. A token holder cannot import unsigned or
  self-signed content, and a trusted signer still needs the token to upload.
  Keep the signing seed on the builder, not in the guest image or policy.
  Removing a public key from `upload-allowed-signers` revokes it.
- **Nix linker coupling.** `/nix` is required for the current host-built
  `avm-pkg`. A statically linked or Nix-free build relaxes that constraint but
  still needs the guest's dynamic libraries.
- **x86_64 mount budget.** Three shares already consume the limit. The upload
  path uses the existing `/workspace` and network, so it adds no fourth mount.
  Additional mounts still require aarch64 or a different delivery path.
