//! GameStream pairing, both ends, as moonlight-qt (`NvPairingManager::pair`)
//! and Sunshine (`nvhttp.cpp`: `getservercert`, `clientchallenge`,
//! `serverchallengeresp`, `clientpairingsecret`) do it:
//!
//! 1. The client makes a PIN and shows it; the person types it on the
//!    host. The client sends a salt and its certificate
//!    (`phrase=getservercert`); once the PIN is in, the host answers with
//!    its certificate. Both derive the AES key from salt and PIN.
//! 2. `clientchallenge`: 16 random bytes, encrypted. The host answers with
//!    the hash of (challenge, its certificate's signature, a server
//!    secret) and a challenge of its own, encrypted.
//! 3. `serverchallengeresp`: the client hashes (the host's challenge, its
//!    own certificate's signature, a client secret), encrypted. The host
//!    answers with its secret and its signature of it; the client checks
//!    the signature (someone in the middle) and the hash from step 2 (a
//!    wrong PIN).
//! 4. `clientpairingsecret`: the client's secret and its signature of it.
//!    The host checks both against step 3 and the client's certificate,
//!    and pins the certificate.
//! 5. `phrase=pairchallenge` over HTTPS with the client certificate proves
//!    the TLS side works.

use crate::crypto::{self, Credentials, Hash};
use crate::xml;
use crate::GameStreamError;

/// How a client reaches the host's pairing endpoint: plain HTTP or HTTPS
/// (with its certificate), `/pair` with these query parameters, the
/// response XML back. Long-polls on the first request until the person
/// has typed the PIN on the host.
pub trait PairRequests {
    fn pair(&self, https: bool, query: &[(&str, String)]) -> Result<String, GameStreamError>;
}

/// A 4-digit PIN for the person to type on the host.
pub fn new_pin() -> String {
    let n = u32::from_le_bytes(crypto::random::<4>()) % 10_000;
    format!("{n:04}")
}

/// Client side. Returns the host's certificate (DER), to pin for HTTPS.
/// `app_version` is the host's `serverinfo` `appversion`.
pub fn pair_client(
    requests: &dyn PairRequests,
    credentials: &Credentials,
    app_version: &str,
    pin: &str,
    device_name: &str,
) -> Result<Vec<u8>, GameStreamError> {
    let hash = Hash::for_app_version(app_version);
    let salt: [u8; 16] = crypto::random();
    let key = crypto::pin_key(hash, &salt, pin);
    let base = |extra: (&'static str, String)| {
        vec![
            ("devicename", device_name.to_owned()),
            ("updateState", "1".to_owned()),
            extra,
        ]
    };
    let unpair = || {
        let _ = requests.pair(false, &[("phrase", "unpair".to_owned())]);
    };

    // 1. The host's certificate, once the PIN is typed there.
    let mut query = base(("phrase", "getservercert".into()));
    query.push(("salt", xml::encode_hex(&salt)));
    query.push((
        "clientcert",
        xml::encode_hex(credentials.cert_pem.as_bytes()),
    ));
    let answer = requests.pair(false, &query)?;
    xml::ok(&answer)?;
    if xml::text(&answer, "paired").as_deref() != Some("1") {
        return Err(GameStreamError::Pairing("the host refused to pair"));
    }
    let server_pem = xml::hex(&answer, "plaincert")
        .filter(|c| !c.is_empty())
        .ok_or(GameStreamError::Pairing(
            "the host is already pairing with someone",
        ))?;
    let server_der = crypto::pem_to_der(&server_pem)?;

    // 2. Our challenge.
    let challenge: [u8; 16] = crypto::random();
    let answer = requests.pair(
        false,
        &base((
            "clientchallenge",
            xml::encode_hex(&crypto::ecb_encrypt(&key, &challenge)),
        )),
    )?;
    if xml::text(&answer, "paired").as_deref() != Some("1") {
        unpair();
        return Err(GameStreamError::Pairing("the host refused our challenge"));
    }
    let response = crypto::ecb_decrypt(
        &key,
        &xml::hex(&answer, "challengeresponse").unwrap_or_default(),
    );
    if response.len() < hash.output_len() + 16 {
        unpair();
        return Err(GameStreamError::Pairing(
            "the host's challenge response is short",
        ));
    }
    let server_response = &response[..hash.output_len()];
    let server_challenge = &response[hash.output_len()..hash.output_len() + 16];

    // 3. Our answer to the host's challenge.
    let client_secret: [u8; 16] = crypto::random();
    let mut ours = server_challenge.to_vec();
    ours.extend_from_slice(&credentials.signature());
    ours.extend_from_slice(&client_secret);
    let mut padded = hash.digest(&ours);
    padded.resize(32, 0);
    let answer = requests.pair(
        false,
        &base((
            "serverchallengeresp",
            xml::encode_hex(&crypto::ecb_encrypt(&key, &padded)),
        )),
    )?;
    if xml::text(&answer, "paired").as_deref() != Some("1") {
        unpair();
        return Err(GameStreamError::Pairing("the host refused our answer"));
    }
    let pairing_secret = xml::hex(&answer, "pairingsecret").unwrap_or_default();
    if pairing_secret.len() <= 16 {
        unpair();
        return Err(GameStreamError::Pairing(
            "the host's pairing secret is short",
        ));
    }
    let (server_secret, server_signature) = pairing_secret.split_at(16);
    if !crypto::verify(&server_der, server_secret, server_signature) {
        unpair();
        return Err(GameStreamError::Pairing(
            "the host's secret is not signed by its certificate: someone in the middle",
        ));
    }
    let mut expected = challenge.to_vec();
    expected.extend_from_slice(&crypto::certificate_signature(&server_der)?);
    expected.extend_from_slice(server_secret);
    if hash.digest(&expected) != server_response {
        unpair();
        return Err(GameStreamError::WrongPin);
    }

    // 4. Our secret, signed.
    let mut secret = client_secret.to_vec();
    secret.extend_from_slice(&credentials.sign(&client_secret));
    let answer = requests.pair(
        false,
        &base(("clientpairingsecret", xml::encode_hex(&secret))),
    )?;
    if xml::text(&answer, "paired").as_deref() != Some("1") {
        unpair();
        return Err(GameStreamError::Pairing(
            "the host did not accept our secret",
        ));
    }

    // 5. Over HTTPS with our certificate.
    let answer = requests.pair(true, &base(("phrase", "pairchallenge".into())))?;
    if xml::text(&answer, "paired").as_deref() != Some("1") {
        unpair();
        return Err(GameStreamError::Pairing(
            "the host's HTTPS pairing check failed",
        ));
    }
    Ok(server_der)
}

/// Where a host's pairing with one client stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The client sent `getservercert`; waiting for the person's PIN.
    WaitingForPin,
    ServerCert,
    ClientChallenge,
    ServerChallengeResponse,
}

/// Host side: one client's pairing, phase by phase, as Sunshine checks it.
pub struct HostPairing {
    phase: Phase,
    salt: [u8; 16],
    /// The client's certificate, DER.
    pub client_cert: Vec<u8>,
    pub device_name: String,
    key: Option<[u8; 16]>,
    server_secret: [u8; 16],
    server_challenge: [u8; 16],
    client_hash: Vec<u8>,
}

/// What a host pairing step answers: the XML, and the client's certificate
/// once the client is paired.
pub struct Step {
    pub xml: String,
    pub paired: Option<Vec<u8>>,
    /// The pairing is over (paired or failed); forget it.
    pub done: bool,
}

fn fail(message: &str) -> Step {
    Step {
        xml: xml::response(400, Some(message), &[("paired", "0".into())]),
        paired: None,
        done: true,
    }
}

impl HostPairing {
    /// `getservercert` arrived: hold it until the PIN is typed.
    pub fn start(
        salt_hex: &str,
        client_cert_hex: &str,
        device_name: &str,
    ) -> Result<Self, &'static str> {
        let salt = xml::decode_hex(salt_hex)
            .filter(|s| s.len() >= 16)
            .ok_or("Salt too short")?;
        let pem = xml::decode_hex(client_cert_hex).ok_or("Invalid client certificate")?;
        let client_cert = crypto::pem_to_der(&pem).map_err(|_| "Invalid client certificate")?;
        Ok(HostPairing {
            phase: Phase::WaitingForPin,
            salt: salt[..16].try_into().expect("16 bytes"),
            client_cert,
            device_name: device_name.to_owned(),
            key: None,
            server_secret: [0; 16],
            server_challenge: [0; 16],
            client_hash: Vec::new(),
        })
    }

    /// The person typed `pin`: the answer to the held `getservercert`.
    pub fn pin(&mut self, pin: &str, server: &Credentials) -> String {
        self.key = Some(crypto::pin_key(Hash::Sha256, &self.salt, pin));
        self.phase = Phase::ServerCert;
        xml::response(
            200,
            None,
            &[
                ("paired", "1".into()),
                ("plaincert", xml::encode_hex(server.cert_pem.as_bytes())),
            ],
        )
    }

    /// The next request of this client's pairing.
    pub fn step(&mut self, query: &[(String, String)], server: &Credentials) -> Step {
        let arg = |name: &str| {
            query
                .iter()
                .find(|(k, _)| k == name)
                .and_then(|(_, v)| xml::decode_hex(v))
        };
        let Some(key) = self.key else {
            return fail("Pairing not started");
        };
        if let Some(challenge) = arg("clientchallenge") {
            if self.phase != Phase::ServerCert {
                return fail("Out of order call to clientchallenge");
            }
            self.phase = Phase::ClientChallenge;
            let mut decrypted = crypto::ecb_decrypt(&key, &challenge);
            decrypted.extend_from_slice(&server.signature());
            self.server_secret = crypto::random();
            decrypted.extend_from_slice(&self.server_secret);
            let mut plain = Hash::Sha256.digest(&decrypted);
            self.server_challenge = crypto::random();
            plain.extend_from_slice(&self.server_challenge);
            return Step {
                xml: xml::response(
                    200,
                    None,
                    &[
                        ("paired", "1".into()),
                        (
                            "challengeresponse",
                            xml::encode_hex(&crypto::ecb_encrypt(&key, &plain)),
                        ),
                    ],
                ),
                paired: None,
                done: false,
            };
        }
        if let Some(response) = arg("serverchallengeresp") {
            if self.phase != Phase::ClientChallenge {
                return fail("Out of order call to serverchallengeresp");
            }
            self.phase = Phase::ServerChallengeResponse;
            self.client_hash = crypto::ecb_decrypt(&key, &response);
            let mut secret = self.server_secret.to_vec();
            secret.extend_from_slice(&server.sign(&self.server_secret));
            return Step {
                xml: xml::response(
                    200,
                    None,
                    &[
                        ("pairingsecret", xml::encode_hex(&secret)),
                        ("paired", "1".into()),
                    ],
                ),
                paired: None,
                done: false,
            };
        }
        if let Some(secret) = arg("clientpairingsecret") {
            if self.phase != Phase::ServerChallengeResponse {
                return fail("Out of order call to clientpairingsecret");
            }
            if secret.len() <= 16 {
                return fail("Client pairing secret too short");
            }
            let (secret, signature) = secret.split_at(16);
            let Ok(client_signature) = crypto::certificate_signature(&self.client_cert) else {
                return fail("Invalid client certificate");
            };
            let mut data = self.server_challenge.to_vec();
            data.extend_from_slice(&client_signature);
            data.extend_from_slice(secret);
            // The client pads its hash to 32 bytes.
            let same =
                Hash::Sha256.digest(&data) == self.client_hash[..self.client_hash.len().min(32)];
            let signed = crypto::verify(&self.client_cert, secret, signature);
            let paired = same && signed;
            return Step {
                xml: xml::response(
                    200,
                    None,
                    &[("paired", if paired { "1" } else { "0" }.into())],
                ),
                paired: paired.then(|| self.client_cert.clone()),
                done: true,
            };
        }
        fail("Invalid pairing request")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A host in memory: the client's requests go straight to its pairing.
    struct InMemory {
        server: Credentials,
        pin: String,
        pairing: Mutex<Option<HostPairing>>,
        paired: Mutex<Option<Vec<u8>>>,
    }

    impl PairRequests for InMemory {
        fn pair(&self, https: bool, query: &[(&str, String)]) -> Result<String, GameStreamError> {
            let query: Vec<(String, String)> = query
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            let get = |name: &str| {
                query
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            };
            if https {
                return Ok(xml::response(200, None, &[("paired", "1".into())]));
            }
            if get("phrase").as_deref() == Some("getservercert") {
                let mut pairing = HostPairing::start(
                    &get("salt").unwrap(),
                    &get("clientcert").unwrap(),
                    &get("devicename").unwrap(),
                )
                .unwrap();
                // The person types the PIN on the host.
                let answer = pairing.pin(&self.pin, &self.server);
                *self.pairing.lock().unwrap() = Some(pairing);
                return Ok(answer);
            }
            let mut pairing = self.pairing.lock().unwrap();
            let step = pairing.as_mut().unwrap().step(&query, &self.server);
            if let Some(cert) = step.paired {
                *self.paired.lock().unwrap() = Some(cert);
            }
            Ok(step.xml)
        }
    }

    #[test]
    fn our_client_pairs_with_our_host_and_a_wrong_pin_fails() {
        let client = Credentials::generate("NVIDIA GameStream Client").unwrap();
        let host = InMemory {
            server: Credentials::generate("windowcast GameStream Host").unwrap(),
            pin: "4821".into(),
            pairing: Mutex::default(),
            paired: Mutex::default(),
        };
        let server_der = pair_client(&host, &client, "7.1.431.-1", "4821", "test").unwrap();
        assert_eq!(server_der, host.server.cert_der);
        assert_eq!(
            host.paired.lock().unwrap().as_deref(),
            Some(&client.cert_der[..])
        );

        *host.paired.lock().unwrap() = None;
        let wrong = pair_client(&host, &client, "7.1.431.-1", "0000", "test");
        assert!(matches!(wrong, Err(GameStreamError::WrongPin)), "{wrong:?}");
        assert!(host.paired.lock().unwrap().is_none());
    }
}
