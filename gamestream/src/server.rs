//! A GameStream host for stock Moonlight: `serverinfo` (HTTP and HTTPS),
//! `pair` (the host end of `pairing`, with the PIN the person types here
//! from Moonlight's screen) and, over HTTPS with a paired client's
//! certificate, `applist`, `launch`, `resume` and `cancel`; the launched
//! stream itself is `stream`. Responses follow Sunshine's `nvhttp.cpp`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::crypto::Credentials;
use crate::pairing::HostPairing;
use crate::stream::{Apps, Launch};
use crate::{xml, GameStreamError};

/// GameStream's HTTPS port.
pub const HTTPS_PORT: u16 = 47984;
/// What hosts of GameStream generation 7 say they are; Moonlight reads the
/// generation from the first number.
pub const APP_VERSION: &str = "7.1.431.-1";
const GFE_VERSION: &str = "3.23.0.74";

/// A client waiting for the person to type its PIN here.
#[derive(Debug, Clone)]
pub struct PairingRequest {
    pub device_name: String,
    pub address: SocketAddr,
}

struct Pending {
    pairing: HostPairing,
    request: PairingRequest,
    /// The held `getservercert` answer.
    answer: Option<oneshot::Sender<String>>,
}

/// The host's GameStream face.
pub struct GameStreamServer {
    name: String,
    unique_id: String,
    credentials: Credentials,
    apps: Arc<dyn Apps>,
    /// The launched app, if any.
    launch: Mutex<Option<Arc<Launch>>>,
    paired: Mutex<Vec<Vec<u8>>>,
    paired_path: PathBuf,
    pending: Mutex<HashMap<String, Pending>>,
    /// `ServerCodecModeSupport`.
    pub codec_modes: u32,
    /// The HTTPS port `serverinfo` names: the one being served.
    https_port: std::sync::atomic::AtomicU16,
    /// The RTSP port launches name: the one being served.
    rtsp_port: std::sync::atomic::AtomicU16,
}

impl GameStreamServer {
    /// A host named `name` with its credentials and paired clients kept in
    /// `dir`, offering `apps`.
    pub fn open(
        name: &str,
        dir: &std::path::Path,
        apps: Arc<dyn Apps>,
    ) -> Result<Arc<Self>, GameStreamError> {
        let credentials = Credentials::load_or_generate(dir, "windowcast GameStream Host")?;
        let paired_path = dir.join("gamestream-clients");
        let paired = std::fs::read_to_string(&paired_path)
            .map(|text| text.lines().filter_map(xml::decode_hex).collect())
            .unwrap_or_default();
        let unique_id = {
            use sha2::Digest;
            let digest = sha2::Sha256::digest(&credentials.cert_der);
            let h = xml::encode_hex(&digest[..16]).to_uppercase();
            format!(
                "{}-{}-{}-{}-{}",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..32]
            )
        };
        Ok(Arc::new(GameStreamServer {
            name: name.to_owned(),
            unique_id,
            credentials,
            apps,
            launch: Mutex::new(None),
            paired: Mutex::new(paired),
            paired_path,
            pending: Mutex::default(),
            codec_modes: 1,
            https_port: std::sync::atomic::AtomicU16::new(HTTPS_PORT),
            rtsp_port: std::sync::atomic::AtomicU16::new(crate::rtsp::RTSP_PORT),
        }))
    }

    /// Clients waiting for their PIN to be typed here.
    pub fn pairing_requests(&self) -> Vec<PairingRequest> {
        let pending = self.pending.lock().expect("pending");
        pending
            .values()
            .filter(|p| p.answer.is_some())
            .map(|p| p.request.clone())
            .collect()
    }

    /// The person typed the PIN a waiting client shows. With several
    /// waiting, the one from `device_name` (any if `None`).
    pub fn enter_pin(&self, pin: &str, device_name: Option<&str>) -> bool {
        let mut pending = self.pending.lock().expect("pending");
        let Some(waiting) = pending
            .values_mut()
            .find(|p| p.answer.is_some() && device_name.is_none_or(|n| n == p.request.device_name))
        else {
            return false;
        };
        let answer = waiting.pairing.pin(pin, &self.credentials);
        waiting
            .answer
            .take()
            .is_some_and(|sender| sender.send(answer).is_ok())
    }

    pub fn paired_clients(&self) -> usize {
        self.paired.lock().expect("paired").len()
    }

    fn is_paired(&self, cert: &[u8]) -> bool {
        self.paired
            .lock()
            .expect("paired")
            .iter()
            .any(|c| c == cert)
    }

    fn pin_client(&self, cert: Vec<u8>) {
        let mut paired = self.paired.lock().expect("paired");
        if !paired.contains(&cert) {
            paired.push(cert);
            let text: String = paired.iter().map(|c| xml::encode_hex(c) + "\n").collect();
            if let Err(e) = std::fs::write(&self.paired_path, text) {
                eprintln!("gamestream: could not save the paired clients: {e}");
            }
        }
    }

    /// Serves plain HTTP on `http`, HTTPS on `https` and the launched
    /// stream's RTSP on `rtsp` until one fails. Streams use the standard
    /// GameStream UDP ports (47998, 47999, 48000).
    pub async fn serve(
        self: Arc<Self>,
        http: TcpListener,
        https: TcpListener,
        rtsp: TcpListener,
    ) -> std::io::Result<()> {
        self.rtsp_port.store(
            rtsp.local_addr()?.port(),
            std::sync::atomic::Ordering::SeqCst,
        );
        {
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                while let Ok((stream, _)) = rtsp.accept().await {
                    let launch = server.launch.lock().expect("launch").clone();
                    let apps = Arc::clone(&server.apps);
                    tokio::spawn(async move {
                        if let Err(e) = crate::stream::serve_rtsp(stream, launch, apps).await {
                            eprintln!("gamestream: rtsp: {e}");
                        }
                    });
                }
            });
        }
        let tls = Arc::new(self.tls_config().map_err(std::io::Error::other)?);
        self.https_port.store(
            https.local_addr()?.port(),
            std::sync::atomic::Ordering::SeqCst,
        );
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
        let plain = {
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                loop {
                    let (stream, from) = http.accept().await?;
                    let server = Arc::clone(&server);
                    let local = stream.local_addr()?;
                    tokio::spawn(async move { server.connection(stream, from, local, None).await });
                }
                #[allow(unreachable_code)]
                Ok::<(), std::io::Error>(())
            })
        };
        loop {
            if plain.is_finished() {
                return Err(std::io::Error::other("the HTTP listener stopped"));
            }
            let (stream, from) = https.accept().await?;
            let local = stream.local_addr()?;
            let server = Arc::clone(&self);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return;
                };
                let peer = tls
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|certs| certs.first())
                    .map(|c| c.as_ref().to_vec());
                server.connection(tls, from, local, Some(peer)).await;
            });
        }
    }

    fn tls_config(&self) -> Result<rustls::ServerConfig, GameStreamError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| GameStreamError::Tls(e.to_string()))?
            .with_client_cert_verifier(Arc::new(AnyClient { provider }))
            .with_single_cert(
                vec![CertificateDer::from(self.credentials.cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.credentials.key_der()?)),
            )
            .map_err(|e| GameStreamError::Tls(e.to_string()))
    }

    /// One request per connection, as GameStream clients make them.
    /// `peer` is `Some` on HTTPS: the client's certificate, if it sent one.
    async fn connection<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut stream: S,
        from: SocketAddr,
        local: SocketAddr,
        peer: Option<Option<Vec<u8>>>,
    ) {
        let Some((path, query)) = read_request(&mut stream).await else {
            return;
        };
        let https = peer.is_some();
        let cert = peer.flatten();
        let arg = |name: &str| {
            query
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let body = match path.as_str() {
            "/serverinfo" => {
                self.server_info(local, cert.as_deref().is_some_and(|c| self.is_paired(c)))
            }
            "/pair" => match self.pair(&query, from, https, cert.as_deref()).await {
                Some(body) => body,
                None => return,
            },
            "/applist" if cert.as_deref().is_some_and(|c| self.is_paired(c)) => self.app_list(),
            "/launch" | "/resume" if cert.as_deref().is_some_and(|c| self.is_paired(c)) => {
                self.launch(path == "/resume", &arg, local)
            }
            "/cancel" if cert.as_deref().is_some_and(|c| self.is_paired(c)) => {
                if let Some(launch) = self.launch.lock().expect("launch").take() {
                    launch.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                xml::response(200, None, &[("cancel", "1".into())])
            }
            "/applist" | "/launch" | "/resume" | "/cancel" => xml::response(
                401,
                Some("The client is not authorized. Certificate verification failed."),
                &[],
            ),
            _ => xml::response(404, None, &[]),
        };
        let _ = write_response(&mut stream, &body).await;
    }

    fn server_info(&self, local: SocketAddr, paired: bool) -> String {
        xml::response(
            200,
            None,
            &[
                ("hostname", self.name.clone()),
                ("appversion", APP_VERSION.into()),
                ("GfeVersion", GFE_VERSION.into()),
                ("uniqueid", self.unique_id.clone()),
                (
                    "HttpsPort",
                    self.https_port
                        .load(std::sync::atomic::Ordering::SeqCst)
                        .to_string(),
                ),
                ("ExternalPort", crate::client::HTTP_PORT.to_string()),
                ("MaxLumaPixelsHEVC", "0".into()),
                ("mac", "00:00:00:00:00:00".into()),
                ("LocalIP", local.ip().to_string()),
                ("ServerCodecModeSupport", self.codec_modes.to_string()),
                ("PairStatus", u8::from(paired).to_string()),
                ("currentgame", self.current_game().to_string()),
                (
                    "state",
                    if self.current_game() > 0 {
                        "SUNSHINE_SERVER_BUSY"
                    } else {
                        "SUNSHINE_SERVER_FREE"
                    }
                    .into(),
                ),
            ],
        )
    }

    fn current_game(&self) -> u32 {
        self.launch
            .lock()
            .expect("launch")
            .as_ref()
            .filter(|l| !l.stop.load(std::sync::atomic::Ordering::SeqCst))
            .map_or(0, |l| l.app)
    }

    /// `/launch` (or `/resume` of the running app): keeps the client's input
    /// key and mode for the stream and answers with its RTSP address.
    fn launch(
        &self,
        resume: bool,
        arg: &dyn Fn(&str) -> Option<String>,
        local: SocketAddr,
    ) -> String {
        let key = arg("rikey")
            .and_then(|k| xml::decode_hex(&k))
            .and_then(|k| <[u8; 16]>::try_from(k).ok());
        let key_id = arg("rikeyid").and_then(|k| k.parse::<i64>().ok());
        let (Some(key), Some(key_id)) = (key, key_id) else {
            return xml::response(
                400,
                Some("Missing a required launch parameter"),
                &[("resume", "0".into())],
            );
        };
        let mut launch = self.launch.lock().expect("launch");
        let running = launch
            .as_ref()
            .filter(|l| !l.stop.load(std::sync::atomic::Ordering::SeqCst));
        let app = if resume {
            match running {
                Some(l) => l.app,
                None => {
                    return xml::response(
                        503,
                        Some("No app is running to resume"),
                        &[("resume", "0".into())],
                    )
                }
            }
        } else {
            if running.is_some() {
                return xml::response(
                    400,
                    Some("An app is already running on this host"),
                    &[("resume", "0".into())],
                );
            }
            match arg("appid").and_then(|a| a.parse::<u32>().ok()) {
                Some(app) if self.apps.apps().iter().any(|a| a.id == app) => app,
                _ => {
                    return xml::response(404, Some("No such app"), &[("gamesession", "0".into())])
                }
            }
        };
        let mode: Vec<u32> = arg("mode")
            .unwrap_or_default()
            .split('x')
            .filter_map(|v| v.parse().ok())
            .collect();
        let mode = (
            mode.first().copied().unwrap_or(1280),
            mode.get(1).copied().unwrap_or(720),
            mode.get(2).copied().filter(|f| *f > 0).unwrap_or(60),
        );
        if let Some(old) = launch.take() {
            old.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        *launch = Some(Launch::new(app, key, key_id as u32, mode));
        let host = match local.ip() {
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
            ip => ip.to_string(),
        };
        xml::response(
            200,
            None,
            &[
                (
                    "sessionUrl0",
                    format!(
                        "rtsp://{host}:{}",
                        self.rtsp_port.load(std::sync::atomic::Ordering::SeqCst)
                    ),
                ),
                ("gamesession", "1".into()),
                ("resume", u8::from(resume).to_string()),
            ],
        )
    }

    fn app_list(&self) -> String {
        let mut out =
            String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">");
        for app in self.apps.apps() {
            out.push_str(&format!(
                "<App><IsHdrSupported>{}</IsHdrSupported><AppTitle>{}</AppTitle><ID>{}</ID></App>",
                u8::from(app.hdr),
                xml::escape(&app.title),
                app.id
            ));
        }
        out.push_str("</root>");
        out
    }

    /// One `/pair` request. `None` when the connection is held and dropped
    /// without an answer (the pairing was abandoned).
    async fn pair(
        &self,
        query: &[(String, String)],
        from: SocketAddr,
        https: bool,
        cert: Option<&[u8]>,
    ) -> Option<String> {
        let arg = |name: &str| {
            query
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let Some(unique_id) = arg("uniqueid") else {
            return Some(xml::response(400, Some("Missing uniqueid parameter"), &[]));
        };
        match arg("phrase").as_deref() {
            Some("getservercert") => {
                let pairing = match HostPairing::start(
                    &arg("salt").unwrap_or_default(),
                    &arg("clientcert").unwrap_or_default(),
                    &arg("devicename").unwrap_or_default(),
                ) {
                    Ok(pairing) => pairing,
                    Err(message) => {
                        return Some(xml::response(400, Some(message), &[("paired", "0".into())]))
                    }
                };
                let (tx, rx) = oneshot::channel();
                {
                    let mut pending = self.pending.lock().expect("pending");
                    if pending.get(&unique_id).is_some_and(|p| p.answer.is_some()) {
                        return Some(xml::response(
                            409,
                            Some("A pairing session with this uniqueid already exists"),
                            &[("paired", "0".into())],
                        ));
                    }
                    let request = PairingRequest {
                        device_name: pairing.device_name.clone(),
                        address: from,
                    };
                    println!(
                        "gamestream: {} at {from} wants to pair; type the PIN it shows",
                        request.device_name
                    );
                    pending.insert(
                        unique_id.clone(),
                        Pending {
                            pairing,
                            request,
                            answer: Some(tx),
                        },
                    );
                }
                // Held until the person types the PIN (or gives up).
                match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
                    Ok(Ok(answer)) => Some(answer),
                    _ => {
                        self.pending.lock().expect("pending").remove(&unique_id);
                        None
                    }
                }
            }
            Some("pairchallenge") if https && cert.is_some_and(|c| self.is_paired(c)) => {
                Some(xml::response(200, None, &[("paired", "1".into())]))
            }
            Some("pairchallenge") => Some(xml::response(200, None, &[("paired", "0".into())])),
            _ => {
                let mut pending = self.pending.lock().expect("pending");
                let Some(waiting) = pending.get_mut(&unique_id) else {
                    return Some(xml::response(400, Some("Invalid uniqueid"), &[]));
                };
                let step = waiting.pairing.step(query, &self.credentials);
                if let Some(cert) = step.paired {
                    println!("gamestream: paired with {}", waiting.request.device_name);
                    self.pin_client(cert);
                }
                if step.done {
                    pending.remove(&unique_id);
                }
                Some(step.xml)
            }
        }
    }
}

/// Reads `GET /path?query HTTP/1.1` and its headers; returns the path and
/// the decoded query.
async fn read_request<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Option<(String, Vec<(String, String)>)> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 64 * 1024 {
            return None;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(10), stream.read(&mut byte)).await
        {
            Ok(Ok(1)) => head.push(byte[0]),
            _ => return None,
        }
    }
    let head = String::from_utf8_lossy(&head);
    let target = head.lines().next()?.split(' ').nth(1)?.to_owned();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (urldecode(k), urldecode(v))
        })
        .collect();
    Some((path.to_owned(), query))
}

fn urldecode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| text.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (escaped, bytes[i]) {
            (Some(b), _) => {
                out.push(b);
                i += 3;
                continue;
            }
            (None, b'+') => out.push(b' '),
            (None, b) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn write_response<S: AsyncWrite + Unpin>(stream: &mut S, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}

/// Takes any client certificate in the handshake (checking only that the
/// client holds its key); which ones are paired is checked per request.
#[derive(Debug)]
struct AnyClient {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ClientCertVerifier for AnyClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
