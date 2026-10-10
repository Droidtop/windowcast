//! OpenID Connect: the client signing in with a provider, and the host
//! checking the ID token it presents.
//!
//! Client: the authorization code flow with PKCE (RFC 7636), redirected
//! to a loopback port the client listens on (RFC 8252), for a device with
//! a browser ([`BrowserSignIn`]). The redirect names `localhost`, the form
//! providers accept for native apps without registering a port (Dex takes
//! no other), and the client listens on that port on both 127.0.0.1 and
//! ::1, whichever the browser resolves it to; the device authorization flow (RFC 8628)
//! for one without ([`DeviceSignIn`]). Both end in an ID token. The client
//! is a public client: it has an id and no secret.
//!
//! Host: [`Verifier`] fetches the provider's discovery document and keys,
//! and checks the token's signature (asymmetric algorithms only), issuer,
//! audience, expiry and, when the token carries one, a nonce bound to the
//! presenting device's key ([`nonce_for`]); each token registers one
//! device only.
//!
//! Providers are reached over https; plain http only on loopback, for a
//! provider running on the same machine (tests).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windowcast_identity::PeerId;
use windowcast_protocol::OidcProviderInfo;

use crate::{Account, CheckError, Method};

const NONCE_LABEL: &[u8] = b"windowcast-oidc-nonce-v1\0";
const NONCE_PREFIX: &str = "wc1.";

/// One provider a host accepts, as its configuration names it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// What clients and policy rules (`provider:<name>`) call it.
    pub name: String,
    /// The issuer URL, exactly as the provider's tokens carry it.
    pub issuer: String,
    /// The client id windowcast is registered under at the provider; the
    /// ID tokens' audience.
    pub client_id: String,
    /// Scopes to ask for besides `openid`.
    pub scopes: Vec<String>,
    /// Claims to take the user's name from, the first present winning.
    pub username_claims: Vec<String>,
    /// The claim holding the user's groups (a list of strings).
    pub groups_claim: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        ProviderConfig {
            name: String::new(),
            issuer: String::new(),
            client_id: String::new(),
            scopes: vec!["profile".into(), "email".into()],
            username_claims: vec!["preferred_username".into(), "email".into(), "sub".into()],
            groups_claim: "groups".into(),
        }
    }
}

impl ProviderConfig {
    /// What the host tells clients about it.
    pub fn info(&self) -> OidcProviderInfo {
        OidcProviderInfo {
            name: self.name.clone(),
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            scopes: self.scopes.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("{0}: only https, or http on loopback")]
    Insecure(String),
    #[error("could not reach the provider: {0}")]
    Http(String),
    #[error("the provider's answer is not what was expected: {0}")]
    Malformed(String),
    #[error("the provider refused: {0}")]
    Refused(String),
    #[error("the sign-in was not finished in time")]
    TimedOut,
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// The parts of a provider's discovery document windowcast uses.
#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
}

/// Fetches `issuer`'s discovery document and checks it names the same
/// issuer.
pub fn discover(issuer: &str) -> Result<Metadata, OidcError> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let metadata: Metadata = get_json(&url)?;
    if metadata.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
        return Err(OidcError::Malformed(format!(
            "the discovery document names issuer {}",
            metadata.issuer
        )));
    }
    for endpoint in [
        &metadata.authorization_endpoint,
        &metadata.token_endpoint,
        &metadata.jwks_uri,
    ] {
        secure(endpoint)?;
    }
    Ok(metadata)
}

/// The nonce a device asks the provider to put in its ID token:
/// `wc1.<salt>.<SHA-256 of a label, the device key and the salt>`, so a
/// host can tell the token was asked for by the device presenting it.
pub fn nonce_for(device: &PeerId) -> String {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    nonce_with(device, &salt)
}

fn nonce_with(device: &PeerId, salt: &[u8]) -> String {
    let digest = Sha256::new()
        .chain_update(NONCE_LABEL)
        .chain_update(device.0)
        .chain_update(salt)
        .finalize();
    format!(
        "{NONCE_PREFIX}{}.{}",
        URL_SAFE_NO_PAD.encode(salt),
        URL_SAFE_NO_PAD.encode(digest)
    )
}

/// Whether `nonce` was made by [`nonce_for`] for `device`.
fn nonce_matches(nonce: &str, device: &PeerId) -> bool {
    let Some(rest) = nonce.strip_prefix(NONCE_PREFIX) else {
        return false;
    };
    let Some((salt, _)) = rest.split_once('.') else {
        return false;
    };
    match URL_SAFE_NO_PAD.decode(salt) {
        Ok(salt) => nonce_with(device, &salt) == nonce,
        Err(_) => false,
    }
}

/// The authorization code flow with PKCE in the user's browser: open
/// [`Self::url`], then [`Self::finish`] waits for the provider to send the
/// browser back to this client's loopback port and exchanges the code.
pub struct BrowserSignIn {
    url: String,
    listeners: Vec<TcpListener>,
    redirect_uri: String,
    verifier: String,
    state: String,
    token_endpoint: String,
    client_id: String,
}

impl BrowserSignIn {
    /// Starts a sign-in with `provider` for `device`, whose key the ID
    /// token's nonce is bound to.
    pub fn start(provider: &OidcProviderInfo, device: &PeerId) -> Result<Self, OidcError> {
        let metadata = discover(&provider.issuer)?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let mut listeners = vec![listener];
        // Best effort: a machine without IPv6 has only the first.
        if let Ok(v6) = TcpListener::bind(("::1", port)) {
            listeners.push(v6);
        }
        let redirect_uri = format!("http://localhost:{port}/callback");
        let verifier = random_text(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_text(16);
        let mut url = url::Url::parse(&metadata.authorization_endpoint)
            .map_err(|e| OidcError::Malformed(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &provider.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", &scopes(provider))
            .append_pair("state", &state)
            .append_pair("nonce", &nonce_for(device))
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(BrowserSignIn {
            url: url.into(),
            listeners,
            redirect_uri,
            verifier,
            state,
            token_endpoint: metadata.token_endpoint,
            client_id: provider.client_id.clone(),
        })
    }

    /// The page to open in the user's browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits up to `within` for the browser to come back, and returns the
    /// ID token.
    pub fn finish(self, within: Duration) -> Result<String, OidcError> {
        let deadline = Instant::now() + within;
        for listener in &self.listeners {
            listener.set_nonblocking(true)?;
        }
        loop {
            let mut accepted = None;
            for listener in &self.listeners {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted = Some(stream);
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let Some(mut stream) = accepted else {
                if Instant::now() >= deadline {
                    return Err(OidcError::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line)?;
            // "GET /callback?code=...&state=... HTTP/1.1"
            let Some(target) = line.split_whitespace().nth(1) else {
                continue;
            };
            let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else {
                continue;
            };
            if url.path() != "/callback" {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                continue;
            }
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
            let page = |text: &str| {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                )
            };
            if query.get("state") != Some(&self.state) {
                let _ =
                    stream.write_all(page("<p>This sign-in was not started here.</p>").as_bytes());
                continue;
            }
            if let Some(error) = query.get("error") {
                let _ = stream
                    .write_all(page("<p>Sign-in refused. You can close this page.</p>").as_bytes());
                return Err(OidcError::Refused(error.clone()));
            }
            let Some(code) = query.get("code") else {
                continue;
            };
            let _ = stream.write_all(
                page("<p>Signed in. You can close this page and return to windowcast.</p>")
                    .as_bytes(),
            );
            let response = post_form(
                &self.token_endpoint,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", &self.redirect_uri),
                    ("client_id", &self.client_id),
                    ("code_verifier", &self.verifier),
                ],
            )?;
            return id_token_from(response);
        }
    }
}

/// The device authorization flow: show [`Self::user_code`] and
/// [`Self::verification_uri`] to the user, who finishes on another device;
/// [`Self::finish`] polls until they have, or [`Self::wait`] for a while
/// at a time.
pub struct DeviceSignIn {
    pub user_code: String,
    pub verification_uri: String,
    /// The same page with the code filled in, when the provider gives one.
    pub verification_uri_complete: Option<String>,
    device_code: String,
    interval: Duration,
    /// When the provider may be asked again.
    next_poll: Instant,
    expires: Instant,
    token_endpoint: String,
    client_id: String,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_url")]
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

impl DeviceSignIn {
    pub fn start(provider: &OidcProviderInfo) -> Result<Self, OidcError> {
        let metadata = discover(&provider.issuer)?;
        let endpoint = metadata.device_authorization_endpoint.ok_or_else(|| {
            OidcError::Malformed("the provider has no device authorization endpoint".into())
        })?;
        secure(&endpoint)?;
        let response = post_form(
            &endpoint,
            &[
                ("client_id", &provider.client_id),
                ("scope", &scopes(provider)),
            ],
        )?;
        let authorization: DeviceAuthorization =
            serde_json::from_value(response).map_err(|e| OidcError::Malformed(e.to_string()))?;
        let interval = Duration::from_secs(authorization.interval.unwrap_or(5).max(1));
        Ok(DeviceSignIn {
            user_code: authorization.user_code,
            verification_uri: authorization.verification_uri,
            verification_uri_complete: authorization.verification_uri_complete,
            device_code: authorization.device_code,
            interval,
            next_poll: Instant::now() + interval,
            expires: Instant::now() + Duration::from_secs(authorization.expires_in),
            token_endpoint: metadata.token_endpoint,
            client_id: provider.client_id.clone(),
        })
    }

    /// Polls until the user has signed in (the ID token), refused, or the
    /// code expired.
    pub fn finish(mut self) -> Result<String, OidcError> {
        loop {
            if let Some(id_token) = self.wait(Duration::from_secs(60))? {
                return Ok(id_token);
            }
        }
    }

    /// Polls for up to `within` (at the pace the provider asked for):
    /// the ID token once the user has signed in, `None` if they have not
    /// yet (call again), an error if they refused or the code expired.
    pub fn wait(&mut self, within: Duration) -> Result<Option<String>, OidcError> {
        let deadline = Instant::now() + within;
        loop {
            if Instant::now() >= self.expires {
                return Err(OidcError::TimedOut);
            }
            let now = Instant::now();
            if self.next_poll > now {
                if self.next_poll > deadline {
                    std::thread::sleep(deadline.saturating_duration_since(now));
                    return Ok(None);
                }
                std::thread::sleep(self.next_poll - now);
            }
            self.next_poll = Instant::now() + self.interval;
            match post_form(
                &self.token_endpoint,
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", &self.device_code),
                    ("client_id", &self.client_id),
                ],
            ) {
                Ok(response) => return id_token_from(response).map(Some),
                Err(OidcError::Refused(error)) if error == "authorization_pending" => {}
                Err(OidcError::Refused(error)) if error == "slow_down" => {
                    self.interval += Duration::from_secs(5);
                    self.next_poll = Instant::now() + self.interval;
                }
                Err(e) => return Err(e),
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
}

/// A host's check of one provider's ID tokens.
pub struct Verifier {
    config: ProviderConfig,
    keys: Mutex<Option<(JwkSet, Instant)>>,
    /// Tokens already used, by hash, with their expiry.
    used: Mutex<HashMap<[u8; 32], u64>>,
}

/// Keys are fetched again when a token names one not in the set, at most
/// this often.
const KEY_REFRESH: Duration = Duration::from_secs(60);

const ALGORITHMS: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

impl Verifier {
    pub fn new(config: ProviderConfig) -> Self {
        Verifier {
            config,
            keys: Mutex::new(None),
            used: Mutex::new(HashMap::new()),
        }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Checks `id_token` presented by `device` and gives the account.
    pub fn verify(&self, id_token: &str, device: &PeerId) -> Result<Account, CheckError> {
        let refused = |why: String| CheckError::Refused(format!("{}: {why}", self.config.name));
        let header = jsonwebtoken::decode_header(id_token).map_err(|e| refused(e.to_string()))?;
        if !ALGORITHMS.contains(&header.alg) {
            return Err(refused(format!(
                "algorithm {:?} is not accepted",
                header.alg
            )));
        }
        let key = self.key(header.kid.as_deref())?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[self.config.issuer.as_str()]);
        validation.set_audience(&[self.config.client_id.as_str()]);
        validation.leeway = 60;
        let claims: serde_json::Value =
            jsonwebtoken::decode::<serde_json::Value>(id_token, &key, &validation)
                .map_err(|e| refused(e.to_string()))?
                .claims;

        if let Some(nonce) = claims.get("nonce").and_then(|n| n.as_str()) {
            if !nonce_matches(nonce, device) {
                return Err(refused("the nonce is not this device's".into()));
            }
        }
        let expires = claims.get("exp").and_then(|e| e.as_u64()).unwrap_or(0);
        self.use_once(id_token, expires)
            .map_err(|_| refused("this token was used before".into()))?;

        let name = self
            .config
            .username_claims
            .iter()
            .find_map(|claim| claims.get(claim).and_then(|v| v.as_str()))
            .ok_or_else(|| refused("the token names no user".into()))?
            .to_owned();
        let groups = match claims.get(&self.config.groups_claim) {
            Some(serde_json::Value::Array(groups)) => groups
                .iter()
                .filter_map(|g| g.as_str().map(str::to_owned))
                .collect(),
            Some(serde_json::Value::String(group)) => vec![group.clone()],
            _ => Vec::new(),
        };
        Ok(Account {
            name,
            groups,
            method: Method::Oidc,
            provider: self.config.name.clone(),
        })
    }

    fn key(&self, kid: Option<&str>) -> Result<DecodingKey, CheckError> {
        let mut keys = self.keys.lock().expect("provider keys");
        let find = |set: &JwkSet| match kid {
            Some(kid) => set.find(kid).cloned(),
            None if set.keys.len() == 1 => set.keys.first().cloned(),
            None => None,
        };
        if let Some(jwk) = keys.as_ref().and_then(|(set, _)| find(set)) {
            return DecodingKey::from_jwk(&jwk).map_err(|e| CheckError::Refused(e.to_string()));
        }
        if keys
            .as_ref()
            .is_some_and(|(_, fetched)| fetched.elapsed() < KEY_REFRESH)
        {
            return Err(CheckError::Refused("the token's key is unknown".into()));
        }
        let unavailable = |e: OidcError| CheckError::Unavailable(e.to_string());
        let metadata = discover(&self.config.issuer).map_err(unavailable)?;
        let set: JwkSet = get_json(&metadata.jwks_uri).map_err(unavailable)?;
        let jwk = find(&set);
        *keys = Some((set, Instant::now()));
        let jwk = jwk.ok_or_else(|| CheckError::Refused("the token's key is unknown".into()))?;
        DecodingKey::from_jwk(&jwk).map_err(|e| CheckError::Refused(e.to_string()))
    }

    fn use_once(&self, id_token: &str, expires: u64) -> Result<(), ()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let hash: [u8; 32] = Sha256::digest(id_token.as_bytes()).into();
        let mut used = self.used.lock().expect("used tokens");
        used.retain(|_, until| *until + 60 > now);
        if used.contains_key(&hash) {
            return Err(());
        }
        used.insert(hash, expires);
        Ok(())
    }
}

fn scopes(provider: &OidcProviderInfo) -> String {
    std::iter::once("openid")
        .chain(
            provider
                .scopes
                .iter()
                .map(String::as_str)
                .filter(|s| *s != "openid"),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

fn random_text(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

/// https anywhere; http only to a loopback address.
fn secure(endpoint: &str) -> Result<(), OidcError> {
    let url = url::Url::parse(endpoint).map_err(|e| OidcError::Malformed(e.to_string()))?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name == "localhost",
        None => false,
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(OidcError::Insecure(endpoint.to_owned())),
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build()
}

fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, OidcError> {
    secure(url)?;
    agent()
        .get(url)
        .call()
        .map_err(|e| OidcError::Http(e.to_string()))?
        .into_json()
        .map_err(|e| OidcError::Malformed(e.to_string()))
}

/// Posts a form; an OAuth error answer (`{"error": ...}`, status 400 or
/// 401) is [`OidcError::Refused`] with its code.
fn post_form(url: &str, form: &[(&str, &str)]) -> Result<serde_json::Value, OidcError> {
    secure(url)?;
    match agent().post(url).send_form(form) {
        Ok(response) => response
            .into_json()
            .map_err(|e| OidcError::Malformed(e.to_string())),
        Err(ureq::Error::Status(status, response)) => {
            let body: serde_json::Value = response.into_json().unwrap_or_default();
            match body.get("error").and_then(|e| e.as_str()) {
                Some(error) => Err(OidcError::Refused(error.to_owned())),
                None => Err(OidcError::Http(format!("status {status}"))),
            }
        }
        Err(e) => Err(OidcError::Http(e.to_string())),
    }
}

fn id_token_from(response: serde_json::Value) -> Result<String, OidcError> {
    response
        .get("id_token")
        .and_then(|t| t.as_str())
        .map(str::to_owned)
        .ok_or_else(|| OidcError::Malformed("the answer has no ID token".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_is_bound_to_the_device() {
        let device = PeerId([3; 32]);
        let nonce = nonce_for(&device);
        assert!(nonce.starts_with("wc1."));
        assert!(nonce_matches(&nonce, &device));
        assert!(!nonce_matches(&nonce, &PeerId([4; 32])));
        assert!(!nonce_matches("someone else's nonce", &device));
        assert_ne!(nonce, nonce_for(&device));
    }

    #[test]
    fn only_https_or_loopback() {
        assert!(secure("https://id.example.org/token").is_ok());
        assert!(secure("http://127.0.0.1:5556/dex/token").is_ok());
        assert!(secure("http://localhost:8080/").is_ok());
        assert!(secure("http://[::1]:8080/").is_ok());
        assert!(secure("http://id.example.org/token").is_err());
        assert!(secure("ftp://127.0.0.1/").is_err());
    }

    #[test]
    fn scopes_start_with_openid_once() {
        let info = OidcProviderInfo {
            name: "corp".into(),
            issuer: String::new(),
            client_id: String::new(),
            scopes: vec!["openid".into(), "email".into(), "groups".into()],
        };
        assert_eq!(scopes(&info), "openid email groups");
    }

    #[test]
    fn a_token_is_used_once() {
        let verifier = Verifier::new(ProviderConfig::default());
        let later = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        assert!(verifier.use_once("token", later).is_ok());
        assert!(verifier.use_once("token", later).is_err());
        assert!(verifier.use_once("another", later).is_ok());
    }
}
