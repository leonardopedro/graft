---
title: "workspace blob"
description: "Read, write and inspect media blobs"
sidebar:
  label: "blob"
---

:::note
Generated from the `workspace` command definitions. Do not edit by hand; run `cargo run -p graft-workspace --example gen-cli-docs` instead.
:::

Read, write and inspect media blobs

## Usage

```text
workspace blob <COMMAND>
```

## Commands

| Command | Description |
| --- | --- |
| [`put`](/docs/workspace/cli/blob/put/) | Ingest a file and point an entry at it |
| [`presign`](/docs/workspace/cli/blob/presign/) | Get a short-lived URL for a direct upload to staging |
| [`commit`](/docs/workspace/cli/blob/commit/) | Verify a direct upload, store it canonically, and create its entry |
| [`get`](/docs/workspace/cli/blob/get/) | Write an entry's blob, or a range of it, to a file |
| [`sweep-staging`](/docs/workspace/cli/blob/sweep-staging/) | Remove abandoned staging objects |
| [`gc`](/docs/workspace/cli/blob/gc/) | Delete raw objects that no entry references and that no release pins |
| [`gc-cdc`](/docs/workspace/cli/blob/gc-cdc/) | Delete unreferenced archived blobs, then rewrite the encrypted archive volume into a fresh one and repoint its tag |
| [`ls`](/docs/workspace/cli/blob/ls/) | List recorded blobs |
| [`show`](/docs/workspace/cli/blob/show/) | Print one blob's record |

