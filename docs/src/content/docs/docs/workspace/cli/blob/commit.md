---
title: "workspace blob commit"
description: "Verify a direct upload, store it canonically, and create its entry"
sidebar:
  label: "commit"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Verify a direct upload, store it canonically, and create its entry

## Usage

```text
workspace blob commit [OPTIONS] --ticket <KEY> --hash <HASH> <PATH>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <PATH> |  | Workspace path for the new entry |
| --ticket <KEY> |  | Staging key printed by `blob presign` |
| --hash <HASH> |  | Hash the client computed for the uploaded bytes |
| --size <BYTES> |  | Expected byte count; checked before the object is copied |
| --mime <MIME> |  |  |

