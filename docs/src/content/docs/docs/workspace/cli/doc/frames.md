---
title: "workspace doc frames"
description: "Print, or write out, frames after the given cursors"
sidebar:
  label: "frames"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Print, or write out, frames after the given cursors

## Usage

```text
workspace doc frames [OPTIONS] <DOC>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <DOC> |  | Document id |
| --cursor <PEER=SEQ> |  | Cursor as `PEER=SEQ`, repeatable. Producers without one start at 1 |
| --out-dir <DIR> |  | Write each payload to `<DIR>/<peer>-<seq>.bin` instead of printing only its length |
| --snapshot-out <FILE> |  | Write the compacted snapshot that precedes these frames here |

