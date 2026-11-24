-- Loro oplog schema, version 2.
--
-- The oplog lives in its own Graft volume (`<catalog_tag>-oplog`), not in the
-- catalog: frames arrive per peer and ack per frame, and putting either in the
-- catalog would turn edit traffic into catalog contention and catalog sync.
--
-- Sequencing is per (doc, producer): a producer numbers its own frames from 1
-- and the coordinator accepts them only in order. A frame is durable before it
-- is reported as applied, and a client that reconnects asks for every frame
-- after its cursor, so no acked frame can be lost.
--
-- `acks` records, for each consumer, how far it has consumed a producer's
-- stream. Liveness is computed from these rows; there is no advisory counter.

CREATE TABLE oplog_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE docs (
    doc_id     TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE producers (
    doc_id     TEXT NOT NULL,
    peer       TEXT NOT NULL,
    next_seq   INTEGER NOT NULL DEFAULT 1,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (doc_id, peer)
) STRICT;

CREATE TABLE frames (
    doc_id      TEXT NOT NULL,
    peer        TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    data        BLOB NOT NULL,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (doc_id, peer, seq)
) STRICT;

CREATE TABLE acks (
    doc_id     TEXT NOT NULL,
    producer   TEXT NOT NULL,
    consumer   TEXT NOT NULL,
    acked_seq  INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (doc_id, producer, consumer)
) STRICT;

-- Compaction keeps the state, not the history. `frontier` is JSON
-- `[[peer, seq], ...]`; frames at or below it may be pruned, and a joining
-- client imports `data` before applying newer frames.
CREATE TABLE snapshots (
    doc_id     TEXT PRIMARY KEY,
    frontier   TEXT NOT NULL,
    data       BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

PRAGMA user_version = 2;
