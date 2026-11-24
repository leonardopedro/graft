---
title: "workspace update gc"
description: "Remove manifests and chunks unreachable from every release ref"
sidebar:
  label: "gc"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Remove manifests and chunks unreachable from every release ref

## Usage

```text
workspace update gc [OPTIONS] <NAME>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <NAME> |  | Repository name |
| --grace-secs <SECS> | 86400 | Keep unreferenced objects newer than this many seconds |
| --dry-run |  | Report what would be removed without deleting it |

