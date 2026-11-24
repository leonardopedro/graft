---
title: "workspace serve"
description: "Serve live-document frames over a Unix socket"
sidebar:
  label: "serve"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Serve live-document frames over a Unix socket

## Usage

```text
workspace serve [OPTIONS] --socket <PATH>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| --socket <PATH> |  | Socket path for document clients |
| --compact-after-secs <SECS> | 300 | Compact acknowledged documents after this many seconds without a committed frame or ack. Zero disables idle compaction |

