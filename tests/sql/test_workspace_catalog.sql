-- Catalog schema, constraints and revision behaviour through the Graft VFS.
.open "file:ws-catalog?vfs=graft"
pragma graft_switch="5rMJkf2zHE-2xMqqKcN8RLZh:74ggc1B6R4-2kkvcy9fi4CHJ:74ggc1B6jg-2udz14pbDayZC";
.echo off
.read crates/graft-workspace/sql/catalog.sql
.echo on
pragma foreign_keys=on;
pragma user_version;

INSERT INTO catalog_meta (key, value) VALUES ('workspace_id', 'w-test');

INSERT INTO blobs (hash, sha256, size, mime, storage, locator, created_at)
VALUES ('blake3:aa', 'aa', 3, 'image/png', 'raw', 'media/raw/w-test/aa', 1);
SELECT hash, size, storage, refcount, pinned FROM blobs;

-- The same path may exist in two workspaces.
INSERT INTO entries (workspace_id, path, lane, kind, blob_hash, mime, size, revision, updated_at)
VALUES ('w-test', 'media/cat.png', 'media', 'file', 'blake3:aa', 'image/png', 3, 1, 1);
INSERT INTO entries (workspace_id, path, lane, kind, revision, updated_at)
VALUES ('w-other', 'media/cat.png', 'media', 'file', 1, 1);
SELECT workspace_id, path, revision FROM entries ORDER BY workspace_id;

-- Upsert keeps one row per (workspace, path) and bumps the revision.
INSERT INTO entries (workspace_id, path, lane, kind, revision, updated_at)
VALUES ('w-test', 'media/cat.png', 'media', 'file', 2, 2)
ON CONFLICT (workspace_id, path) DO UPDATE SET
    revision = entries.revision + 1,
    updated_at = excluded.updated_at;
SELECT path, revision FROM entries WHERE workspace_id = 'w-test';

-- Lane is constrained.
INSERT INTO entries (workspace_id, path, lane, kind, updated_at)
VALUES ('w-test', 'x', 'bogus', 'doc', 1);

-- Blob references are enforced.
INSERT INTO entries (workspace_id, path, lane, kind, blob_hash, updated_at)
VALUES ('w-test', 'y', 'media', 'file', 'blake3:missing', 1);

-- Blob storage is constrained.
INSERT INTO blobs (hash, size, storage, locator, created_at)
VALUES ('blake3:bb', 1, 'zip', 'media/raw/bb', 1);

-- Soft delete keeps the row, so a release can still reference it.
UPDATE entries
   SET deleted_at = 3, revision = revision + 1
 WHERE workspace_id = 'w-test' AND path = 'media/cat.png';
SELECT path, revision, deleted_at FROM entries WHERE workspace_id = 'w-test';

-- A published release is recorded once per DOI, keyed to the workspace.
INSERT INTO releases (doi, concept_doi, workspace_id, manifest_hash, zenodo_record, published_at)
VALUES ('10.5281/zenodo.100', '10.5281/zenodo.99', 'w-test', 'blake3:manifest', 100, 4);
SELECT doi, concept_doi, workspace_id, manifest_hash, zenodo_record FROM releases;

pragma graft_snapshot;
pragma graft_status;
