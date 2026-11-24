---
title: "workspace doc append"
description: "Offer one frame from a producer. The payload is read from a file"
sidebar:
  label: "append"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Offer one frame from a producer. The payload is read from a file

## Usage

```text
workspace doc append --peer <PEER> --seq <SEQ> --file <FILE> <DOC>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <DOC> |  | Document id |
| --peer <PEER> |  | Producer name; sequences are per (document, producer) |
| --seq <SEQ> |  | Next sequence number for this producer, starting at 1 |
| --file <FILE> |  | File holding the opaque frame payload |

