//! Coordinator transport for the live-document lane.
//!
//! The framing is deliberately small: a `u32` length prefix and a binary body.
//! `Hello` carries the document and the client's per-producer cursors; the
//! server answers with a `Snapshot` when the client is behind a compacted
//! prefix, then the frames after those cursors (or after the snapshot), then a
//! `CaughtUp` frontier, so a reconnecting client knows exactly what it has. A
//! snapshot above one frame limit is sent as `SnapshotBegin`, bounded
//! `SnapshotChunk`s and `SnapshotEnd`; the client verifies the announced size
//! and blake3 hash before importing it. New `Delta` frames are relayed to every
//! other connection joined to the same document after they are committed to the
//! oplog. `Ack` only records retention progress; the coordinator never
//! withholds frames based on acks.
//!
//! This is the repository-side half of the plan: emthin's `collab_*` client
//! still needs to speak this framing. The protocol is transport-agnostic in
//! shape, so a WebSocket bridge can carry the same frames later.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use crate::Workspace;

/// Largest accepted frame body. Deltas are small; a snapshot that fits this
/// frame is sent whole, and larger snapshots use the chunked frames below.
/// It still bounds a broken peer's allocation per frame.
pub const MAX_FRAME_BYTES: usize = 16 << 20;

/// Payload size for one `SnapshotChunk`. Well below the frame limit, so even a
/// document many times the frame limit is transferred without ever building an
/// oversized frame.
pub const SNAPSHOT_CHUNK_BYTES: usize = 1 << 20;

const TAG_HELLO: u8 = 1;
const TAG_DELTA: u8 = 2;
const TAG_ACK: u8 = 3;
const TAG_BYE: u8 = 4;
const TAG_CAUGHT_UP: u8 = 5;
const TAG_ERROR: u8 = 6;
const TAG_SNAPSHOT: u8 = 7;
const TAG_SNAPSHOT_BEGIN: u8 = 8;
const TAG_SNAPSHOT_CHUNK: u8 = 9;
const TAG_SNAPSHOT_END: u8 = 10;

/// One message on the coordinator socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// First frame from a client: the document to join and the cursors it
    /// already has. Producers without a cursor start at 1.
    Hello {
        doc: String,
        peer: String,
        cursors: Vec<(String, i64)>,
    },
    /// An opaque payload from `peer` at `seq`.
    Delta {
        peer: String,
        seq: i64,
        data: Vec<u8>,
    },
    /// Compacted state to import before the deltas that follow. A client
    /// behind `frontier` must import it; a current client never receives one.
    /// Used only when the snapshot fits one frame; otherwise the three chunked
    /// frames below carry it.
    Snapshot {
        frontier: Vec<(String, i64)>,
        data: Vec<u8>,
    },
    /// Start of a snapshot too large for one frame. `size` and `hash` let the
    /// client reserve, bound and verify the reassembled bytes.
    SnapshotBegin {
        frontier: Vec<(String, i64)>,
        size: u64,
        hash: String,
    },
    /// A contiguous slice of the snapshot announced by `SnapshotBegin`.
    SnapshotChunk { offset: u64, data: Vec<u8> },
    /// Every byte announced by `SnapshotBegin` has been sent.
    SnapshotEnd,
    /// Record that the sender consumed through `seq` of `producer`.
    Ack { producer: String, seq: i64 },
    /// Client is leaving.
    Bye,
    /// Server finished replaying frames; the client is now current at
    /// `frontier` and later frames arrive as deltas.
    CaughtUp { frontier: Vec<(String, i64)> },
    /// Server-side rejection, e.g. a sequence gap. The connection stays open.
    Error { message: String },
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Frame::Hello { doc, peer, cursors } => {
                out.push(TAG_HELLO);
                put_str(&mut out, doc);
                put_str(&mut out, peer);
                put_u32(&mut out, cursors.len() as u32);
                for (peer, seq) in cursors {
                    put_str(&mut out, peer);
                    out.extend_from_slice(&seq.to_le_bytes());
                }
            }
            Frame::Delta { peer, seq, data } => {
                out.push(TAG_DELTA);
                put_str(&mut out, peer);
                out.extend_from_slice(&seq.to_le_bytes());
                put_bytes(&mut out, data);
            }
            Frame::Snapshot { frontier, data } => {
                out.push(TAG_SNAPSHOT);
                put_u32(&mut out, frontier.len() as u32);
                for (peer, seq) in frontier {
                    put_str(&mut out, peer);
                    out.extend_from_slice(&seq.to_le_bytes());
                }
                put_bytes(&mut out, data);
            }
            Frame::SnapshotBegin { frontier, size, hash } => {
                out.push(TAG_SNAPSHOT_BEGIN);
                put_u32(&mut out, frontier.len() as u32);
                for (peer, seq) in frontier {
                    put_str(&mut out, peer);
                    out.extend_from_slice(&seq.to_le_bytes());
                }
                out.extend_from_slice(&size.to_le_bytes());
                put_str(&mut out, hash);
            }
            Frame::SnapshotChunk { offset, data } => {
                out.push(TAG_SNAPSHOT_CHUNK);
                out.extend_from_slice(&offset.to_le_bytes());
                put_bytes(&mut out, data);
            }
            Frame::SnapshotEnd => out.push(TAG_SNAPSHOT_END),
            Frame::Ack { producer, seq } => {
                out.push(TAG_ACK);
                put_str(&mut out, producer);
                out.extend_from_slice(&seq.to_le_bytes());
            }
            Frame::Bye => out.push(TAG_BYE),
            Frame::CaughtUp { frontier } => {
                out.push(TAG_CAUGHT_UP);
                put_u32(&mut out, frontier.len() as u32);
                for (peer, seq) in frontier {
                    put_str(&mut out, peer);
                    out.extend_from_slice(&seq.to_le_bytes());
                }
            }
            Frame::Error { message } => {
                out.push(TAG_ERROR);
                put_str(&mut out, message);
            }
        }
        out
    }

    fn decode(body: &[u8]) -> io::Result<Frame> {
        let mut cur = Cursor { body, at: 0 };
        let tag = cur.u8()?;
        let frame = match tag {
            TAG_HELLO => Frame::Hello {
                doc: cur.string()?,
                peer: cur.string()?,
                cursors: cur.cursors()?,
            },
            TAG_DELTA => Frame::Delta {
                peer: cur.string()?,
                seq: cur.i64()?,
                data: cur.bytes()?.to_vec(),
            },
            TAG_ACK => Frame::Ack { producer: cur.string()?, seq: cur.i64()? },
            TAG_SNAPSHOT => Frame::Snapshot {
                frontier: cur.cursors()?,
                data: cur.bytes()?.to_vec(),
            },
            TAG_SNAPSHOT_BEGIN => Frame::SnapshotBegin {
                frontier: cur.cursors()?,
                size: cur.u64()?,
                hash: cur.string()?,
            },
            TAG_SNAPSHOT_CHUNK => Frame::SnapshotChunk {
                offset: cur.u64()?,
                data: cur.bytes()?.to_vec(),
            },
            TAG_SNAPSHOT_END => Frame::SnapshotEnd,
            TAG_BYE => Frame::Bye,
            TAG_CAUGHT_UP => Frame::CaughtUp { frontier: cur.cursors()? },
            TAG_ERROR => Frame::Error { message: cur.string()? },
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown frame tag {other}"),
                ));
            }
        };
        if cur.at != body.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing bytes after frame",
            ));
        }
        Ok(frame)
    }
}

/// Frontier and bytes returned after a chunked snapshot is validated.
pub type SnapshotParts = (Vec<(String, i64)>, Vec<u8>);

/// Collects the frames of a chunked snapshot and verifies the announced size,
/// contiguous offsets and blake3 hash before returning the bytes.
///
/// This is the receiving half of `SnapshotBegin`/`SnapshotChunk`/`SnapshotEnd`.
/// A client that misses the end marker, receives a duplicate offset, or sees a
/// final hash mismatch gets an error instead of importing a partial document.
pub struct SnapshotAssembler {
    frontier: Vec<(String, i64)>,
    size: usize,
    hash: String,
    data: Vec<u8>,
}

impl SnapshotAssembler {
    pub fn new(
        frontier: Vec<(String, i64)>,
        size: u64,
        hash: String,
    ) -> io::Result<SnapshotAssembler> {
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot has zero size",
            ));
        }
        let size = usize::try_from(size).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot size does not fit this platform",
            )
        })?;
        let Some(digest) = hash.strip_prefix("blake3:") else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot hash must be blake3:<hex>",
            ));
        };
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot hash must be 64 hex characters",
            ));
        }
        Ok(SnapshotAssembler {
            frontier,
            size,
            hash,
            data: Vec::with_capacity(size.min(SNAPSHOT_CHUNK_BYTES)),
        })
    }

    /// Append the next contiguous chunk.
    pub fn push(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot chunk is empty",
            ));
        }
        if data.len() > SNAPSHOT_CHUNK_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot chunk exceeds the chunk limit",
            ));
        }
        if offset != self.data.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "snapshot chunk starts at {offset}, expected {}",
                    self.data.len()
                ),
            ));
        }
        if self
            .data
            .len()
            .checked_add(data.len())
            .is_none_or(|end| end > self.size)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot chunks exceed the announced size",
            ));
        }
        self.data.extend_from_slice(data);
        Ok(())
    }

    /// Verify the size and hash and return the snapshot.
    pub fn finish(self) -> io::Result<SnapshotParts> {
        if self.data.len() != self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "snapshot ended at {} bytes, expected {}",
                    self.data.len(),
                    self.size
                ),
            ));
        }
        let actual = format!("blake3:{}", blake3::hash(&self.data).to_hex());
        if actual != self.hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("snapshot hashed to {actual}, expected {}", self.hash),
            ));
        }
        Ok((self.frontier, self.data))
    }
}

struct Cursor<'a> {
    body: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.body.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated frame"))?;
        let slice = &self.body[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn string(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?.to_vec())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    fn cursors(&mut self) -> io::Result<Vec<(String, i64)>> {
        let count = self.u32()? as usize;
        let mut cursors = Vec::with_capacity(count);
        for _ in 0..count {
            cursors.push((self.string()?, self.i64()?));
        }
        Ok(cursors)
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_bytes(out, value.as_bytes());
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value);
}

/// Write one length-prefixed frame and flush it.
pub fn write_frame(mut writer: impl Write, frame: &Frame) -> io::Result<()> {
    let body = frame.encode();
    if body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds the maximum size",
        ));
    }
    writer.write_all(&(body.len() as u32).to_le_bytes())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Read one frame. `Ok(None)` means the peer closed the connection cleanly.
pub fn read_frame(mut reader: impl Read) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds the maximum size",
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Frame::decode(&body).map(Some)
}

struct Conn {
    doc: String,
    peer: String,
    tx: mpsc::Sender<Frame>,
}

type Registry = Arc<Mutex<HashMap<u64, Conn>>>;

/// A bound coordinator socket. [`Server::run`] serves until the shutdown flag
/// is set; [`Server::shutdown_handle`] lets a test or host stop it.
pub struct Server {
    listener: UnixListener,
    state: Arc<Mutex<Workspace>>,
    conns: Registry,
    /// Every live connection's stop handle, so shutdown wakes blocked reads.
    stops: Arc<Mutex<HashMap<u64, UnixStream>>>,
    handles: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    next_id: AtomicU64,
    shutdown: Arc<AtomicBool>,
    socket: PathBuf,
    idle_compaction: Option<Duration>,
    last_activity: Arc<Mutex<Instant>>,
}

impl Server {
    /// Bind `socket`. An existing path is refused rather than removed: a live
    /// coordinator would otherwise be silently displaced.
    pub fn bind(workspace: Workspace, socket: &Path) -> Result<Server> {
        if socket.exists() {
            bail!(
                "{} already exists; remove it if no coordinator is running",
                socket.display()
            );
        }
        if let Some(parent) = socket.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory {}", parent.display()))?;
        }
        let listener = UnixListener::bind(socket)
            .with_context(|| format!("bind coordinator socket {}", socket.display()))?;
        listener.set_nonblocking(true)?;

        Ok(Server {
            listener,
            state: Arc::new(Mutex::new(workspace)),
            conns: Arc::new(Mutex::new(HashMap::new())),
            stops: Arc::new(Mutex::new(HashMap::new())),
            handles: Arc::new(Mutex::new(Vec::new())),
            next_id: AtomicU64::new(1),
            shutdown: Arc::new(AtomicBool::new(false)),
            socket: socket.to_path_buf(),
            idle_compaction: None,
            last_activity: Arc::new(Mutex::new(Instant::now())),
        })
    }

    /// Compact documents after this much write inactivity. Each pass stores a
    /// snapshot before deleting any acknowledged frames, so reconnecting
    /// clients still get the document state. `Duration::ZERO` disables it.
    pub fn compact_when_idle(mut self, idle: Duration) -> Server {
        self.idle_compaction = (!idle.is_zero()).then_some(idle);
        self
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn shutdown_handle(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// Accept connections until shutdown. Each connection gets a reader thread
    /// and a writer thread, so a slow client cannot stall the relay.
    pub fn run(self) -> Result<()> {
        while !self.shutdown.load(Ordering::Relaxed) {
            self.compact_if_idle();
            match self.listener.accept() {
                Ok((stream, _)) => self.spawn(stream),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err).context("accept coordinator connection"),
            }
        }
        drop(self.listener);

        // Stop accepting, wake every blocked connection read, and wait for the
        // connection threads to drop their workspace handles. Without this a
        // restarted coordinator could not reopen the same data directory while
        // an idle connection still held the volume lock.
        {
            let stops = self.stops.lock().expect("stop registry lock poisoned");
            for stream in stops.values() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        let handles: Vec<_> = self
            .handles
            .lock()
            .expect("handle registry lock poisoned")
            .drain(..)
            .collect();
        for handle in handles {
            let _ = handle.join();
        }

        let _ = std::fs::remove_file(&self.socket);
        Ok(())
    }

    fn compact_if_idle(&self) {
        let Some(idle) = self.idle_compaction else {
            return;
        };
        let elapsed = self
            .last_activity
            .lock()
            .expect("activity lock poisoned")
            .elapsed();
        if elapsed < idle {
            return;
        }
        let report = self
            .state
            .lock()
            .expect("workspace lock poisoned")
            .compact_idle();
        *self.last_activity.lock().expect("activity lock poisoned") = Instant::now();
        match report {
            Ok(report) if report.docs > 0 => eprintln!(
                "workspace: idle compaction: {} doc(s), {} snapshot(s), {} producer(s), {} frame(s) deleted, {} blocked, {} failed",
                report.docs,
                report.snapshots,
                report.compacted,
                report.frames_deleted,
                report.blocked,
                report.failed
            ),
            Ok(_) => {}
            Err(err) => eprintln!("workspace: idle compaction: {err:#}"),
        }
    }

    fn spawn(&self, stream: UnixStream) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let state = self.state.clone();
        let conns = self.conns.clone();
        let activity = self.last_activity.clone();
        let stops = self.stops.clone();
        let Ok(stop) = stream.try_clone() else {
            return;
        };
        stops
            .lock()
            .expect("stop registry lock poisoned")
            .insert(id, stop);
        let handle = thread::spawn(move || {
            if let Err(err) = handle_connection(id, stream, state, conns, activity) {
                eprintln!("workspace: coordinator connection {id}: {err:#}");
            }
            stops
                .lock()
                .expect("stop registry lock poisoned")
                .remove(&id);
        });
        self.handles
            .lock()
            .expect("handle registry lock poisoned")
            .push(handle);
    }
}

fn handle_connection(
    id: u64,
    stream: UnixStream,
    state: Arc<Mutex<Workspace>>,
    conns: Registry,
    activity: Arc<Mutex<Instant>>,
) -> Result<()> {
    let mut reader = stream.try_clone()?;
    let (tx, rx) = mpsc::channel::<Frame>();
    thread::spawn(move || {
        let mut writer = stream;
        while let Ok(frame) = rx.recv() {
            if write_frame(&mut writer, &frame).is_err() {
                break;
            }
        }
    });

    let hello = match read_frame(&mut reader)? {
        Some(Frame::Hello { doc, peer, cursors }) => {
            if doc.is_empty() || peer.is_empty() {
                let _ = tx.send(Frame::Error {
                    message: "hello needs a document and a peer".to_string(),
                });
                return Ok(());
            }
            (doc, peer, cursors)
        }
        Some(_) => {
            let _ = tx.send(Frame::Error {
                message: "first frame must be Hello".to_string(),
            });
            return Ok(());
        }
        None => return Ok(()),
    };
    let (doc, peer, cursors) = hello;

    // Join and register while holding the workspace lock, so a delta committed
    // by another connection cannot be broadcast both in the catch-up batch and
    // again as a live frame.
    let batch = {
        let workspace = state.lock().expect("workspace lock poisoned");
        let batch = match workspace.join(&doc, &cursors) {
            Ok(batch) => batch,
            Err(err) => {
                let _ = tx.send(Frame::Error { message: err.to_string() });
                return Ok(());
            }
        };
        conns.lock().expect("registry lock poisoned").insert(
            id,
            Conn {
                doc: doc.clone(),
                peer: peer.clone(),
                tx: tx.clone(),
            },
        );
        batch
    };

    if let Some(snapshot) = batch.snapshot
        && !send_snapshot(&tx, snapshot.frontier, snapshot.data)
    {
        remove_conn(&conns, id);
        return Ok(());
    }

    for frame in batch.frames {
        let frame = Frame::Delta {
            peer: frame.peer,
            seq: frame.seq,
            data: frame.data,
        };
        if let Frame::Delta { peer: from, .. } = &frame
            && *from == peer
        {
            continue;
        }
        if tx.send(frame).is_err() {
            remove_conn(&conns, id);
            return Ok(());
        }
    }
    if tx
        .send(Frame::CaughtUp { frontier: batch.frontier })
        .is_err()
    {
        remove_conn(&conns, id);
        return Ok(());
    }

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                let _ = tx.send(Frame::Error { message: err.to_string() });
                break;
            }
            Err(err) => {
                remove_conn(&conns, id);
                return Err(err.into());
            }
        };

        match frame {
            Frame::Delta { peer: from, seq, data } => {
                if from != peer {
                    let _ = tx.send(Frame::Error {
                        message: format!("delta from {from:?}, expected {peer:?}"),
                    });
                    continue;
                }
                let workspace = state.lock().expect("workspace lock poisoned");
                match workspace.submit(&doc, &peer, seq, &data) {
                    Ok(crate::AppendOutcome::Applied { .. }) => {
                        *activity.lock().expect("activity lock poisoned") = Instant::now();
                        broadcast(
                            &conns,
                            &doc,
                            id,
                            &peer,
                            Frame::Delta { peer: peer.clone(), seq, data },
                        );
                    }
                    Ok(crate::AppendOutcome::Duplicate { .. }) => {}
                    Ok(crate::AppendOutcome::Gap { expected, .. }) => {
                        let _ = tx.send(Frame::Error {
                            message: format!("gap: expected seq {expected}"),
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(Frame::Error { message: err.to_string() });
                    }
                }
            }
            Frame::Ack { producer, seq } => {
                let workspace = state.lock().expect("workspace lock poisoned");
                if let Err(err) = workspace.ack(&doc, &producer, &peer, seq) {
                    let _ = tx.send(Frame::Error { message: err.to_string() });
                } else {
                    *activity.lock().expect("activity lock poisoned") = Instant::now();
                }
            }
            Frame::Bye => break,
            Frame::Hello { .. } => {
                let _ = tx.send(Frame::Error { message: "already joined".to_string() });
            }
            Frame::CaughtUp { .. }
            | Frame::Error { .. }
            | Frame::Snapshot { .. }
            | Frame::SnapshotBegin { .. }
            | Frame::SnapshotChunk { .. }
            | Frame::SnapshotEnd => {
                let _ = tx.send(Frame::Error {
                    message: "unexpected server frame from client".to_string(),
                });
            }
        }
    }

    remove_conn(&conns, id);
    Ok(())
}

/// Send a compacted snapshot whole when it fits one frame, otherwise as a
/// begin/chunk/end sequence. Each chunk is bounded independently, so a large
/// document never needs one allocation the frame limit would reject.
fn send_snapshot(tx: &mpsc::Sender<Frame>, frontier: Vec<(String, i64)>, data: Vec<u8>) -> bool {
    if data.len() + 64 * 1024 <= MAX_FRAME_BYTES {
        return tx.send(Frame::Snapshot { frontier, data }).is_ok();
    }

    let hash = format!("blake3:{}", blake3::hash(&data).to_hex());
    if tx
        .send(Frame::SnapshotBegin { frontier, size: data.len() as u64, hash })
        .is_err()
    {
        return false;
    }
    for (index, chunk) in data.chunks(SNAPSHOT_CHUNK_BYTES).enumerate() {
        let offset = (index * SNAPSHOT_CHUNK_BYTES) as u64;
        if tx
            .send(Frame::SnapshotChunk { offset, data: chunk.to_vec() })
            .is_err()
        {
            return false;
        }
    }
    tx.send(Frame::SnapshotEnd).is_ok()
}

fn remove_conn(conns: &Registry, id: u64) {
    conns.lock().expect("registry lock poisoned").remove(&id);
}

fn broadcast(conns: &Registry, doc: &str, from_id: u64, from_peer: &str, frame: Frame) {
    let conns = conns.lock().expect("registry lock poisoned");
    for (id, conn) in conns.iter() {
        if *id == from_id || conn.doc != doc || conn.peer == from_peer {
            continue;
        }
        let _ = conn.tx.send(frame.clone());
    }
}
