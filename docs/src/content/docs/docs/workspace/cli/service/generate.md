---
title: "workspace service generate"
description: "Write systemd units or a launchd plist for scheduled one-shot backups"
sidebar:
  label: "generate"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Write systemd units or a launchd plist for scheduled one-shot backups

## Usage

```text
workspace service generate [OPTIONS] --out <DIR>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| --kind <KIND> |  | Service manager format. Defaults to the current platform. One of: `systemd`, `launchd`. |
| --interval <SECS> | 3600 | Seconds between backups |
| --keep <N> | 7 | Number of complete backups to keep |
| --name <NAME> | workspace-backup | Unit/plist base name |
| --out <DIR> |  | Directory to write the generated files into |
| --exec <PATH> |  | `workspace` executable to invoke; defaults to this binary |

