---
title: "workspace update prune-refs"
description: "Delete old release refs, keeping the newest ones"
sidebar:
  label: "prune-refs"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Delete old release refs, keeping the newest ones

## Usage

```text
workspace update prune-refs [OPTIONS] --keep <N> <NAME>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <NAME> |  | Repository name |
| --keep <N> |  | Number of most recently published refs to keep |
| --grace-secs <SECS> | 86400 | Keep refs published within this many seconds |
| --dry-run |  | Report what would be removed without deleting it |

