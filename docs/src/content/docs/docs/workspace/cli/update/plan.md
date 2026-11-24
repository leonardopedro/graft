---
title: "workspace update plan"
description: "Compare a release with an installed-state manifest"
sidebar:
  label: "plan"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Compare a release with an installed-state manifest

## Usage

```text
workspace update plan [OPTIONS] <NAME> <RELEASE>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <NAME> |  | Repository name |
| <RELEASE> |  | Release identifier |
| --state <FILE> |  | Manifest written by a previous apply or `update state` |
| --public-key <FILE> |  | Require a manifest signed by this Ed25519 public key |

