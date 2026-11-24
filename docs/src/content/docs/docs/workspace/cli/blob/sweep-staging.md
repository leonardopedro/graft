---
title: "workspace blob sweep-staging"
description: "Remove abandoned staging objects"
sidebar:
  label: "sweep-staging"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Remove abandoned staging objects

## Usage

```text
workspace blob sweep-staging [OPTIONS]
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| --older-than <SECS> | 86400 | Only remove objects at least this old |

