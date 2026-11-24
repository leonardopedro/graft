---
title: "workspace update state"
description: "Write a manifest for an installed directory without contacting a repo"
sidebar:
  label: "state"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Write a manifest for an installed directory without contacting a repo

## Usage

```text
workspace update state --out <FILE> <NAME> <RELEASE> <ROOT>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <NAME> |  | Repository name to record |
| <RELEASE> |  | Release identifier to record |
| <ROOT> |  | Installed directory to scan |
| --out <FILE> |  | Manifest to write |

