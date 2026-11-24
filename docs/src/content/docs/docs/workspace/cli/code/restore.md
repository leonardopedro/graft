---
title: "workspace code restore"
description: "Restore a checkpoint into a directory, verifying every hash"
sidebar:
  label: "restore"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Restore a checkpoint into a directory, verifying every hash

## Usage

```text
workspace code restore --out <OUT> <TREE>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <TREE> |  | Tree manifest hash (`blake3:<hex>`), from `code snapshot` or an entry |
| --out <OUT> |  | Destination directory; created if missing |

