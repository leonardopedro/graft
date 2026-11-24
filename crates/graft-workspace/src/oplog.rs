//! The Loro oplog lane: a durable, per-document, per-peer frame log.
//!
//! The coordinator never interprets Loro data. It stores opaque, sequenced
//! frames, hands them to reconnecting clients from a cursor, and records how
//! far each consumer has read. After compaction it stores the materialized
//! Loro snapshot and the sequence frontier that snapshot covers, so a client
//! that missed pruned frames still gets the document state. Convergence is
//! Loro's job; this module only guarantees that an applied frame is durable
//! and that a client can ask for everything it has not seen yet.
//!
//! Frames are stored in a SQLite database in a **separate Graft volume** from
//! the catalog (`sql/oplog.sql`). Ack traffic therefore never contends with
//! catalog writes, and the oplog can be compacted independently.

use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

pub const OPLOG_SCHEMA_VERSION: i64 = 2;
pub const OPLOG_SCHEMA_SQL: &str = include_str!("../sql/oplog.sql");

/// Version 1 kept only frames. Version 2 adds the compacted snapshot a
/// reconnecting client needs once early frames have been pruned.
const OPLOG_MIGRATION_1_TO_2: &str = "
CREATE TABLE snapshots (
    doc_id     TEXT PRIMARY KEY,
    frontier   TEXT NOT NULL,
    data       BLOB NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
PRAGMA user_version = 2;
";

/// One frame's upper bound. A larger update is a snapshot, not a delta, and
/// belongs in the catalog's frontier/snapshot path instead.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

const MAX_IDENT_LEN: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub peer: String,
    pub seq: i64,
    pub data: Vec<u8>,
}

/// A compacted document snapshot plus the per-producer sequence it covers.
/// A client behind `frontier` imports `data` before applying newer frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub doc_id: String,
    pub frontier: Vec<(String, i64)>,
    pub data: Vec<u8>,
}

/// Summary of one automatic compaction pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    /// Documents that already had a stored snapshot or at least one frame.
    pub docs: usize,
    /// Documents whose frames were complete enough to materialize.
    pub snapshots: usize,
    /// Producers whose acknowledged prefix could be pruned.
    pub compacted: usize,
    /// Producers with a consumer still behind their applied prefix.
    pub blocked: usize,
    /// Frame rows removed across all producers.
    pub frames_deleted: usize,
    /// Documents whose stored frames could not be materialized (for example
    /// because a frame was not valid Loro data).
    pub failed: usize,
}

/// Result of offering a frame to the coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendOutcome {
    /// Sequenced, committed, and durable.
    Applied { peer: String, seq: i64 },
    /// Already applied; the producer is replaying.
    Duplicate { peer: String, seq: i64 },
    /// Out of order; the producer must resend from `expected`.
    Gap { peer: String, expected: i64 },
}

/// Result of an acknowledged compaction pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    /// Every known consumer had acked through the requested sequence.
    Compacted { through: i64, frames: usize },
    /// A known consumer is behind; nothing was deleted.
    Blocked {
        consumer: String,
        acked: i64,
        through: i64,
    },
    /// `through < 1`, so there was nothing to compact.
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSummary {
    pub doc_id: String,
    pub peers: i64,
    pub frames: i64,
    pub bytes: i64,
}

#[derive(Debug, Error)]
pub enum OplogErr {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("oplog schema version {found} is not supported (expected {supported})")]
    UnsupportedSchema { found: i64, supported: i64 },

    #[error("invalid document id {0:?}")]
    InvalidDocId(String),

    #[error("invalid peer id {0:?}")]
    InvalidPeer(String),

    #[error("invalid sequence {0}: sequences start at 1")]
    InvalidSeq(i64),

    #[error("frame of {size} bytes exceeds the {max} byte limit")]
    FrameTooLarge { size: usize, max: usize },

    #[error("cannot compact through {through}: only {applied} frames were applied")]
    BeyondApplied { through: i64, applied: i64 },

    #[error("stored snapshot for {0:?} has an invalid frontier")]
    CorruptSnapshot(String),
}

/// An open oplog volume. One writer per volume, enforced by Graft.
pub struct Oplog {
    conn: Connection,
}

impl Oplog {
    pub fn open(conn: Connection) -> Result<Oplog, OplogErr> {
        conn.busy_timeout(Duration::from_secs(5))?;

        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 0 {
            conn.execute_batch(OPLOG_SCHEMA_SQL)?;
        } else if version == 1 {
            conn.execute_batch(OPLOG_MIGRATION_1_TO_2)?;
        }

        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != OPLOG_SCHEMA_VERSION {
            return Err(OplogErr::UnsupportedSchema {
                found: version,
                supported: OPLOG_SCHEMA_VERSION,
            });
        }

        Ok(Oplog { conn })
    }

    /// Offer the next frame from `peer`. Frames are accepted strictly in order,
    /// which is what makes `Duplicate`/`Gap` definitive without buffering.
    pub fn append(
        &self,
        doc_id: &str,
        peer: &str,
        seq: i64,
        data: &[u8],
    ) -> Result<AppendOutcome, OplogErr> {
        validate_doc(doc_id)?;
        validate_peer(peer)?;
        if seq < 1 {
            return Err(OplogErr::InvalidSeq(seq));
        }
        if data.len() > MAX_FRAME_BYTES {
            return Err(OplogErr::FrameTooLarge { size: data.len(), max: MAX_FRAME_BYTES });
        }

        let expected = self.next_seq(doc_id, peer)?;
        if seq < expected {
            return Ok(AppendOutcome::Duplicate { peer: peer.to_owned(), seq });
        }
        if seq > expected {
            return Ok(AppendOutcome::Gap { peer: peer.to_owned(), expected });
        }

        let now = now_unix();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO docs (doc_id, created_at) VALUES (?1, ?2)
             ON CONFLICT (doc_id) DO NOTHING",
            params![doc_id, now],
        )?;
        tx.execute(
            "INSERT INTO frames (doc_id, peer, seq, data, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![doc_id, peer, seq, data, now],
        )?;
        tx.execute(
            "INSERT INTO producers (doc_id, peer, next_seq, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (doc_id, peer) DO UPDATE SET
                 next_seq = excluded.next_seq,
                 updated_at = excluded.updated_at",
            params![doc_id, peer, seq + 1, now],
        )?;
        tx.commit()?;

        Ok(AppendOutcome::Applied { peer: peer.to_owned(), seq })
    }

    /// Every stored frame after the caller's per-peer cursors, ordered by
    /// producer then sequence. Producers without a cursor start at 1.
    pub fn frames_since(
        &self,
        doc_id: &str,
        cursors: &[(String, i64)],
    ) -> Result<Vec<Frame>, OplogErr> {
        validate_doc(doc_id)?;
        let cursors: HashMap<&str, i64> = cursors
            .iter()
            .map(|(peer, seq)| (peer.as_str(), *seq))
            .collect();

        let mut stmt = self
            .conn
            .prepare("SELECT peer, seq, data FROM frames WHERE doc_id = ?1 ORDER BY peer, seq")?;
        let rows = stmt.query_map([doc_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;

        let mut frames = Vec::new();
        for row in rows {
            let (peer, seq, data) = row?;
            let after = cursors.get(peer.as_str()).copied().unwrap_or(0);
            if seq > after {
                frames.push(Frame { peer, seq, data });
            }
        }
        Ok(frames)
    }

    /// The next sequence number the coordinator expects from `peer`.
    pub fn next_seq(&self, doc_id: &str, peer: &str) -> Result<i64, OplogErr> {
        validate_doc(doc_id)?;
        validate_peer(peer)?;
        let next = self
            .conn
            .query_row(
                "SELECT next_seq FROM producers WHERE doc_id = ?1 AND peer = ?2",
                params![doc_id, peer],
                |row| row.get(0),
            )
            .optional()?;
        Ok(next.unwrap_or(1))
    }

    /// Record that `consumer` has consumed `producer`'s stream through `seq`.
    /// Acks are monotonic and clamped to what the coordinator has actually
    /// applied, so a bogus ack cannot prune unread frames. Returns the
    /// effective watermark.
    pub fn ack(
        &self,
        doc_id: &str,
        producer: &str,
        consumer: &str,
        seq: i64,
    ) -> Result<i64, OplogErr> {
        validate_doc(doc_id)?;
        validate_peer(producer)?;
        validate_peer(consumer)?;

        let applied = (self.next_seq(doc_id, producer)? - 1).max(0);
        let acked = seq.clamp(0, applied);

        self.conn.execute(
            "INSERT INTO acks (doc_id, producer, consumer, acked_seq, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (doc_id, producer, consumer) DO UPDATE SET
                 acked_seq = MAX(acks.acked_seq, excluded.acked_seq),
                 updated_at = excluded.updated_at",
            params![doc_id, producer, consumer, acked, now_unix()],
        )?;

        self.acked(doc_id, producer, consumer)
    }

    pub fn acked(&self, doc_id: &str, producer: &str, consumer: &str) -> Result<i64, OplogErr> {
        validate_doc(doc_id)?;
        validate_peer(producer)?;
        validate_peer(consumer)?;
        let acked = self
            .conn
            .query_row(
                "SELECT acked_seq FROM acks
                  WHERE doc_id = ?1 AND producer = ?2 AND consumer = ?3",
                params![doc_id, producer, consumer],
                |row| row.get(0),
            )
            .optional()?;
        Ok(acked.unwrap_or(0))
    }

    /// Store a compacted snapshot and advance each producer's expected next
    /// sequence past the frontier. Replacing an older snapshot is allowed:
    /// frames newer than the new frontier stay in place.
    pub fn put_snapshot(
        &self,
        doc_id: &str,
        frontier: &[(String, i64)],
        data: &[u8],
    ) -> Result<(), OplogErr> {
        validate_doc(doc_id)?;
        if data.is_empty() || frontier.is_empty() {
            return Err(OplogErr::CorruptSnapshot(doc_id.to_string()));
        }
        for (peer, seq) in frontier {
            validate_peer(peer)?;
            if *seq < 0 {
                return Err(OplogErr::InvalidSeq(*seq));
            }
        }

        let frontier_json = serde_json::to_string(frontier)
            .map_err(|_| OplogErr::CorruptSnapshot(doc_id.to_string()))?;
        let now = now_unix();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO docs (doc_id, created_at) VALUES (?1, ?2)
             ON CONFLICT (doc_id) DO NOTHING",
            params![doc_id, now],
        )?;
        tx.execute(
            "INSERT INTO snapshots (doc_id, frontier, data, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (doc_id) DO UPDATE SET
                 frontier = excluded.frontier,
                 data = excluded.data,
                 created_at = excluded.created_at",
            params![doc_id, frontier_json, data, now],
        )?;
        for (peer, seq) in frontier {
            tx.execute(
                "INSERT INTO producers (doc_id, peer, next_seq, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (doc_id, peer) DO UPDATE SET
                     next_seq = MAX(producers.next_seq, excluded.next_seq),
                     updated_at = excluded.updated_at",
                params![doc_id, peer, seq + 1, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The latest stored snapshot, if the document was compacted after this
    /// schema version was introduced.
    pub fn snapshot(&self, doc_id: &str) -> Result<Option<Snapshot>, OplogErr> {
        validate_doc(doc_id)?;
        let row = self
            .conn
            .query_row(
                "SELECT frontier, data FROM snapshots WHERE doc_id = ?1",
                [doc_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let Some((frontier, data)) = row else {
            return Ok(None);
        };
        let frontier = serde_json::from_str(&frontier)
            .map_err(|_| OplogErr::CorruptSnapshot(doc_id.to_string()))?;
        Ok(Some(Snapshot {
            doc_id: doc_id.to_string(),
            frontier,
            data,
        }))
    }

    /// Delete frames through `through`, but only if every consumer known to
    /// the coordinator has acked that far. This is the safe form of [`prune`]:
    /// a caller compacts after writing a snapshot, and a lagging consumer
    /// blocks the pass instead of losing frames it has not read.
    ///
    /// Consumers are known from their ack rows; a peer that has never acked
    /// cannot be waited for. `through` may not exceed what the coordinator
    /// applied — that is a caller bug, not an empty compaction.
    pub fn compact(
        &self,
        doc_id: &str,
        producer: &str,
        through: i64,
    ) -> Result<CompactOutcome, OplogErr> {
        validate_doc(doc_id)?;
        validate_peer(producer)?;
        if through < 1 {
            return Ok(CompactOutcome::Nothing);
        }

        let applied = (self.next_seq(doc_id, producer)? - 1).max(0);
        if through > applied {
            return Err(OplogErr::BeyondApplied { through, applied });
        }

        let lagging: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT consumer, acked_seq FROM acks
                  WHERE doc_id = ?1 AND producer = ?2 AND acked_seq < ?3
                  ORDER BY consumer LIMIT 1",
                params![doc_id, producer, through],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((consumer, acked)) = lagging {
            return Ok(CompactOutcome::Blocked { consumer, acked, through });
        }

        let frames = self.prune(doc_id, producer, through)?;
        Ok(CompactOutcome::Compacted { through, frames })
    }

    /// Delete frames up to `through` without checking acks. Callers that have
    /// not verified every consumer should use [`compact`] instead. Returns
    /// rows deleted.
    pub fn prune(&self, doc_id: &str, producer: &str, through: i64) -> Result<usize, OplogErr> {
        validate_doc(doc_id)?;
        validate_peer(producer)?;
        if through < 1 {
            return Ok(0);
        }
        let deleted = self.conn.execute(
            "DELETE FROM frames WHERE doc_id = ?1 AND peer = ?2 AND seq <= ?3",
            params![doc_id, producer, through],
        )?;
        Ok(deleted)
    }

    /// Per-producer applied watermarks: what a reconnecting client must catch
    /// up to, and what a manifest can record.
    pub fn frontier(&self, doc_id: &str) -> Result<Vec<(String, i64)>, OplogErr> {
        validate_doc(doc_id)?;
        let mut stmt = self
            .conn
            .prepare("SELECT peer, next_seq - 1 FROM producers WHERE doc_id = ?1 ORDER BY peer")?;
        let rows = stmt.query_map([doc_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut frontier = Vec::new();
        for row in rows {
            frontier.push(row?);
        }
        Ok(frontier)
    }

    pub fn docs(&self) -> Result<Vec<DocSummary>, OplogErr> {
        let mut stmt = self.conn.prepare(
            "SELECT d.doc_id,
                    (SELECT COUNT(*) FROM producers p WHERE p.doc_id = d.doc_id),
                    (SELECT COUNT(*) FROM frames f WHERE f.doc_id = d.doc_id),
                    (SELECT COALESCE(SUM(LENGTH(f.data)), 0) FROM frames f
                      WHERE f.doc_id = d.doc_id)
               FROM docs d
              ORDER BY d.doc_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DocSummary {
                doc_id: row.get(0)?,
                peers: row.get(1)?,
                frames: row.get(2)?,
                bytes: row.get(3)?,
            })
        })?;
        let mut docs = Vec::new();
        for row in rows {
            docs.push(row?);
        }
        Ok(docs)
    }

    pub fn doc_summary(&self, doc_id: &str) -> Result<Option<DocSummary>, OplogErr> {
        validate_doc(doc_id)?;
        Ok(self
            .conn
            .query_row(
                "SELECT d.doc_id,
                        (SELECT COUNT(*) FROM producers p WHERE p.doc_id = d.doc_id),
                        (SELECT COUNT(*) FROM frames f WHERE f.doc_id = d.doc_id),
                        (SELECT COALESCE(SUM(LENGTH(f.data)), 0) FROM frames f
                          WHERE f.doc_id = d.doc_id)
                   FROM docs d
                  WHERE d.doc_id = ?1",
                [doc_id],
                |row| {
                    Ok(DocSummary {
                        doc_id: row.get(0)?,
                        peers: row.get(1)?,
                        frames: row.get(2)?,
                        bytes: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }
}

fn validate_doc(doc_id: &str) -> Result<(), OplogErr> {
    validate_ident(doc_id).ok_or_else(|| OplogErr::InvalidDocId(doc_id.to_owned()))
}

fn validate_peer(peer: &str) -> Result<(), OplogErr> {
    validate_ident(peer).ok_or_else(|| OplogErr::InvalidPeer(peer.to_owned()))
}

/// Identifiers are used in filenames by the CLI and in grants, so keep them to
/// a conservative alphabet.
fn validate_ident(value: &str) -> Option<()> {
    if value.is_empty() || value.len() > MAX_IDENT_LEN {
        return None;
    }
    let ok = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    ok.then_some(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oplog() -> Oplog {
        Oplog::open(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn applied(outcome: AppendOutcome) -> i64 {
        match outcome {
            AppendOutcome::Applied { seq, .. } => seq,
            other => panic!("expected applied, got {other:?}"),
        }
    }

    #[test]
    fn frames_apply_in_order_and_replays_are_duplicates() {
        let oplog = oplog();
        assert_eq!(applied(oplog.append("doc", "alice", 1, b"a").unwrap()), 1);
        assert_eq!(applied(oplog.append("doc", "alice", 2, b"b").unwrap()), 2);

        assert_eq!(
            oplog.append("doc", "alice", 2, b"b").unwrap(),
            AppendOutcome::Duplicate { peer: "alice".into(), seq: 2 }
        );
        assert_eq!(
            oplog.append("doc", "alice", 1, b"a").unwrap(),
            AppendOutcome::Duplicate { peer: "alice".into(), seq: 1 }
        );
        assert_eq!(oplog.next_seq("doc", "alice").unwrap(), 3);
    }

    #[test]
    fn gaps_report_the_expected_sequence_and_apply_nothing() {
        let oplog = oplog();
        assert_eq!(
            oplog.append("doc", "alice", 3, b"c").unwrap(),
            AppendOutcome::Gap { peer: "alice".into(), expected: 1 }
        );
        assert_eq!(oplog.next_seq("doc", "alice").unwrap(), 1);
        assert!(oplog.frames_since("doc", &[]).unwrap().is_empty());
        assert!(oplog.doc_summary("doc").unwrap().is_none());
    }

    #[test]
    fn peers_are_sequenced_independently() {
        let oplog = oplog();
        applied(oplog.append("doc", "alice", 1, b"a1").unwrap());
        applied(oplog.append("doc", "bob", 1, b"b1").unwrap());
        applied(oplog.append("doc", "alice", 2, b"a2").unwrap());

        let frames = oplog.frames_since("doc", &[]).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].peer, "alice");
        assert_eq!(frames[0].seq, 1);
        assert_eq!(frames[2].peer, "bob");
        assert_eq!(
            oplog.frontier("doc").unwrap(),
            vec![("alice".to_string(), 2), ("bob".to_string(), 1)]
        );
    }

    #[test]
    fn frames_since_returns_only_the_tail() {
        let oplog = oplog();
        for seq in 1..=3 {
            applied(oplog.append("doc", "alice", seq, b"x").unwrap());
        }
        applied(oplog.append("doc", "bob", 1, b"y").unwrap());

        let cursors = vec![("alice".to_string(), 2)];
        let frames = oplog.frames_since("doc", &cursors).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().any(|f| f.peer == "alice" && f.seq == 3));
        assert!(frames.iter().any(|f| f.peer == "bob" && f.seq == 1));

        let cursors = vec![("alice".to_string(), 3), ("bob".to_string(), 1)];
        assert!(oplog.frames_since("doc", &cursors).unwrap().is_empty());
    }

    #[test]
    fn acks_are_monotonic_and_clamped() {
        let oplog = oplog();
        assert_eq!(oplog.ack("doc", "alice", "bob", 5).unwrap(), 0);
        for seq in 1..=4 {
            applied(oplog.append("doc", "alice", seq, b"x").unwrap());
        }
        assert_eq!(oplog.ack("doc", "alice", "bob", 2).unwrap(), 2);
        assert_eq!(oplog.ack("doc", "alice", "bob", 1).unwrap(), 2);
        // Cannot ack past what the coordinator applied.
        assert_eq!(oplog.ack("doc", "alice", "bob", 99).unwrap(), 4);
        assert_eq!(oplog.acked("doc", "alice", "bob").unwrap(), 4);
    }

    #[test]
    fn prune_removes_only_frames_through_the_watermark() {
        let oplog = oplog();
        for seq in 1..=3 {
            applied(oplog.append("doc", "alice", seq, b"x").unwrap());
        }
        assert_eq!(oplog.prune("doc", "alice", 2).unwrap(), 2);
        let frames = oplog.frames_since("doc", &[]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].seq, 3);
        // Replays of pruned sequences are still duplicates, not gaps.
        assert!(matches!(
            oplog.append("doc", "alice", 1, b"x").unwrap(),
            AppendOutcome::Duplicate { .. }
        ));
        assert_eq!(oplog.prune("doc", "alice", 0).unwrap(), 0);
    }

    #[test]
    fn compact_waits_for_every_known_consumer() {
        let oplog = oplog();
        for seq in 1..=3 {
            applied(oplog.append("doc", "alice", seq, b"x").unwrap());
        }
        assert_eq!(
            oplog.compact("doc", "alice", 3).unwrap(),
            CompactOutcome::Compacted { through: 3, frames: 3 },
            "with no ack rows there is no known consumer to wait for"
        );

        for seq in 1..=3 {
            applied(oplog.append("doc2", "alice", seq, b"x").unwrap());
        }
        oplog.ack("doc2", "alice", "bob", 1).unwrap();
        assert_eq!(
            oplog.compact("doc2", "alice", 2).unwrap(),
            CompactOutcome::Blocked {
                consumer: "bob".into(),
                acked: 1,
                through: 2
            }
        );
        assert_eq!(oplog.frames_since("doc2", &[]).unwrap().len(), 3);

        oplog.ack("doc2", "alice", "bob", 2).unwrap();
        assert_eq!(
            oplog.compact("doc2", "alice", 2).unwrap(),
            CompactOutcome::Compacted { through: 2, frames: 2 }
        );
        assert_eq!(oplog.frames_since("doc2", &[]).unwrap().len(), 1);
    }

    #[test]
    fn compact_rejects_a_frontier_the_coordinator_never_applied() {
        let oplog = oplog();
        applied(oplog.append("doc", "alice", 1, b"x").unwrap());
        assert!(matches!(
            oplog.compact("doc", "alice", 2),
            Err(OplogErr::BeyondApplied { through: 2, applied: 1 })
        ));
        assert_eq!(
            oplog.compact("doc", "alice", 0).unwrap(),
            CompactOutcome::Nothing
        );
    }

    #[test]
    fn snapshots_cover_a_prefix_and_survive_pruning() {
        let oplog = oplog();
        for seq in 1..=2 {
            applied(oplog.append("doc", "alice", seq, b"x").unwrap());
        }
        oplog
            .put_snapshot("doc", &[("alice".into(), 2)], b"snapshot")
            .unwrap();
        assert_eq!(oplog.next_seq("doc", "alice").unwrap(), 3);
        let snapshot = oplog.snapshot("doc").unwrap().unwrap();
        assert_eq!(snapshot.frontier, vec![("alice".to_string(), 2)]);
        assert_eq!(snapshot.data, b"snapshot");

        assert_eq!(
            oplog.compact("doc", "alice", 2).unwrap(),
            CompactOutcome::Compacted { through: 2, frames: 2 }
        );
        assert!(oplog.frames_since("doc", &[]).unwrap().is_empty());
        assert_eq!(oplog.snapshot("doc").unwrap().unwrap().data, b"snapshot");

        // New frames continue from the stored frontier.
        applied(oplog.append("doc", "alice", 3, b"y").unwrap());
        assert_eq!(oplog.frames_since("doc", &[]).unwrap().len(), 1);
    }

    #[test]
    fn version_one_oplogs_migrate_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oplog.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE oplog_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
             CREATE TABLE docs (doc_id TEXT PRIMARY KEY, created_at INTEGER NOT NULL) STRICT;
             CREATE TABLE producers (
                 doc_id TEXT NOT NULL, peer TEXT NOT NULL,
                 next_seq INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL,
                 PRIMARY KEY (doc_id, peer)) STRICT;
             CREATE TABLE frames (
                 doc_id TEXT NOT NULL, peer TEXT NOT NULL, seq INTEGER NOT NULL,
                 data BLOB NOT NULL, received_at INTEGER NOT NULL,
                 PRIMARY KEY (doc_id, peer, seq)) STRICT;
             CREATE TABLE acks (
                 doc_id TEXT NOT NULL, producer TEXT NOT NULL, consumer TEXT NOT NULL,
                 acked_seq INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL,
                 PRIMARY KEY (doc_id, producer, consumer)) STRICT;
             PRAGMA user_version = 1;",
        )
        .unwrap();
        drop(conn);

        let oplog = Oplog::open(Connection::open(&path).unwrap()).unwrap();
        oplog
            .put_snapshot("doc", &[("alice".into(), 1)], b"snapshot")
            .unwrap();
        assert_eq!(oplog.snapshot("doc").unwrap().unwrap().data, b"snapshot");
    }

    #[test]
    fn docs_summary_counts_peers_frames_and_bytes() {
        let oplog = oplog();
        applied(oplog.append("a", "alice", 1, b"123").unwrap());
        applied(oplog.append("a", "alice", 2, b"12").unwrap());
        applied(oplog.append("a", "bob", 1, b"1").unwrap());
        applied(oplog.append("b", "alice", 1, b"12345").unwrap());

        let docs = oplog.docs().unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(
            docs[0],
            DocSummary {
                doc_id: "a".into(),
                peers: 2,
                frames: 3,
                bytes: 6
            }
        );
        assert_eq!(docs[1].bytes, 5);
    }

    #[test]
    fn documents_are_isolated() {
        let oplog = oplog();
        applied(oplog.append("doc-a", "alice", 1, b"a").unwrap());
        assert!(oplog.frames_since("doc-b", &[]).unwrap().is_empty());
        assert_eq!(oplog.next_seq("doc-b", "alice").unwrap(), 1);
    }

    #[test]
    fn rejects_invalid_identifiers_sequences_and_oversized_frames() {
        let oplog = oplog();
        assert!(matches!(
            oplog.append("", "alice", 1, b"x"),
            Err(OplogErr::InvalidDocId(_))
        ));
        assert!(matches!(
            oplog.append("doc", "bad peer", 1, b"x"),
            Err(OplogErr::InvalidPeer(_))
        ));
        assert!(matches!(
            oplog.append("doc", "alice", 0, b"x"),
            Err(OplogErr::InvalidSeq(0))
        ));
        let big = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(matches!(
            oplog.append("doc", "alice", 1, &big),
            Err(OplogErr::FrameTooLarge { .. })
        ));
        assert!(matches!(
            oplog.frames_since("../doc", &[]),
            Err(OplogErr::InvalidDocId(_))
        ));
    }
}
