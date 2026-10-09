//! A GameStream client's HTTP side, for Sunshine, Apollo and windowcast's
//! own GameStream host: `serverinfo` over plain HTTP, pairing, and over
//! HTTPS (this client's certificate one way, the host's pinned certificate
//! the other) the app list, launching, resuming and quitting. Requests and
//! parameters follow moonlight-qt's `NvHTTP`.

use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::crypto::Credentials;
use crate::pairing::{self, PairRequests};
use crate::{xml, GameStreamError};

/// GameStream's plain HTTP port.
pub const HTTP_PORT: u16 = 47989;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Pairing waits on the person typing the PIN on the host.
const PAIR_TIMEOUT: Duration = Duration::from_secs(120);
/// Launching can start a game first.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);

/// What `serverinfo` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub hostname: String,
    pub app_version: String,
    pub unique_id: String,
    pub https_port: u16,
    pub paired: bool,
    /// The running app's ID, 0 for none.
    pub current_game: u32,
    /// `ServerCodecModeSupport` bits (H.264 1, HEVC 0x100, AV1 0x10000...).
    pub codec_modes: u32,
}

/// One app the host offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: u32,
    pub title: String,
    pub hdr: bool,
}

/// What a launch asks for.
#[derive(Debug, Clone)]
pub struct Launch {
    pub app_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The AES key and key id input and control are encrypted with.
    pub input_key: [u8; 16],
    pub input_key_id: u32,
    /// Play the sound on the host too.
    pub host_audio: bool,
    /// 196610: stereo (two channels, mask 0x3).
    pub surround_audio_info: u32,
    pub gamepads: u32,
}

/// A client: its credentials and the unique ID it gives hosts.
pub struct GameStreamClient {
    credentials: Credentials,
    unique_id: String,
}

/// One host as the client reaches it.
#[derive(Debug, Clone)]
pub struct Host {
    pub address: String,
    pub http_port: u16,
    pub https_port: u16,
    /// The host's certificate from pairing, DER; pinned for HTTPS.
    pub cert: Option<Vec<u8>>,
}

impl GameStreamClient {
    pub fn new(credentials: Credentials) -> Self {
        // Stable per client: from its certificate.
        let digest = Sha256::digest(&credentials.cert_der);
        let unique_id = xml::encode_hex(&digest[..8]).to_uppercase();
        GameStreamClient {
            credentials,
            unique_id,
        }
    }

    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    fn url(
        &self,
        scheme: &str,
        host: &Host,
        port: u16,
        path: &str,
        query: &[(&str, String)],
    ) -> String {
        let address = if host.address.contains(':') && !host.address.starts_with('[') {
            format!("[{}]", host.address)
        } else {
            host.address.clone()
        };
        let mut url = format!(
            "{scheme}://{address}:{port}/{path}?uniqueid={}&uuid={}",
            self.unique_id,
            xml::encode_hex(&crate::crypto::random::<16>())
        );
        for (key, value) in query {
            url.push('&');
            url.push_str(key);
            url.push('=');
            url.push_str(&urlencode(value));
        }
        url
    }

    fn plain(
        &self,
        host: &Host,
        path: &str,
        query: &[(&str, String)],
        timeout: Duration,
    ) -> Result<String, GameStreamError> {
        let agent = ureq::AgentBuilder::new().timeout(timeout).build();
        get(&agent, &self.url("http", host, host.http_port, path, query))
    }

    fn secure(
        &self,
        host: &Host,
        path: &str,
        query: &[(&str, String)],
        timeout: Duration,
    ) -> Result<String, GameStreamError> {
        let cert = host.cert.clone().ok_or(GameStreamError::NotPaired)?;
        let agent = ureq::AgentBuilder::new()
            .tls_config(Arc::new(self.tls(cert)?))
            .timeout(timeout)
            .build();
        get(
            &agent,
            &self.url("https", host, host.https_port, path, query),
        )
    }

    fn tls(&self, server_cert: Vec<u8>) -> Result<rustls::ClientConfig, GameStreamError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| GameStreamError::Tls(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned {
                cert: server_cert,
                provider,
            }))
            .with_client_auth_cert(
                vec![CertificateDer::from(self.credentials.cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.credentials.key_der()?)),
            )
            .map_err(|e| GameStreamError::Tls(e.to_string()))
    }

    /// `serverinfo` over plain HTTP: no pairing needed.
    pub fn server_info(&self, host: &Host) -> Result<ServerInfo, GameStreamError> {
        let answer = self.plain(host, "serverinfo", &[], TIMEOUT)?;
        xml::ok(&answer)?;
        let number = |name: &str| xml::text(&answer, name).and_then(|v| v.parse::<i64>().ok());
        Ok(ServerInfo {
            hostname: xml::text(&answer, "hostname").unwrap_or_default(),
            app_version: xml::text(&answer, "appversion").unwrap_or_default(),
            unique_id: xml::text(&answer, "uniqueid").unwrap_or_default(),
            https_port: number("HttpsPort").map_or(47984, |p| p as u16),
            paired: number("PairStatus") == Some(1),
            current_game: number("currentgame").unwrap_or(0) as u32,
            codec_modes: number("ServerCodecModeSupport").unwrap_or(1) as u32,
        })
    }

    /// Pairs with `host` once the person types `pin` there. Returns the
    /// host with its certificate pinned.
    pub fn pair(&self, host: &Host, pin: &str, device_name: &str) -> Result<Host, GameStreamError> {
        let info = self.server_info(host)?;
        let mut host = Host {
            https_port: info.https_port,
            ..host.clone()
        };
        let requests = Requests {
            client: self,
            host: std::sync::Mutex::new(host.clone()),
        };
        let cert = pairing::pair_client(
            &requests,
            &self.credentials,
            &info.app_version,
            pin,
            device_name,
        )?;
        host.cert = Some(cert);
        Ok(host)
    }

    /// The host's apps.
    pub fn apps(&self, host: &Host) -> Result<Vec<App>, GameStreamError> {
        let answer = self.secure(host, "applist", &[], TIMEOUT)?;
        xml::ok(&answer)?;
        Ok(xml::blocks(&answer, "App")
            .into_iter()
            .filter_map(|app| {
                Some(App {
                    id: xml::text(app, "ID")?.parse().ok()?,
                    title: xml::text(app, "AppTitle").unwrap_or_default(),
                    hdr: xml::text(app, "IsHdrSupported").as_deref() == Some("1"),
                })
            })
            .collect())
    }

    /// Launches an app (or `resume` the running one); returns the RTSP
    /// session URL the stream is set up with.
    pub fn launch(
        &self,
        host: &Host,
        launch: &Launch,
        resume: bool,
    ) -> Result<String, GameStreamError> {
        let query = [
            ("appid", launch.app_id.to_string()),
            (
                "mode",
                format!("{}x{}x{}", launch.width, launch.height, launch.fps),
            ),
            ("additionalStates", "1".into()),
            ("sops", "0".into()),
            ("rikey", xml::encode_hex(&launch.input_key)),
            ("rikeyid", (launch.input_key_id as i32).to_string()),
            (
                "localAudioPlayMode",
                u8::from(launch.host_audio).to_string(),
            ),
            ("surroundAudioInfo", launch.surround_audio_info.to_string()),
            ("remoteControllersBitmap", launch.gamepads.to_string()),
            ("gcmap", launch.gamepads.to_string()),
            ("gcpersist", "0".into()),
            ("corever", "1".into()),
        ];
        let answer = self.secure(
            host,
            if resume { "resume" } else { "launch" },
            &query,
            LAUNCH_TIMEOUT,
        )?;
        xml::ok(&answer)?;
        xml::text(&answer, "sessionUrl0")
            .ok_or(GameStreamError::Pairing("the host gave no session URL"))
    }

    /// Quits the running app.
    pub fn cancel(&self, host: &Host) -> Result<(), GameStreamError> {
        let answer = self.secure(host, "cancel", &[], TIMEOUT)?;
        xml::ok(&answer)
    }
}

/// Pairing's requests through this client.
struct Requests<'a> {
    client: &'a GameStreamClient,
    host: std::sync::Mutex<Host>,
}

impl PairRequests for Requests<'_> {
    fn pair(&self, https: bool, query: &[(&str, String)]) -> Result<String, GameStreamError> {
        let host = self.host.lock().expect("host").clone();
        if https {
            // Step 5 runs against the certificate the host sent in step 1:
            // pinned from the plaincert the client verified.
            return self.client.secure(&host, "pair", query, TIMEOUT);
        }
        let answer = self.client.plain(&host, "pair", query, PAIR_TIMEOUT)?;
        if let Some(pem) = xml::hex(&answer, "plaincert").filter(|c| !c.is_empty()) {
            self.host.lock().expect("host").cert = crate::crypto::pem_to_der(&pem).ok();
        }
        Ok(answer)
    }
}

fn get(agent: &ureq::Agent, url: &str) -> Result<String, GameStreamError> {
    match agent.get(url).call() {
        Ok(response) => response
            .into_string()
            .map_err(|e| GameStreamError::Http(e.to_string())),
        // GameStream hosts answer refusals with an XML body too.
        Err(ureq::Error::Status(_, response)) => response
            .into_string()
            .map_err(|e| GameStreamError::Http(e.to_string())),
        Err(e) => Err(GameStreamError::Http(e.to_string())),
    }
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Trusts exactly one certificate: the host's, from pairing. GameStream
/// hosts' certificates are self-signed and name no host.
#[derive(Debug)]
struct Pinned {
    cert: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.cert.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "not the certificate this host paired with".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
