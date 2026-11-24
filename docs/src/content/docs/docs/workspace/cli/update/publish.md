---
title: "workspace update publish"
description: "Publish a directory as an immutable release"
sidebar:
  label: "publish"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Publish a directory as an immutable release

## Usage

```text
workspace update publish [OPTIONS] <NAME> <RELEASE> <ROOT>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <NAME> |  | Repository name, e.g. `my-app` |
| <RELEASE> |  | Release identifier, e.g. `1.4.0` |
| <ROOT> |  | Directory to publish |
| --signing-key <FILE> |  | Ed25519 private key (64 hex characters) used to sign the manifest |
| --compress |  | Store zstd-compressed chunks when they are smaller than raw chunks |
| --compress-level <LEVEL> |  | Zstd compression level (1-22); implies --compress |

