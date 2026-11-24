---
title: "workspace update"
description: "Binary updates: `FastCDC` content-addressed releases and deltas"
sidebar:
  label: "update"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Binary updates: `FastCDC` content-addressed releases and deltas

## Usage

```text
workspace update [OPTIONS] <COMMAND>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| --repo <SPEC> |  | Repository store: `fs:DIR`, `s3:BUCKET[/PREFIX]`, or a read-only `https://`, `http2://` (h2c), or `quic://` (HTTP/3) URL |
| --prefix <PREFIX> | updates | Prefix inside the repository store |
| --ca-file <FILE> |  | Additional PEM certificate authority for HTTPS or QUIC |

## Commands

| Command | Description |
| --- | --- |
| [`publish`](/docs/workspace/cli/update/publish/) | Publish a directory as an immutable release |
| [`plan`](/docs/workspace/cli/update/plan/) | Compare a release with an installed-state manifest |
| [`state`](/docs/workspace/cli/update/state/) | Write a manifest for an installed directory without contacting a repo |
| [`apply`](/docs/workspace/cli/update/apply/) | Apply a release, downloading only chunks absent from the installed tree |
| [`ls`](/docs/workspace/cli/update/ls/) | List release refs in the repository |
| [`keygen`](/docs/workspace/cli/update/keygen/) | Generate an Ed25519 signing key pair |
| [`gc`](/docs/workspace/cli/update/gc/) | Remove manifests and chunks unreachable from every release ref |
| [`prune-refs`](/docs/workspace/cli/update/prune-refs/) | Delete old release refs, keeping the newest ones |
| [`deploy`](/docs/workspace/cli/update/deploy/) | Install a release into a new deployment and atomically switch `current` |
| [`rollback`](/docs/workspace/cli/update/rollback/) | Switch `current` back to the previous deployment |
| [`deployments`](/docs/workspace/cli/update/deployments/) | List deployments under a deployment root |
| [`prune-deployments`](/docs/workspace/cli/update/prune-deployments/) | Delete old deployments, keeping the active and previous versions |

