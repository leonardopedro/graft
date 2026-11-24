---
title: "workspace update prune-deployments"
description: "Delete old deployments, keeping the active and previous versions"
sidebar:
  label: "prune-deployments"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Delete old deployments, keeping the active and previous versions

## Usage

```text
workspace update prune-deployments [OPTIONS] --keep <N> <ROOT>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <ROOT> |  | Deployment root |
| --keep <N> |  | Number of deployments to keep, including current and previous |
| --dry-run |  | Report what would be removed without deleting it |

