use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use bytes::Bytes;
use graft_workspace::{
    ManifestPublicKey, ManifestSigningKey,
    config::MediaConfig,
    update::{self, UpdateErr, UpdateRepo},
};
use http::{HeaderValue, Request, Response, StatusCode};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;

fn repo(dir: &Path) -> UpdateRepo {
    UpdateRepo::from_config(
        &MediaConfig::Fs {
            root: dir.join("repo").display().to_string(),
        },
        "updates",
        "demo",
    )
    .unwrap()
}

fn serve_h2(root: PathBuf) -> SocketAddr {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        runtime.block_on(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let root = root.clone();
                tokio::spawn(async move {
                    let Ok(mut connection) = h2::server::handshake(socket).await else {
                        return;
                    };
                    while let Some(request) = connection.accept().await {
                        let Ok((request, respond)) = request else {
                            break;
                        };
                        let root = root.clone();
                        tokio::spawn(async move {
                            let _ = respond_file(request, respond, &root).await;
                        });
                    }
                });
            }
        });
    });
    addr
}

async fn respond_file(
    request: Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    root: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let name = request.uri().path().trim_start_matches('/');
    if name.is_empty() || name.contains("..") {
        let response = Response::builder().status(StatusCode::NOT_FOUND).body(())?;
        respond.send_response(response, true)?;
        return Ok(());
    }
    let path = root.join(name);
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(_) => {
            let response = Response::builder().status(StatusCode::NOT_FOUND).body(())?;
            respond.send_response(response, true)?;
            return Ok(());
        }
    };

    let head = request.method() == http::Method::HEAD;
    let mut status = StatusCode::OK;
    let mut start = 0usize;
    let mut end = data.len();
    let mut content_range = None;
    if let Some(value) = request.headers().get("range")
        && let Some(value) = value.to_str().ok()
        && let Some(range) = value.strip_prefix("bytes=")
        && let Some((from, to)) = range.split_once('-')
    {
        start = from.parse::<usize>().unwrap_or(0);
        end = if to.is_empty() {
            data.len()
        } else {
            to.parse::<usize>()
                .map_or(data.len(), |to| (to + 1).min(data.len()))
        };
        if start >= data.len() {
            let response = Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .body(())?;
            respond.send_response(response, true)?;
            return Ok(());
        }
        status = StatusCode::PARTIAL_CONTENT;
        content_range = Some(format!(
            "bytes {start}-{}/{total}",
            end.saturating_sub(1),
            total = data.len()
        ));
    }
    let body = if head {
        Vec::new()
    } else {
        data[start..end].to_vec()
    };
    let content_length = if head { data.len() } else { body.len() };
    let mut response = Response::builder()
        .status(status)
        .header("content-length", content_length.to_string())
        .header("accept-ranges", "bytes");
    if let Some(range) = content_range {
        response = response.header("content-range", HeaderValue::from_str(&range)?);
    }
    let mut send = respond.send_response(response.body(())?, false)?;
    if head {
        send.send_data(Bytes::new(), true)?;
    } else {
        send.send_data(Bytes::from(body), true)?;
    }
    Ok(())
}

fn serve_h3(root: PathBuf, ca_file: PathBuf) -> SocketAddr {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    fs::write(&ca_file, cert.pem()).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let endpoint = runtime
        .block_on(async move { quinn::Endpoint::server(server_config, ([127, 0, 0, 1], 0).into()) })
        .unwrap();
    let addr = endpoint.local_addr().unwrap();
    thread::spawn(move || {
        runtime.block_on(async move {
            while let Some(incoming) = endpoint.accept().await {
                let root = root.clone();
                tokio::spawn(async move {
                    let Ok(connection) = incoming.await else {
                        return;
                    };
                    let Ok(mut server) = h3::server::Connection::<_, Bytes>::new(
                        h3_quinn::Connection::new(connection),
                    )
                    .await
                    else {
                        return;
                    };
                    while let Ok(Some(request)) = server.accept().await {
                        let root = root.clone();
                        tokio::spawn(async move {
                            let Ok((request, mut stream)) = request.resolve_request().await else {
                                return;
                            };
                            let _ = respond_h3(request, &mut stream, &root).await;
                        });
                    }
                });
            }
        });
    });
    addr
}

async fn respond_h3<C, B>(
    request: Request<()>,
    stream: &mut h3::server::RequestStream<C, B>,
    root: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    C: h3::quic::BidiStream<B>,
    B: bytes::Buf + From<Vec<u8>>,
{
    let name = request.uri().path().trim_start_matches('/');
    let path = root.join(name);
    let data = if name.is_empty() || name.contains("..") {
        None
    } else {
        fs::read(path).ok()
    };
    let Some(data) = data else {
        stream
            .send_response(Response::builder().status(StatusCode::NOT_FOUND).body(())?)
            .await?;
        stream.finish().await?;
        return Ok(());
    };

    let head = request.method() == http::Method::HEAD;
    let mut status = StatusCode::OK;
    let mut start = 0usize;
    let mut end = data.len();
    let mut content_range = None;
    if let Some(value) = request.headers().get("range")
        && let Some(value) = value.to_str().ok()
        && let Some(range) = value.strip_prefix("bytes=")
        && let Some((from, to)) = range.split_once('-')
    {
        start = from.parse::<usize>().unwrap_or(0);
        end = if to.is_empty() {
            data.len()
        } else {
            to.parse::<usize>()
                .map_or(data.len(), |to| (to + 1).min(data.len()))
        };
        if start >= data.len() {
            stream
                .send_response(
                    Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .body(())?,
                )
                .await?;
            stream.finish().await?;
            return Ok(());
        }
        status = StatusCode::PARTIAL_CONTENT;
        content_range = Some(format!(
            "bytes {start}-{}/{total}",
            end.saturating_sub(1),
            total = data.len()
        ));
    }
    let body = if head {
        Vec::new()
    } else {
        data[start..end].to_vec()
    };
    let content_length = if head { data.len() } else { body.len() };
    let mut response = Response::builder()
        .status(status)
        .header("content-length", content_length.to_string())
        .header("accept-ranges", "bytes");
    if let Some(range) = content_range {
        response = response.header("content-range", HeaderValue::from_str(&range)?);
    }
    stream.send_response(response.body(())?).await?;
    if !body.is_empty() {
        stream.send_data(B::from(body)).await?;
    }
    stream.finish().await?;
    Ok(())
}

#[test]
fn h3_repository_serves_signed_releases_with_a_private_ca() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let local = repo(dir);

    let private = dir.join("private.key");
    let public = dir.join("public.key");
    update::generate_keypair(&private, &public).unwrap();
    let signing = ManifestSigningKey::load(&private).unwrap();
    let trust = ManifestPublicKey::load(&public).unwrap();

    let release = dir.join("release");
    fs::create_dir_all(&release).unwrap();
    let payload: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(17) % 249) as u8)
        .collect();
    fs::write(release.join("payload.bin"), &payload).unwrap();
    fs::write(release.join("version.txt"), b"quic\n").unwrap();
    local.publish("1.0.0", &release, Some(&signing)).unwrap();

    let ca_file = dir.join("ca.pem");
    let addr = serve_h3(dir.join("repo"), ca_file.clone());
    let http = UpdateRepo::from_locator_with_ca(
        &format!("quic://{addr}"),
        "updates",
        "demo",
        Some(&ca_file),
    )
    .unwrap();
    let manifest = http.manifest("1.0.0", Some(&trust)).unwrap();
    assert_eq!(manifest.files.len(), 2);

    let install = dir.join("install");
    let state = dir.join("state.json");
    let report = http
        .apply("1.0.0", &install, &state, None, false, Some(&trust), None)
        .unwrap();
    assert_eq!(report.files_written, 2);
    assert_eq!(fs::read(install.join("payload.bin")).unwrap(), payload);
    assert_eq!(fs::read(install.join("version.txt")).unwrap(), b"quic\n");
}

#[test]
fn h3_repository_installs_compressed_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let local = repo(dir);

    let private = dir.join("private.key");
    let public = dir.join("public.key");
    update::generate_keypair(&private, &public).unwrap();
    let signing = ManifestSigningKey::load(&private).unwrap();
    let trust = ManifestPublicKey::load(&public).unwrap();

    let release = dir.join("release");
    fs::create_dir_all(&release).unwrap();
    let mut text = Vec::new();
    while text.len() < 200_000 {
        text.extend_from_slice(b"compressed over http3 release payload\n");
    }
    fs::write(release.join("payload.txt"), &text).unwrap();
    let published = local
        .publish_with_compression("1.0.0", &release, Some(&signing), Some(9))
        .unwrap();
    assert!(published.compressed_chunks > 0, "{published:?}");

    let ca_file = dir.join("ca.pem");
    let addr = serve_h3(dir.join("repo"), ca_file.clone());
    let http = UpdateRepo::from_locator_with_ca(
        &format!("quic://{addr}"),
        "updates",
        "demo",
        Some(&ca_file),
    )
    .unwrap();
    let manifest = http.manifest("1.0.0", Some(&trust)).unwrap();
    assert!(
        manifest
            .files
            .iter()
            .flat_map(|file| &file.chunks)
            .any(|chunk| chunk.encoding.is_some()),
        "the manifest records compressed chunks"
    );

    let install = dir.join("install");
    let state = dir.join("state.json");
    let report = http
        .apply("1.0.0", &install, &state, None, false, Some(&trust), None)
        .unwrap();
    assert_eq!(report.files_written, 1);
    assert_eq!(fs::read(install.join("payload.txt")).unwrap(), text);
}

#[test]
fn h2_repository_serves_signed_manifests_and_deltas() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let local = repo(dir);

    let private = dir.join("private.key");
    let public = dir.join("public.key");
    update::generate_keypair(&private, &public).unwrap();
    let signing = ManifestSigningKey::load(&private).unwrap();
    let trust = ManifestPublicKey::load(&public).unwrap();

    let release = dir.join("release");
    fs::create_dir_all(&release).unwrap();
    let payload: Vec<u8> = (0..400_000u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect();
    fs::write(release.join("payload.bin"), &payload).unwrap();
    fs::write(release.join("version.txt"), b"one\n").unwrap();
    local.publish("1.0.0", &release, Some(&signing)).unwrap();

    let addr = serve_h2(dir.join("repo"));
    let http = UpdateRepo::from_locator(&format!("h2c://{addr}"), "updates", "demo").unwrap();
    let manifest = http.manifest("1.0.0", Some(&trust)).unwrap();
    assert_eq!(manifest.files.len(), 2);

    let install = dir.join("install");
    let state = dir.join("state.json");
    let report = http
        .apply("1.0.0", &install, &state, None, false, Some(&trust), None)
        .unwrap();
    assert_eq!(report.files_written, 2);
    assert_eq!(fs::read(install.join("payload.bin")).unwrap(), payload);

    fs::write(release.join("version.txt"), b"two\n").unwrap();
    local.publish("2.0.0", &release, Some(&signing)).unwrap();
    let installed = update::read_state(&state, "demo").unwrap();
    let plan = http.plan("2.0.0", Some(&installed), Some(&trust)).unwrap();
    assert_eq!(plan.files_changed, 1);
    assert_eq!(plan.files_unchanged, 1);
    assert!(plan.bytes_missing < 1024, "{plan:?}");

    let err = http.publish("3.0.0", &release, None).unwrap_err();
    assert!(matches!(err, UpdateErr::ReadOnly(_)), "{err:?}");
    let err = http.releases().unwrap_err();
    assert!(matches!(err, UpdateErr::NoListing(_)), "{err:?}");
}
