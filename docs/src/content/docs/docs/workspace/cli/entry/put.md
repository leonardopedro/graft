---
title: "workspace entry put"
description: "Insert or update an entry"
sidebar:
  label: "put"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Insert or update an entry

## Usage

```text
workspace entry put [OPTIONS] --lane <LANE> --kind <KIND> <PATH>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <PATH> |  | Workspace path, e.g. `notes/hello` |
| --lane <LANE> |  | One of: `loro`, `code`, `media`, `archive`. |
| --kind <KIND> |  | One of: `doc`, `file`, `dir`, `dataset`, `release`. |
| --blob <HASH> |  | Blob hash (`blake3:<hex>`) this entry points at |
| --loro-doc <ID> |  | Loro document id for a live document |
| --code-tree <HASH> |  | Code tree manifest hash |
| --release-doi <DOI> |  | DOI of the release this entry belongs to |
| --mime <MIME> |  |  |
| --size <BYTES> |  |  |

