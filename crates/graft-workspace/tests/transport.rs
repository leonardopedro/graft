//! The coordinator relay over a real Unix socket: join, catch up, broadcast,
//! reconnect from cursors, and gap reporting.

use std::{
    fs,
    os::unix::net::UnixStream,
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

use graft::remote::RemoteConfig;
use graft_workspace::{
    Workspace, WorkspaceConfig,
    transport::{
        Frame, MAX_FRAME_BYTES, SNAPSHOT_CHUNK_BYTES, Server, SnapshotAssembler, read_frame,
        write_frame,
    },
};
use loro::{ExportMode, LoroDoc};

fn config(dir: &Path) -> WorkspaceConfig {
    WorkspaceConfig {
        remote: RemoteConfig::Fs {
            root: dir.join("remote").display().to_string(),
        },
        data_dir: dir.join("data"),
        autosync: None,
        catalog_tag: "ws-catalog".to_string(),
        media: None,
        archive: None,
        backup: None,
    }
}

fn recv(stream: &mut UnixStream) -> Frame {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    read_frame(&mut *stream)
        .expect("read frame")
        .expect("connection closed")
}

fn send(stream: &mut UnixStream, frame: Frame) {
    write_frame(&mut *stream, &frame).expect("write frame");
}

fn join(socket: &Path, peer: &str, cursors: &[(&str, i64)]) -> UnixStream {
    let mut stream = UnixStream::connect(socket).unwrap();
    send(
        &mut stream,
        Frame::Hello {
            doc: "notes-plan".to_string(),
            peer: peer.to_string(),
            cursors: cursors
                .iter()
                .map(|(peer, seq)| (peer.to_string(), *seq))
                .collect(),
        },
    );
    stream
}

/// Read until `CaughtUp`, returning the deltas that preceded it.
fn caught_up(stream: &mut UnixStream) -> (Vec<Frame>, Vec<(String, i64)>) {
    let mut frames = Vec::new();
    loop {
        match recv(stream) {
            Frame::CaughtUp { frontier } => return (frames, frontier),
            Frame::Error { message } => panic!("coordinator error: {message}"),
            frame => frames.push(frame),
        }
    }
}

fn delta(text: &str) -> Vec<u8> {
    let doc = LoroDoc::new();
    doc.get_text("text").insert(0, text).unwrap();
    doc.export(ExportMode::all_updates()).unwrap()
}

struct Coordinator(Child);

impl Drop for Coordinator {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Coordinator {
    fn spawn(dir: &Path, compact_after_secs: u64) -> Coordinator {
        let child = Command::new(env!("CARGO_BIN_EXE_workspace"))
            .current_dir(dir)
            .args([
                "--data",
                "data",
                "--remote",
                "fs:remote",
                "serve",
                "--socket",
                "coordinator.sock",
                "--compact-after-secs",
                &compact_after_secs.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn coordinator");
        let mut coordinator = Coordinator(child);
        let socket = dir.join("coordinator.sock");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            if let Some(status) = coordinator.0.try_wait().unwrap() {
                panic!("coordinator exited before binding: {status}");
            }
            assert!(Instant::now() < deadline, "coordinator did not bind");
            thread::sleep(Duration::from_millis(25));
        }
        coordinator
    }

    fn kill(mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

#[test]
fn idle_compaction_serves_a_snapshot_to_a_fresh_peer() {
    let dir = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::open(&config(dir.path())).unwrap();
    workspace.init().unwrap();

    let socket = dir.path().join("coordinator.sock");
    let server = Server::bind(workspace, &socket)
        .unwrap()
        .compact_when_idle(Duration::from_millis(100));
    let shutdown = server.shutdown_handle();
    let running = std::thread::spawn(move || server.run().unwrap());

    let mut a = join(&socket, "a", &[]);
    assert!(caught_up(&mut a).0.is_empty());
    let payload = delta("hello");
    send(
        &mut a,
        Frame::Delta { peer: "a".into(), seq: 1, data: payload },
    );

    // No consumer has acked, but the snapshot makes pruning safe for a fresh
    // client: it receives the whole state instead of a truncated frame tail.
    std::thread::sleep(Duration::from_millis(600));

    let mut fresh = join(&socket, "fresh", &[]);
    let (frames, frontier) = caught_up(&mut fresh);
    assert_eq!(frontier, vec![("a".to_string(), 1)]);
    assert_eq!(frames.len(), 1, "{frames:?}");
    let snapshot = match &frames[0] {
        Frame::Snapshot { frontier, data } => {
            assert_eq!(frontier, &vec![("a".to_string(), 1)]);
            data
        }
        frame => panic!("expected snapshot, got {frame:?}"),
    };
    let restored = LoroDoc::new();
    restored.import(snapshot).unwrap();
    assert_eq!(restored.get_text("text").to_string(), "hello");

    drop(fresh);
    drop(a);
    shutdown.store(true, Ordering::Relaxed);
    running.join().unwrap();
}

#[test]
fn snapshot_assembler_rejects_malformed_sequences() {
    let payload = b"abcdef";
    let hash = format!("blake3:{}", blake3::hash(payload).to_hex());

    let mut assembler = SnapshotAssembler::new(
        vec![("a".to_string(), 1)],
        payload.len() as u64,
        hash.clone(),
    )
    .unwrap();
    assert!(
        assembler.push(1, b"a").is_err(),
        "a chunk must start at the current offset"
    );
    assembler.push(0, b"abc").unwrap();
    assembler.push(3, b"def").unwrap();
    let (frontier, assembled) = assembler.finish().unwrap();
    assert_eq!(frontier, vec![("a".to_string(), 1)]);
    assert_eq!(assembled, payload);

    let mut short = SnapshotAssembler::new(Vec::new(), payload.len() as u64, hash.clone()).unwrap();
    short.push(0, b"abc").unwrap();
    assert!(short.finish().is_err(), "a missing tail is rejected");

    let mut oversized = SnapshotAssembler::new(Vec::new(), 3, hash.clone()).unwrap();
    assert!(
        oversized.push(0, b"abcd").is_err(),
        "a chunk may not exceed the announced size"
    );

    let mut corrupt = SnapshotAssembler::new(Vec::new(), payload.len() as u64, hash).unwrap();
    corrupt.push(0, b"abcde").unwrap();
    corrupt.push(5, b"g").unwrap();
    assert!(corrupt.finish().is_err(), "a hash mismatch is rejected");

    assert!(SnapshotAssembler::new(Vec::new(), 0, "blake3:00".to_string()).is_err());
    assert!(
        SnapshotAssembler::new(Vec::new(), 1, "md5:00".to_string()).is_err(),
        "only blake3 snapshot hashes are accepted"
    );
}

#[test]
fn chunked_snapshot_transfer_reassembles_large_documents() {
    let dir = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::open(&config(dir.path())).unwrap();
    workspace.init().unwrap();

    // Larger than one frame, with a size that is not a chunk multiple so the
    // final short chunk is exercised too.
    let payload: Vec<u8> = (0..MAX_FRAME_BYTES + SNAPSHOT_CHUNK_BYTES + 17)
        .map(|index| (index % 251) as u8)
        .collect();
    workspace
        .oplog()
        .put_snapshot("large", &[("a".to_string(), 1)], &payload)
        .unwrap();

    let socket = dir.path().join("coordinator.sock");
    let server = Server::bind(workspace, &socket).unwrap();
    let shutdown = server.shutdown_handle();
    let running = std::thread::spawn(move || server.run().unwrap());

    let mut stream = UnixStream::connect(&socket).unwrap();
    send(
        &mut stream,
        Frame::Hello {
            doc: "large".to_string(),
            peer: "client".to_string(),
            cursors: Vec::new(),
        },
    );

    let mut assembler: Option<SnapshotAssembler> = None;
    loop {
        match recv(&mut stream) {
            Frame::SnapshotBegin { frontier, size, hash } => {
                assert_eq!(frontier, vec![("a".to_string(), 1)]);
                assert_eq!(size, payload.len() as u64);
                assembler = Some(SnapshotAssembler::new(frontier, size, hash).unwrap());
            }
            Frame::SnapshotChunk { offset, data } => {
                assert!(data.len() <= SNAPSHOT_CHUNK_BYTES);
                assembler.as_mut().unwrap().push(offset, &data).unwrap();
            }
            Frame::SnapshotEnd => {}
            Frame::Snapshot { .. } => {
                panic!("an oversized snapshot must not use one frame")
            }
            Frame::CaughtUp { frontier } => {
                assert_eq!(frontier, vec![("a".to_string(), 1)]);
                break;
            }
            Frame::Error { message } => panic!("coordinator error: {message}"),
            frame => panic!("unexpected frame before catch-up: {frame:?}"),
        }
    }
    let (frontier, assembled) = assembler.unwrap().finish().unwrap();
    assert_eq!(frontier, vec![("a".to_string(), 1)]);
    assert_eq!(assembled, payload);

    drop(stream);
    shutdown.store(true, Ordering::Relaxed);
    running.join().unwrap();
}

#[test]
fn interrupted_chunked_snapshot_restarts_from_begin_on_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::open(&config(dir.path())).unwrap();
    workspace.init().unwrap();
    let payload: Vec<u8> = (0..MAX_FRAME_BYTES + SNAPSHOT_CHUNK_BYTES + 17)
        .map(|index| (index % 239) as u8)
        .collect();
    workspace
        .oplog()
        .put_snapshot("large", &[("a".to_string(), 1)], &payload)
        .unwrap();

    let socket = dir.path().join("coordinator.sock");
    let server = Server::bind(workspace, &socket).unwrap();
    let shutdown = server.shutdown_handle();
    let running = std::thread::spawn(move || server.run().unwrap());

    let mut first = UnixStream::connect(&socket).unwrap();
    send(
        &mut first,
        Frame::Hello {
            doc: "large".to_string(),
            peer: "client".to_string(),
            cursors: Vec::new(),
        },
    );

    // Take the begin frame and two chunks, then disappear mid-transfer.
    let mut partial: Option<SnapshotAssembler> = None;
    let mut chunks = 0;
    while chunks < 2 {
        match recv(&mut first) {
            Frame::SnapshotBegin { frontier, size, hash } => {
                partial = Some(SnapshotAssembler::new(frontier, size, hash).unwrap());
            }
            Frame::SnapshotChunk { offset, data } => {
                partial.as_mut().unwrap().push(offset, &data).unwrap();
                chunks += 1;
            }
            frame => panic!("unexpected frame before the interruption: {frame:?}"),
        }
    }
    assert!(partial.is_some(), "the stream must have started");
    drop(partial);
    drop(first);

    // A restarted client has no partial state to resume from: it joins again
    // from empty cursors and must receive a fresh begin/chunk/end stream.
    let mut second = UnixStream::connect(&socket).unwrap();
    send(
        &mut second,
        Frame::Hello {
            doc: "large".to_string(),
            peer: "client".to_string(),
            cursors: Vec::new(),
        },
    );

    let mut assembler: Option<SnapshotAssembler> = None;
    loop {
        match recv(&mut second) {
            Frame::SnapshotBegin { frontier, size, hash } => {
                assembler = Some(SnapshotAssembler::new(frontier, size, hash).unwrap());
            }
            Frame::SnapshotChunk { offset, data } => {
                assembler.as_mut().unwrap().push(offset, &data).unwrap();
            }
            Frame::SnapshotEnd => {}
            Frame::CaughtUp { frontier } => {
                assert_eq!(frontier, vec![("a".to_string(), 1)]);
                break;
            }
            Frame::Error { message } => panic!("coordinator error: {message}"),
            frame => panic!("unexpected frame after restart: {frame:?}"),
        }
    }
    let (frontier, assembled) = assembler.unwrap().finish().unwrap();
    assert_eq!(frontier, vec![("a".to_string(), 1)]);
    assert_eq!(assembled, payload);

    drop(second);
    shutdown.store(true, Ordering::Relaxed);
    running.join().unwrap();
}

fn large_delta(seed: usize) -> Vec<u8> {
    let doc = LoroDoc::new();
    let mut state = seed as u64 + 1;
    let text: String = (0..1 << 20)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'!' + (state % 94) as u8)
        })
        .collect();
    doc.get_text("text").insert(0, &text).unwrap();
    doc.export(ExportMode::all_updates()).unwrap()
}

#[test]
fn process_restart_resumes_a_chunked_snapshot_from_begin() {
    let dir = tempfile::tempdir().unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_workspace"))
        .current_dir(dir.path())
        .args(["--data", "data", "--remote", "fs:remote", "init"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let socket = dir.path().join("coordinator.sock");
    let first = Coordinator::spawn(dir.path(), 1);
    let mut stream = UnixStream::connect(&socket).unwrap();
    send(
        &mut stream,
        Frame::Hello {
            doc: "large".to_string(),
            peer: "a".to_string(),
            cursors: Vec::new(),
        },
    );
    assert!(caught_up(&mut stream).0.is_empty());
    for seq in 1..=24i64 {
        send(
            &mut stream,
            Frame::Delta {
                peer: "a".to_string(),
                seq,
                data: large_delta(seq as usize),
            },
        );
    }

    // Let idle compaction materialize a snapshot larger than one frame. Keep
    // the producer connected so the server has time to read the last frame.
    thread::sleep(Duration::from_secs(3));

    let mut interrupted = UnixStream::connect(&socket).unwrap();
    send(
        &mut interrupted,
        Frame::Hello {
            doc: "large".to_string(),
            peer: "client".to_string(),
            cursors: Vec::new(),
        },
    );
    let mut chunks = 0;
    while chunks < 2 {
        match recv(&mut interrupted) {
            Frame::SnapshotBegin { frontier, .. } => {
                assert_eq!(frontier, vec![("a".to_string(), 24)]);
            }
            Frame::SnapshotChunk { .. } => chunks += 1,
            Frame::Snapshot { data, .. } => panic!(
                "expected a chunked snapshot, got one frame with {} bytes",
                data.len()
            ),
            frame => panic!("expected a chunked snapshot, got {frame:?}"),
        }
    }
    drop(interrupted);
    drop(stream);

    // Kill the coordinator while the transfer is incomplete. A supervisor
    // restart must reopen the same data directory and start the snapshot again.
    first.kill();
    fs::remove_file(&socket).expect("kill leaves the socket inode behind");
    let _second = Coordinator::spawn(dir.path(), 0);

    let mut stream = UnixStream::connect(&socket).unwrap();
    send(
        &mut stream,
        Frame::Hello {
            doc: "large".to_string(),
            peer: "client".to_string(),
            cursors: Vec::new(),
        },
    );

    let mut assembler: Option<SnapshotAssembler> = None;
    loop {
        match recv(&mut stream) {
            Frame::SnapshotBegin { frontier, size, hash } => {
                assembler = Some(SnapshotAssembler::new(frontier, size, hash).unwrap());
            }
            Frame::SnapshotChunk { offset, data } => {
                assembler.as_mut().unwrap().push(offset, &data).unwrap();
            }
            Frame::SnapshotEnd => {}
            Frame::CaughtUp { frontier } => {
                assert_eq!(frontier, vec![("a".to_string(), 24)]);
                break;
            }
            Frame::Error { message } => panic!("coordinator error: {message}"),
            frame => panic!("unexpected frame after process restart: {frame:?}"),
        }
    }
    let (frontier, assembled) = assembler.unwrap().finish().unwrap();
    assert_eq!(frontier, vec![("a".to_string(), 24)]);
    let restored = LoroDoc::new();
    restored.import(&assembled).unwrap();
    assert_eq!(restored.get_text("text").to_string().len(), 24 * (1 << 20));
}

#[test]
fn relay_catches_up_and_broadcasts() {
    let dir = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::open(&config(dir.path())).unwrap();
    workspace.init().unwrap();

    let socket = dir.path().join("coordinator.sock");
    let server = Server::bind(workspace, &socket).unwrap();
    let shutdown = server.shutdown_handle();
    let running = std::thread::spawn(move || server.run().unwrap());

    let mut a = join(&socket, "a", &[]);
    assert!(caught_up(&mut a).0.is_empty());
    let mut b = join(&socket, "b", &[]);
    assert!(caught_up(&mut b).0.is_empty());

    send(
        &mut a,
        Frame::Delta {
            peer: "a".into(),
            seq: 1,
            data: b"one".to_vec(),
        },
    );
    assert_eq!(
        recv(&mut b),
        Frame::Delta {
            peer: "a".into(),
            seq: 1,
            data: b"one".to_vec()
        },
        "a committed frame is relayed to the other peer"
    );

    send(
        &mut a,
        Frame::Delta {
            peer: "a".into(),
            seq: 2,
            data: b"two".to_vec(),
        },
    );
    assert_eq!(
        recv(&mut b),
        Frame::Delta {
            peer: "a".into(),
            seq: 2,
            data: b"two".to_vec()
        }
    );
    send(&mut b, Frame::Ack { producer: "a".into(), seq: 2 });

    send(
        &mut a,
        Frame::Delta {
            peer: "a".into(),
            seq: 3,
            data: b"three".to_vec(),
        },
    );
    // A fresh peer proves the frame is committed, so the reconnect below
    // cannot race the commit.
    let mut watcher = join(&socket, "watcher", &[("a", 2)]);
    let mut frames = Vec::new();
    let frontier = loop {
        match recv(&mut watcher) {
            Frame::CaughtUp { frontier } => {
                // The frame may arrive before or after the catch-up marker.
                if frames
                    .iter()
                    .any(|frame| matches!(frame, Frame::Delta { seq: 3, .. }))
                {
                    break frontier;
                }
            }
            frame => frames.push(frame),
        }
    };
    assert_eq!(frontier, vec![("a".to_string(), 3)]);
    assert_eq!(
        frames,
        vec![Frame::Delta {
            peer: "a".into(),
            seq: 3,
            data: b"three".to_vec()
        }]
    );
    drop(watcher);

    // Reconnecting with the last consumed cursor replays only the tail.
    let mut again = join(&socket, "b", &[("a", 2)]);
    let (frames, frontier) = caught_up(&mut again);
    assert_eq!(
        frames,
        vec![Frame::Delta {
            peer: "a".into(),
            seq: 3,
            data: b"three".to_vec()
        }]
    );
    assert_eq!(frontier, vec![("a".to_string(), 3)]);

    // A gap is reported without closing the connection.
    send(
        &mut again,
        Frame::Delta {
            peer: "b".into(),
            seq: 5,
            data: b"".to_vec(),
        },
    );
    match recv(&mut again) {
        Frame::Error { message } => assert!(message.contains("expected seq 1"), "{message}"),
        frame => panic!("expected gap error, got {frame:?}"),
    }

    drop(a);
    drop(b);
    drop(again);
    shutdown.store(true, Ordering::Relaxed);
    running.join().unwrap();
    assert!(
        !socket.exists(),
        "the coordinator removes its socket on shutdown"
    );
}
