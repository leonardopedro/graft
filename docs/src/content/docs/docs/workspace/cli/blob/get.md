---
title: "workspace blob get"
description: "Write an entry's blob, or a range of it, to a file"
sidebar:
  label: "get"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Write an entry's blob, or a range of it, to a file

## Usage

```text
workspace blob get [OPTIONS] --out <FILE> <PATH>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <PATH> |  | Workspace path whose blob is read |
| --out <FILE> |  |  |
| --offset <BYTES> | 0 |  |
| --limit <BYTES> |  |  |

