---
title: "workspace blob presign"
description: "Get a short-lived URL for a direct upload to staging"
sidebar:
  label: "presign"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Get a short-lived URL for a direct upload to staging

## Usage

```text
workspace blob presign [OPTIONS] <HASH>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <HASH> |  | Expected blob hash (`blake3:<hex>`), computed by the client |
| --expires <SECS> | 900 | Presigned request lifetime |

