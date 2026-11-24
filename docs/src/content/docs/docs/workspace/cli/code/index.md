---
title: "workspace code"
description: "Versioned code checkpoints: tree manifests and git bundles"
sidebar:
  label: "code"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Versioned code checkpoints: tree manifests and git bundles

## Usage

```text
workspace code <COMMAND>
```

## Commands

| Command | Description |
| --- | --- |
| [`snapshot`](/docs/workspace/cli/code/snapshot/) | Checkpoint a directory into the encrypted archive (requires [archive]) |
| [`restore`](/docs/workspace/cli/code/restore/) | Restore a checkpoint into a directory, verifying every hash |

