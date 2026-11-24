-- Workspace catalog schema, version 1.
--
-- Canonical definition: `graft-workspace` applies this file verbatim on a fresh
-- catalog volume, and `tests/sql/test_workspace_catalog.sql` reads it so the
-- SQL fixtures exercise the same DDL the daemon ships.
--
-- Invariants:
--   * `entries` is the only mutable table. Blobs and trees are content
--     addressed and are written before the row that references them.
--   * `entries.path` is unique per workspace, not globally.
--   * `blobs.hash` is `blake3:<hex>`; `sha256` is filled only when a portable
--     digest is needed (releases).
--   * `refcount` is advisory. Liveness is recomputed from `entries` plus
--     pinned blobs, because drift after a crash would otherwise be permanent.

CREATE TABLE catalog_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE blobs (
    hash       TEXT PRIMARY KEY,
    sha256     TEXT UNIQUE,
    size       INTEGER NOT NULL,
    mime       TEXT,
    storage    TEXT NOT NULL CHECK (storage IN ('raw', 'cdc')),
    locator    TEXT NOT NULL,
    refcount   INTEGER NOT NULL DEFAULT 0,
    pinned     INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE entries (
    workspace_id  TEXT NOT NULL,
    path          TEXT NOT NULL,
    lane          TEXT NOT NULL CHECK (lane IN ('loro', 'code', 'media', 'archive')),
    kind          TEXT NOT NULL CHECK (kind IN ('doc', 'file', 'dir', 'dataset', 'release')),
    blob_hash     TEXT REFERENCES blobs (hash),
    loro_doc_id   TEXT,
    loro_frontier BLOB,
    code_tree     TEXT,
    release_doi   TEXT,
    mime          TEXT,
    size          INTEGER,
    revision      INTEGER NOT NULL DEFAULT 0,
    deleted_at    INTEGER,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, path)
) STRICT;

CREATE INDEX entries_lane_idx ON entries (workspace_id, lane);
CREATE INDEX entries_blob_idx ON entries (blob_hash);

CREATE TABLE releases (
    doi           TEXT PRIMARY KEY,
    concept_doi   TEXT,
    workspace_id  TEXT NOT NULL,
    manifest_hash TEXT NOT NULL,
    zenodo_record INTEGER NOT NULL,
    published_at  INTEGER NOT NULL
) STRICT;

-- Releases published to binary-update repositories. A row pins its manifest,
-- so `workspace update gc` keeps the manifest and chunks even without a ref
-- until the row is forgotten by `prune-refs`.
CREATE TABLE update_releases (
    workspace_id  TEXT NOT NULL,
    store         TEXT NOT NULL,
    prefix        TEXT NOT NULL,
    name          TEXT NOT NULL,
    release       TEXT NOT NULL,
    manifest_hash TEXT NOT NULL,
    files         INTEGER NOT NULL,
    chunks        INTEGER NOT NULL,
    bytes         INTEGER NOT NULL,
    pinned        INTEGER NOT NULL DEFAULT 1 CHECK (pinned IN (0, 1)),
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, store, prefix, name, release)
) STRICT;

-- Live-document sequencing and acks live in the oplog volume
-- (`sql/oplog.sql`), not here: ack traffic must not contend with the catalog.
-- `entries.loro_frontier` is the only lane-1 pointer stored in this database.

PRAGMA user_version = 1;
