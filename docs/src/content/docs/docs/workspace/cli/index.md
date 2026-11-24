---
title: "CLI reference"
description: "Workspace catalog and live-document oplog on Graft"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Workspace catalog and live-document oplog on Graft

Every invocation opens the workspace volumes, does one thing and exits. See the [CLI usage](/docs/workspace/) guide for the workflow and [Quick start](/docs/workspace/quickstart/) for a worked example.

## Usage

```text
workspace [OPTIONS] <COMMAND>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| --config <FILE> |  | Config file. Defaults to `./workspace.toml` when it exists |
| --data <DIR> |  | Graft data directory; overrides `data_dir` from the config |
| --remote <SPEC> |  | Remote: `memory`, `fs:DIR`, or `s3:BUCKET[/PREFIX]` |

## Commands

| Command | Description |
| --- | --- |
| [`init`](/docs/workspace/cli/init/) | Create the catalog volume and assign a workspace id |
| [`status`](/docs/workspace/cli/status/) | Show the workspace id, schema version and row counts |
| [`share`](/docs/workspace/cli/share/) | Print the reference another data directory needs to adopt this workspace |
| [`clone`](/docs/workspace/cli/clone/) | Adopt an existing workspace from a remote into a fresh data directory |
| [`serve`](/docs/workspace/cli/serve/) | Serve live-document frames over a Unix socket |
| [`sync`](/docs/workspace/cli/sync/) | Push local commits for both volumes to the remote |
| [`pull`](/docs/workspace/cli/pull/) | Fetch new commits for both volumes from the remote |
| [`entry`](/docs/workspace/cli/entry/) | Read and write catalog entries |
| [`blob`](/docs/workspace/cli/blob/) | Read, write and inspect media blobs |
| [`doc`](/docs/workspace/cli/doc/) | Live-document frames: the Loro oplog lane |
| [`code`](/docs/workspace/cli/code/) | Versioned code checkpoints: tree manifests and git bundles |
| [`release`](/docs/workspace/cli/release/) | Release staging: assemble entries into a datapackage |
| [`update`](/docs/workspace/cli/update/) | Binary updates: `FastCDC` content-addressed releases and deltas |
| [`metrics`](/docs/workspace/cli/metrics/) | Print lane counters as Prometheus text |
| [`backup`](/docs/workspace/cli/backup/) | Export the catalog, blobs and document snapshots to the backup target |
| [`restore`](/docs/workspace/cli/restore/) | Rebuild a workspace from a backup prefix into an empty data directory |
| [`service`](/docs/workspace/cli/service/) | Generate service files that schedule `workspace backup` |

