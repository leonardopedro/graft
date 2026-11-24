---
title: "workspace doc compact"
description: "Delete frames every known consumer has acknowledged"
sidebar:
  label: "compact"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Delete frames every known consumer has acknowledged

## Usage

```text
workspace doc compact --producer <PRODUCER> --through <THROUGH> <DOC>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <DOC> |  | Document id |
| --producer <PRODUCER> |  | Producer whose stream is being compacted |
| --through <THROUGH> |  | Sequence to compact through; every consumer must have acked it |

