---
title: "workspace doc ack"
description: "Record how far a consumer has read a producer's stream"
sidebar:
  label: "ack"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Record how far a consumer has read a producer's stream

## Usage

```text
workspace doc ack --producer <PRODUCER> --consumer <CONSUMER> --seq <SEQ> <DOC>
```

## Arguments and options

| Argument or option | Default | Description |
| --- | --- | --- |
| <DOC> |  | Document id |
| --producer <PRODUCER> |  | Producer whose stream is being acknowledged |
| --consumer <CONSUMER> |  | Consumer doing the acknowledging |
| --seq <SEQ> |  | Sequence number consumed through |

