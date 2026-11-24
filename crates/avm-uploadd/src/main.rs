//! Authenticated staging cache for `avm-pkg push`.
//!
//! The australVM client speaks plain HTTP and has no authentication-header
//! hook, so the bearer token is a path prefix:
//!
//! ```text
//! http://127.0.0.1:18090/<64-hex-token>
//! ```
//!
//! Endpoints match `avm-cas::remote`:
//!
//! ```text
//! HEAD   /<token>/chunks/<hex>              present?
//! POST   /<token>/chunks/missing            {"hashes":[]} -> {"missing":[]}
//! PUT    /<token>/chunks/<hex>              BLAKE3-verified bytes
//! GET    /<token>/chunks/<hex>
//! HEAD   /<token>/manifests/<hex>
//! PUT    /<token>/manifests/<hex>
//! GET    /<token>/manifests/<hex>
//! ```
//!
//! The cache deliberately does not parse manifests or write the host avm-pkg
//! store. Chunks are verified against the requested BLAKE3 digest; manifests
//! are staged raw. A host importer then runs `avm-pkg pull`, whose client
//! re-verifies the manifest identity and every chunk before the host store
//! accepts them. A malicious upload can waste cache space, but cannot poison
//! the shared store.
//!
//! `/health` is unauthenticated and returns `ok`.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_MISSING_BODY: usize = 1024 * 1024;
const DEFAULT_MAX_BODY: u64 = 64 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(30);

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct Config {
    token: String,
    root: PathBuf,
    max_body: u64,
}

struct Request {
    method: String,
    target: String,
    body: Vec<u8>,
}

struct ManifestInfo {
    identity: [u8; 32],
    pubkey: Option<[u8; 32]>,
    signature: Option<[u8; 64]>,
}

struct ManifestReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

struct RequestError {
    status: u16,
    message: String,
}

impl RequestError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("verify-manifest") {
        if let Err(message) = verify_manifest_cli(&args[1..]) {
            eprintln!("avm-uploadd: verify-manifest: {message}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(String::as_str) == Some("signer-pubkey") {
        if let Err(message) = signer_pubkey_cli(&args[1..]) {
            eprintln!("avm-uploadd: signer-pubkey: {message}");
            std::process::exit(1);
        }
        return;
    }

    let config = match Config::from_args(&args) {
        Ok(config) => Arc::new(config),
        Err(message) => {
            eprintln!("avm-uploadd: {message}");
            std::process::exit(2);
        }
    };

    if let Err(error) = fs::create_dir_all(config.root.join("chunks"))
        .and_then(|()| fs::create_dir_all(config.root.join("manifests")))
        .and_then(|()| fs::create_dir_all(config.root.join("imported")))
    {
        eprintln!(
            "avm-uploadd: cannot create {}: {error}",
            config.root.display()
        );
        std::process::exit(1);
    }

    let bind = std::env::var("AVM_UPLOAD_BIND").unwrap_or_else(|_| "127.0.0.1:18090".into());
    let listener = match TcpListener::bind(&bind) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("avm-uploadd: cannot bind {bind}: {error}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "avm-uploadd: listening on http://{bind}/<token> root={}",
        config.root.display()
    );
    serve(listener, config);
}

impl Config {
    fn from_args(args: &[String]) -> Result<Self, String> {
        let state = state_root();
        let mut root = std::env::var_os("AVM_UPLOAD_ROOT")
            .map_or_else(|| state.join("upload-cache"), PathBuf::from);
        let mut token_file = std::env::var_os("AVM_UPLOAD_TOKEN_FILE")
            .map_or_else(|| state.join("upload.token"), PathBuf::from);
        let mut max_body = DEFAULT_MAX_BODY;

        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--root" => {
                    root = args
                        .next()
                        .map(PathBuf::from)
                        .ok_or_else(|| "--root needs a directory".to_string())?;
                }
                "--token-file" => {
                    token_file = args
                        .next()
                        .map(PathBuf::from)
                        .ok_or_else(|| "--token-file needs a file".to_string())?;
                }
                "--max-body" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--max-body needs a byte count".to_string())?;
                    max_body = value
                        .parse()
                        .map_err(|_| format!("bad --max-body `{value}`"))?;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: avm-uploadd [--root DIR] [--token-file FILE] [--max-body BYTES]\n\
                         env: AVM_UPLOAD_BIND (default 127.0.0.1:18090)"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }

        let token = fs::read_to_string(&token_file)
            .map_err(|error| format!("cannot read token file {}: {error}", token_file.display()))?;
        let token = token.trim().to_string();
        if token.len() < 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "token in {} must be at least 32 hex characters",
                token_file.display()
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = fs::metadata(&token_file) {
                let mode = meta.permissions().mode() & 0o077;
                if mode != 0 {
                    eprintln!(
                        "avm-uploadd: warning: {} is accessible to group/others (mode {:o})",
                        token_file.display(),
                        meta.permissions().mode() & 0o777
                    );
                }
            }
        }

        Ok(Self { token, root, max_body })
    }
}

fn state_root() -> PathBuf {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(value).join("graft/avm")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/state/graft/avm")
    } else {
        PathBuf::from(".graft/avm")
    }
}

fn parse_manifest(bytes: &[u8]) -> Result<ManifestInfo, String> {
    let mut reader = ManifestReader { bytes, pos: 0 };
    if reader.take(4)? != b"AVMM".as_slice() {
        return Err("manifest magic mismatch".into());
    }
    let format = reader.u16()?;
    if format != 1 && format != 2 {
        return Err(format!("manifest format {format} unsupported"));
    }
    let _name = reader.string()?;
    let _version = reader.string()?;
    let _total_size = reader.u64()?;
    match reader.u8()? {
        0 => {}
        1 => {
            reader.take(32)?;
        }
        _ => return Err("invalid drv_hash presence byte".into()),
    }
    if format >= 2 {
        match reader.u8()? {
            0 => {}
            1 => {
                reader.take(32)?;
            }
            _ => return Err("invalid closure presence byte".into()),
        }
    }
    let chunks = reader.u32()? as usize;
    if chunks > 1 << 24 {
        return Err("manifest chunk count implausible".into());
    }
    let chunk_bytes = chunks
        .checked_mul(32)
        .ok_or_else(|| "manifest chunk list is too large".to_string())?;
    reader.take(chunk_bytes)?;
    let unsigned_end = reader.pos;

    let pubkey = match reader.u8()? {
        0 => None,
        1 => Some(reader.array32()?),
        _ => return Err("invalid pubkey presence byte".into()),
    };
    let signature = match reader.u8()? {
        0 => None,
        1 => Some(reader.array64()?),
        _ => return Err("invalid signature presence byte".into()),
    };
    if reader.pos != bytes.len() {
        return Err("trailing bytes after manifest".into());
    }

    Ok(ManifestInfo {
        identity: *blake3::hash(&bytes[..unsigned_end]).as_bytes(),
        pubkey,
        signature,
    })
}

impl<'a> ManifestReader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(count)
            .ok_or_else(|| "manifest length overflow".to_string())?;
        if end > self.bytes.len() {
            return Err("truncated manifest".into());
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn string(&mut self) -> Result<String, String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| "manifest string is not UTF-8".to_string())
    }

    fn array32(&mut self) -> Result<[u8; 32], String> {
        let bytes = self.take(32)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn array64(&mut self) -> Result<[u8; 64], String> {
        let bytes = self.take(64)?;
        let mut out = [0u8; 64];
        out.copy_from_slice(bytes);
        Ok(out)
    }
}

fn verify_manifest_bytes(
    bytes: &[u8],
    expected_id: &[u8; 32],
    allowed: &[[u8; 32]],
) -> Result<[u8; 32], String> {
    let info = parse_manifest(bytes)?;
    if info.identity != *expected_id {
        return Err(format!(
            "manifest identity {} does not match requested id {}",
            hex(&info.identity),
            hex(expected_id)
        ));
    }
    let signature = info
        .signature
        .ok_or_else(|| "manifest is unsigned".to_string())?;
    let pubkey = info
        .pubkey
        .ok_or_else(|| "manifest carries no public key".to_string())?;
    if !allowed.contains(&pubkey) {
        return Err(format!("signer {} is not allowed", hex(&pubkey)));
    }
    let verifying_key = VerifyingKey::from_bytes(&pubkey)
        .map_err(|error| format!("invalid public key: {error}"))?;
    let signature = Signature::from_bytes(&signature);
    verifying_key
        .verify(&info.identity, &signature)
        .map_err(|error| format!("signature mismatch: {error}"))?;
    Ok(pubkey)
}

fn verify_manifest_cli(args: &[String]) -> Result<(), String> {
    let mut manifest = None;
    let mut id = None;
    let mut allowed = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--manifest" => {
                manifest = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--manifest needs a file".to_string())?
                        .clone(),
                );
                index += 2;
            }
            "--id" => {
                id = Some(parse_hex32(
                    args.get(index + 1)
                        .ok_or_else(|| "--id needs a manifest id".to_string())?,
                )?);
                index += 2;
            }
            "--allow" => {
                allowed.push(parse_hex32(
                    args.get(index + 1)
                        .ok_or_else(|| "--allow needs a signer key".to_string())?,
                )?);
                index += 2;
            }
            "--help" | "-h" => {
                println!(
                    "usage: avm-uploadd verify-manifest --id HEX --manifest FILE \\\n                     --allow SIGNER_HEX [--allow SIGNER_HEX...]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let manifest = manifest.ok_or_else(|| "--manifest is required".to_string())?;
    let id = id.ok_or_else(|| "--id is required".to_string())?;
    if allowed.is_empty() {
        return Err("at least one --allow signer is required".into());
    }
    let bytes = fs::read(&manifest).map_err(|error| format!("cannot read {manifest}: {error}"))?;
    let signer = verify_manifest_bytes(&bytes, &id, &allowed)?;
    println!("verified {} signed-by {}", hex(&id), hex(&signer));
    Ok(())
}

fn signer_pubkey_cli(args: &[String]) -> Result<(), String> {
    let mut seed = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--seed" => {
                seed = Some(parse_hex32(
                    args.get(index + 1)
                        .ok_or_else(|| "--seed needs 32 hex bytes".to_string())?,
                )?);
                index += 2;
            }
            "--help" | "-h" => {
                println!("usage: avm-uploadd signer-pubkey --seed SEED_HEX");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let seed = seed.ok_or_else(|| "--seed is required".to_string())?;
    let key = SigningKey::from_bytes(&seed);
    println!("{}", hex(&key.verifying_key().to_bytes()));
    Ok(())
}

fn parse_hex32(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err(format!("expected 64 hex characters, got {}", value.len()));
    }
    let mut out = [0u8; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex at byte {index}"))? as u8;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex at byte {index}"))? as u8;
        out[index] = (high << 4) | low;
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble"));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).expect("nibble"));
    }
    out
}

fn serve(listener: TcpListener, config: Arc<Config>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let config = Arc::clone(&config);
                std::thread::spawn(move || {
                    if let Err(error) = handle_connection(stream, &config) {
                        eprintln!("avm-uploadd: connection error: {error}");
                    }
                });
            }
            Err(error) => eprintln!("avm-uploadd: accept error: {error}"),
        }
    }
}

fn handle_connection(mut stream: TcpStream, config: &Config) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));

    let request = match read_request(&mut stream, config.max_body) {
        Ok(request) => request,
        Err(error) => {
            write_response(
                &mut stream,
                error.status,
                &[],
                error.message.as_bytes(),
                false,
            )?;
            return Ok(());
        }
    };

    let (status, body) = route(&request, config);
    let head = request.method.eq_ignore_ascii_case("HEAD");
    write_response(&mut stream, status, &[], &body, head)
}

fn route(request: &Request, config: &Config) -> (u16, Vec<u8>) {
    let path = origin_path(&request.target);
    if path == "/health" || path == "/health/" {
        return (200, b"ok\n".to_vec());
    }

    let path = path.trim_start_matches('/');
    let mut parts = path.split('/');
    let token = parts.next().unwrap_or_default();
    if !constant_time_eq(token.as_bytes(), config.token.as_bytes()) {
        return (404, b"not found\n".to_vec());
    }

    let kind = parts.next().unwrap_or_default();
    let id = parts.next().unwrap_or_default();
    let rest = parts.next();
    if rest.is_some() {
        return (404, b"not found\n".to_vec());
    }

    match (request.method.as_str(), kind, id) {
        ("POST", "chunks", "missing") => missing_chunks(request, config),
        ("HEAD" | "GET", "chunks", hex) if is_hex(hex) => {
            serve_file(&chunk_path(&config.root, hex), request.method == "HEAD")
        }
        ("PUT", "chunks", hex) if is_hex(hex) => put_chunk(request, config, hex),
        ("HEAD" | "GET", "manifests", hex) if is_hex(hex) => {
            serve_file(&manifest_path(&config.root, hex), request.method == "HEAD")
        }
        ("PUT", "manifests", hex) if is_hex(hex) => put_object(
            request,
            config,
            &manifest_path(&config.root, hex),
            "manifest",
        ),
        ("GET", "manifests", "") => (200, list_manifests(config)),
        _ => (404, b"not found\n".to_vec()),
    }
}

fn missing_chunks(request: &Request, config: &Config) -> (u16, Vec<u8>) {
    if request.body.len() > MAX_MISSING_BODY {
        return (413, b"missing-list request too large\n".to_vec());
    }
    let value: serde_json::Value = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(_) => return (400, b"invalid JSON\n".to_vec()),
    };
    let Some(hashes) = value.get("hashes").and_then(|v| v.as_array()) else {
        return (400, b"missing `hashes` array\n".to_vec());
    };
    let mut missing = Vec::new();
    for hash in hashes {
        let Some(hash) = hash.as_str() else {
            return (400, b"hash is not a string\n".to_vec());
        };
        if !is_hex(hash) {
            return (400, b"hash is not 64 lowercase hex characters\n".to_vec());
        }
        if !chunk_path(&config.root, hash).is_file() {
            missing.push(hash);
        }
    }
    match serde_json::to_vec(&serde_json::json!({ "missing": missing })) {
        Ok(body) => (200, body),
        Err(_) => (500, b"serialization failed\n".to_vec()),
    }
}

fn put_chunk(request: &Request, config: &Config, hex: &str) -> (u16, Vec<u8>) {
    if request.body.len() as u64 > config.max_body {
        return (413, b"chunk too large\n".to_vec());
    }
    let actual = blake3::hash(&request.body);
    if actual.to_hex().as_str() != hex {
        return (409, b"digest mismatch\n".to_vec());
    }
    match write_atomic(&chunk_path(&config.root, hex), &request.body) {
        Ok(()) => (201, b"stored\n".to_vec()),
        Err(error) => {
            eprintln!("avm-uploadd: cannot store chunk: {error}");
            (500, b"store failed\n".to_vec())
        }
    }
}

fn put_object(request: &Request, config: &Config, path: &Path, what: &str) -> (u16, Vec<u8>) {
    if request.body.len() as u64 > config.max_body {
        return (413, format!("{what} too large\n").into_bytes());
    }
    match write_atomic(path, &request.body) {
        Ok(()) => (201, b"stored\n".to_vec()),
        Err(error) => {
            eprintln!("avm-uploadd: cannot store {what}: {error}");
            (500, b"store failed\n".to_vec())
        }
    }
}

fn list_manifests(config: &Config) -> Vec<u8> {
    let mut ids = Vec::new();
    let dir = config.root.join("manifests");
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Ok(inner) = fs::read_dir(&path)
            {
                for file in inner.flatten() {
                    if let Some(name) = file.file_name().to_str()
                        && is_hex(name)
                    {
                        ids.push(name.to_string());
                    }
                }
            }
        }
    }
    ids.sort();
    serde_json::to_vec(&serde_json::json!({ "manifests": ids }))
        .unwrap_or_else(|_| b"{\"manifests\":[]}".to_vec())
}

fn serve_file(path: &Path, head: bool) -> (u16, Vec<u8>) {
    match fs::read(path) {
        Ok(bytes) => (200, if head { Vec::new() } else { bytes }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (404, b"not found\n".to_vec()),
        Err(error) => {
            eprintln!("avm-uploadd: cannot read {}: {error}", path.display());
            (500, b"read failed\n".to_vec())
        }
    }
}

fn read_request(stream: &mut TcpStream, max_body: u64) -> Result<Request, RequestError> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(index) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(RequestError::new(431, "request headers too large\n"));
        }
        let read = stream
            .read(&mut chunk)
            .map_err(|error| RequestError::new(400, format!("read failed: {error}\n")))?;
        if read == 0 {
            return Err(RequestError::new(400, "empty request\n"));
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let header = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| RequestError::new(400, "headers are not UTF-8\n"))?;
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| RequestError::new(400, "missing request line\n"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| RequestError::new(400, "missing method\n"))?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .ok_or_else(|| RequestError::new(400, "missing target\n"))?
        .to_string();
    let version = parts
        .next()
        .ok_or_else(|| RequestError::new(400, "missing HTTP version\n"))?;
    if !version.starts_with("HTTP/1.") {
        return Err(RequestError::new(505, "only HTTP/1.x is supported\n"));
    }

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(RequestError::new(400, "malformed header\n"));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    if headers
        .get("transfer-encoding")
        .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
    {
        return Err(RequestError::new(411, "chunked bodies are not supported\n"));
    }

    let content_length = match headers.get("content-length") {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| RequestError::new(400, "invalid Content-Length\n"))?,
        None => 0,
    };
    if content_length > max_body {
        return Err(RequestError::new(413, "request body too large\n"));
    }

    let mut body = buf[header_end..].to_vec();
    while (body.len() as u64) < content_length {
        let read = stream
            .read(&mut chunk)
            .map_err(|error| RequestError::new(400, format!("read body failed: {error}\n")))?;
        if read == 0 {
            return Err(RequestError::new(400, "truncated request body\n"));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length as usize);

    Ok(Request { method, target, body })
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
    head: bool,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes())?;
    if !head {
        stream.write_all(body)?;
    }
    stream.flush()
}

fn origin_path(target: &str) -> &str {
    let mut target = target;
    if let Some(rest) = target.strip_prefix("http://") {
        target = rest.find('/').map_or("/", |index| &rest[index..]);
    } else if let Some(rest) = target.strip_prefix("https://") {
        target = rest.find('/').map_or("/", |index| &rest[index..]);
    }
    target.split('?').next().unwrap_or("/")
}

fn chunk_path(root: &Path, hex: &str) -> PathBuf {
    root.join("chunks").join(&hex[..2]).join(hex)
}

fn manifest_path(root: &Path, hex: &str) -> PathBuf {
    root.join("manifests").join(&hex[..2]).join(hex)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    Ok(())
}

fn is_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn start_server() -> (SocketAddr, tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let config = Arc::new(Config {
            token: token.into(),
            root: dir.path().join("cache"),
            max_body: 1024 * 1024,
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || serve(listener, config));
        (addr, dir, token.to_string())
    }

    fn request(addr: SocketAddr, raw: &[u8]) -> Vec<u8> {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(raw).unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn origin_path_accepts_origin_and_absolute_forms() {
        assert_eq!(origin_path("/a/b?x=1"), "/a/b");
        assert_eq!(origin_path("http://host:1/a/b?x=1"), "/a/b");
        assert_eq!(origin_path("https://host"), "/");
    }

    #[test]
    fn chunk_upload_is_digest_checked_and_listed_missing() {
        let (addr, _dir, token) = start_server();
        let payload = b"hello";
        let digest = blake3::hash(payload).to_hex().to_string();
        let put = format!(
            "PUT /{token}/chunks/{digest} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\nhello",
            payload.len()
        );
        let response = request(addr, put.as_bytes());
        assert!(response.starts_with(b"HTTP/1.1 201"), "{response:?}");

        let body = format!("{{\"hashes\":[\"{digest}\",\"{}\"]}}", "0".repeat(64));
        let missing = format!(
            "POST /{token}/chunks/missing HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let response = request(addr, missing.as_bytes());
        let text = String::from_utf8_lossy(&response);
        assert!(text.contains(&"0".repeat(64)), "{text}");
        assert!(!text.contains(&digest), "{text}");

        let bad = format!(
            "PUT /{token}/chunks/{digest} HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nwrong"
        );
        let response = request(addr, bad.as_bytes());
        assert!(response.starts_with(b"HTTP/1.1 409"), "{response:?}");
    }

    #[test]
    fn token_is_required() {
        let (addr, _dir, _token) = start_server();
        let response = request(addr, b"HEAD /wrong/chunks/0000000000000000000000000000000000000000000000000000000000000000 HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(response.starts_with(b"HTTP/1.1 404"), "{response:?}");
    }

    fn test_manifest(seed: Option<&[u8; 32]>) -> (Vec<u8>, [u8; 32], [u8; 32]) {
        use ed25519_dalek::Signer;

        let mut unsigned = Vec::new();
        unsigned.extend_from_slice(b"AVMM");
        unsigned.extend_from_slice(&2u16.to_le_bytes());
        for value in ["pkg", "1.0.0"] {
            unsigned.extend_from_slice(&(value.len() as u32).to_le_bytes());
            unsigned.extend_from_slice(value.as_bytes());
        }
        unsigned.extend_from_slice(&5u64.to_le_bytes());
        unsigned.push(0); // no drv_hash
        unsigned.push(0); // no closure
        unsigned.extend_from_slice(&1u32.to_le_bytes());
        unsigned.extend_from_slice(&[7u8; 32]);
        let id = *blake3::hash(&unsigned).as_bytes();

        let Some(seed) = seed else {
            unsigned.push(0); // no pubkey
            unsigned.push(0); // no signature
            return (unsigned, id, [0u8; 32]);
        };
        let key = SigningKey::from_bytes(seed);
        let pubkey = key.verifying_key().to_bytes();
        let signature = key.sign(&id);
        unsigned.push(1);
        unsigned.extend_from_slice(&pubkey);
        unsigned.push(1);
        unsigned.extend_from_slice(&signature.to_bytes());
        (unsigned, id, pubkey)
    }

    #[test]
    fn signed_manifest_accepts_an_allowed_signer() {
        let seed = [9u8; 32];
        let (bytes, id, pubkey) = test_manifest(Some(&seed));
        assert_eq!(
            verify_manifest_bytes(&bytes, &id, &[pubkey]).unwrap(),
            pubkey
        );
    }

    #[test]
    fn unsigned_manifest_is_rejected() {
        let (bytes, id, _) = test_manifest(None);
        let error = verify_manifest_bytes(&bytes, &id, &[[1u8; 32]]).unwrap_err();
        assert!(error.contains("unsigned"), "{error}");
    }

    #[test]
    fn wrong_signer_is_rejected() {
        let (bytes, id, _) = test_manifest(Some(&[9u8; 32]));
        let error = verify_manifest_bytes(&bytes, &id, &[[1u8; 32]]).unwrap_err();
        assert!(error.contains("not allowed"), "{error}");
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let (mut bytes, id, pubkey) = test_manifest(Some(&[9u8; 32]));
        *bytes.last_mut().unwrap() ^= 1;
        let error = verify_manifest_bytes(&bytes, &id, &[pubkey]).unwrap_err();
        assert!(error.contains("signature mismatch"), "{error}");
    }

    #[test]
    fn manifest_id_must_match_the_staged_name() {
        let (bytes, _id, pubkey) = test_manifest(Some(&[9u8; 32]));
        let error = verify_manifest_bytes(&bytes, &[0u8; 32], &[pubkey]).unwrap_err();
        assert!(error.contains("does not match"), "{error}");
    }
}
