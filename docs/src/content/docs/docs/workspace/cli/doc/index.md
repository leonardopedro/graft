---
title: "workspace doc"
description: "Live-document frames: the Loro oplog lane"
sidebar:
  label: "doc"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Live-document frames: the Loro oplog lane

## Usage

```text
workspace doc <COMMAND>
```

## Commands

| Command | Description |
| --- | --- |
| [`ls`](/docs/workspace/cli/doc/ls/) | List documents with frame counts |
| [`stats`](/docs/workspace/cli/doc/stats/) | Print one document's oplog state and frontier |
| [`append`](/docs/workspace/cli/doc/append/) | Offer one frame from a producer. The payload is read from a file |
| [`frames`](/docs/workspace/cli/doc/frames/) | Print, or write out, frames after the given cursors |
| [`ack`](/docs/workspace/cli/doc/ack/) | Record how far a consumer has read a producer's stream |
| [`compact`](/docs/workspace/cli/doc/compact/) | Delete frames every known consumer has acknowledged |
| [`snapshot`](/docs/workspace/cli/doc/snapshot/) | Record a compacted snapshot frontier on catalog entries |

