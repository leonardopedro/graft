---
title: "workspace blob put"
description: "Ingest a file and point an entry at it"
sidebar:
  label: "put"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Ingest a file and point an entry at it

## Usage

```text
workspace blob put [OPTIONS] --file <FILE> <PATH>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <PATH> |  | Workspace path the entry is created at |
| --file <FILE> |  |  |
| --storage <STORAGE> | raw | `raw` streams into object storage; `cdc` chunks and encrypts into the archive volume and supports range reads too. One of: `raw`, `cdc`. |
| --mime <MIME> |  |  |

