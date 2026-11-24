---
title: "workspace backup"
description: "Export the catalog, blobs and document snapshots to the backup target"
sidebar:
  label: "backup"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Export the catalog, blobs and document snapshots to the backup target

## Usage

```text
workspace backup [OPTIONS]
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| --prefix <PREFIX> |  | Replace the configured base prefix for this run |
| --every <SECS> |  | Run continuously, taking a backup every SECS. Omit for one backup |
| --keep <N> |  | Keep the newest N complete backups; older ones are deleted. In scheduled mode this defaults to 7 |

