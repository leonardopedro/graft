---
title: "workspace update apply"
description: "Apply a release, downloading only chunks absent from the installed tree"
sidebar:
  label: "apply"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Apply a release, downloading only chunks absent from the installed tree

## Usage

```text
workspace update apply [OPTIONS] --state <FILE> <NAME> <RELEASE> <ROOT>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <NAME> |  | Repository name |
| <RELEASE> |  | Release identifier |
| <ROOT> |  | Destination directory |
| --state <FILE> |  | Installed-state manifest; written after a successful update |
| --delete |  | Remove files that the previous state contains and this release does not |
| --public-key <FILE> |  | Require a manifest signed by this Ed25519 public key |

