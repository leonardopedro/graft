---
title: "workspace update deploy"
description: "Install a release into a new deployment and atomically switch `current`"
sidebar:
  label: "deploy"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Install a release into a new deployment and atomically switch `current`

## Usage

```text
workspace update deploy [OPTIONS] <NAME> <RELEASE> <ROOT>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <NAME> |  | Repository name |
| <RELEASE> |  | Release identifier |
| <ROOT> |  | Deployment root (holds `deployments/`, `states/` and `current`) |
| --public-key <FILE> |  | Require a manifest signed by this Ed25519 public key |

