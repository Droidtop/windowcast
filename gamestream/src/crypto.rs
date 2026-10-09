//! The cryptography GameStream pairing uses, as Moonlight and Sunshine use
//! it (moonlight-qt `nvpairingmanager.cpp`, Sunshine `nvhttp.cpp` and
//! `crypto.cpp`): a PIN salted and hashed into an AES-128 key used in ECB
//! mode without padding, SHA-256 (SHA-1 before GameStream generation 7),
//! RSA-2048 PKCS#1 v1.5 signatures with SHA-256, and self-signed X.509
//! certificates whose raw signature bytes go into the challenge hashes.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use rand_core::{OsRng, RngCore};
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::RsaPrivateKey;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::GameStreamError;

/// Random bytes.
pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// The hash a server generation uses: SHA-256 from generation 7, SHA-1
/// before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha256,
}

impl Hash {
    /// The hash for a server's `appversion` (`7.1.431.-1`).
    pub fn for_app_version(version: &str) -> Hash {
        let major: i64 = version
            .split('.')
            .next()
            .and_then(|m| m.trim().parse().ok())
            .unwrap_or(7);
        if major >= 7 {
            Hash::Sha256
        } else {
            Hash::Sha1
        }
    }

    pub fn output_len(self) -> usize {
        match self {
            Hash::Sha1 => 20,
            Hash::Sha256 => 32,
        }
    }

    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => Sha1::digest(data).to_vec(),
            Hash::Sha256 => Sha256::digest(data).to_vec(),
        }
    }
}

/// The AES key a PIN and salt make: the hash of salt then PIN, first 16
/// bytes.
pub fn pin_key(hash: Hash, salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let mut salted = salt.to_vec();
    salted.extend_from_slice(pin.as_bytes());
    let digest = hash.digest(&salted);
    digest[..16].try_into().expect("16 bytes")
}

/// AES-128-ECB without padding over whole blocks (a partial last block is
/// left out, as OpenSSL does with padding off).
pub fn ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    data.as_chunks::<16>()
        .0
        .iter()
        .flat_map(|block| {
            let mut block = aes::Block::clone_from_slice(block);
            cipher.encrypt_block(&mut block);
            block.to_vec()
        })
        .collect()
}

pub fn ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    data.as_chunks::<16>()
        .0
        .iter()
        .flat_map(|block| {
            let mut block = aes::Block::clone_from_slice(block);
            cipher.decrypt_block(&mut block);
            block.to_vec()
        })
        .collect()
}

/// A certificate in DER from its PEM text.
pub fn pem_to_der(pem: &[u8]) -> Result<Vec<u8>, GameStreamError> {
    let (_, parsed) = x509_parser::pem::parse_x509_pem(pem)
        .map_err(|e| GameStreamError::Certificate(e.to_string()))?;
    Ok(parsed.contents)
}

/// The raw signature bytes of a certificate (DER).
pub fn certificate_signature(der: &[u8]) -> Result<Vec<u8>, GameStreamError> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| GameStreamError::Certificate(e.to_string()))?;
    Ok(cert.signature_value.data.to_vec())
}

/// Whether `signature` is the certificate's key's RSA PKCS#1 v1.5 SHA-256
/// signature of `data`.
pub fn verify(cert_der: &[u8], data: &[u8], signature: &[u8]) -> bool {
    use rsa::pkcs1::DecodeRsaPublicKey;
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(cert_der) else {
        return false;
    };
    let Ok(key) = rsa::RsaPublicKey::from_pkcs1_der(&cert.public_key().subject_public_key.data)
    else {
        return false;
    };
    let Ok(signature) = Signature::try_from(signature) else {
        return false;
    };
    VerifyingKey::<Sha256>::new(key)
        .verify(data, &signature)
        .is_ok()
}

/// An RSA-2048 key and the self-signed certificate made from it: what a
/// GameStream client presents (and pins on the host), and what a host
/// serves its HTTPS with.
#[derive(Clone)]
pub struct Credentials {
    key: RsaPrivateKey,
    /// The certificate, PEM.
    pub cert_pem: String,
    /// The certificate, DER.
    pub cert_der: Vec<u8>,
}

impl Credentials {
    /// A new key and certificate with this common name (Moonlight's is
    /// "NVIDIA GameStream Client").
    pub fn generate(common_name: &str) -> Result<Self, GameStreamError> {
        let key = RsaPrivateKey::new(&mut OsRng, 2048)
            .map_err(|e| GameStreamError::Certificate(e.to_string()))?;
        Self::with_key(key, common_name)
    }

    fn with_key(key: RsaPrivateKey, common_name: &str) -> Result<Self, GameStreamError> {
        let pkcs8 = key
            .to_pkcs8_der()
            .map_err(|e| GameStreamError::Certificate(e.to_string()))?;
        let pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
            &rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8.as_bytes()),
            &rcgen::PKCS_RSA_SHA256,
        )
        .map_err(|e| GameStreamError::Certificate(e.to_string()))?;
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
            .map_err(|e| GameStreamError::Certificate(e.to_string()))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        params.serial_number = Some(rcgen::SerialNumber::from_slice(&random::<8>()));
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2046, 1, 1);
        let cert = params
            .self_signed(&pair)
            .map_err(|e| GameStreamError::Certificate(e.to_string()))?;
        Ok(Credentials {
            key,
            cert_pem: cert.pem(),
            cert_der: cert.der().to_vec(),
        })
    }

    /// Loads credentials saved with [`Credentials::save`], or makes and
    /// saves new ones.
    pub fn load_or_generate(
        dir: &std::path::Path,
        common_name: &str,
    ) -> Result<Self, GameStreamError> {
        let (key_path, cert_path) = (
            dir.join("gamestream-key.pem"),
            dir.join("gamestream-cert.pem"),
        );
        if let (Ok(key), Ok(cert)) = (
            std::fs::read_to_string(&key_path),
            std::fs::read_to_string(&cert_path),
        ) {
            let key = RsaPrivateKey::from_pkcs8_pem(&key)
                .map_err(|e| GameStreamError::Certificate(e.to_string()))?;
            let cert_der = pem_to_der(cert.as_bytes())?;
            return Ok(Credentials {
                key,
                cert_pem: cert,
                cert_der,
            });
        }
        let made = Self::generate(common_name)?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(&key_path, made.key_pem()?)?;
        std::fs::write(&cert_path, &made.cert_pem)?;
        Ok(made)
    }

    /// The private key, PKCS#8 PEM.
    pub fn key_pem(&self) -> Result<String, GameStreamError> {
        Ok(self
            .key
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| GameStreamError::Certificate(e.to_string()))?
            .to_string())
    }

    /// The private key, PKCS#8 DER (for TLS).
    pub fn key_der(&self) -> Result<Vec<u8>, GameStreamError> {
        Ok(self
            .key
            .to_pkcs8_der()
            .map_err(|e| GameStreamError::Certificate(e.to_string()))?
            .as_bytes()
            .to_vec())
    }

    /// RSA PKCS#1 v1.5 SHA-256 signature of `data`.
    pub fn sign(&self, data: &[u8]) -> Vec<u8> {
        SigningKey::<Sha256>::new(self.key.clone())
            .sign(data)
            .to_vec()
    }

    /// This certificate's raw signature bytes.
    pub fn signature(&self) -> Vec<u8> {
        certificate_signature(&self.cert_der).expect("our own certificate parses")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecb_round_trips_whole_blocks() {
        let key = pin_key(Hash::Sha256, &[1; 16], "1234");
        let data: Vec<u8> = (0..48).collect();
        let sealed = ecb_encrypt(&key, &data);
        assert_eq!(sealed.len(), 48);
        assert_ne!(sealed, data);
        assert_eq!(ecb_decrypt(&key, &sealed), data);
    }

    #[test]
    fn the_pin_key_is_the_salted_hash() {
        // SHA-256 of 16 zero bytes then "1234", first 16 bytes.
        let mut salted = vec![0u8; 16];
        salted.extend_from_slice(b"1234");
        let full = Sha256::digest(&salted);
        assert_eq!(pin_key(Hash::Sha256, &[0; 16], "1234"), full[..16]);
        assert_eq!(Hash::for_app_version("7.1.431.-1"), Hash::Sha256);
        assert_eq!(Hash::for_app_version("6.2.0.0"), Hash::Sha1);
    }

    #[test]
    fn credentials_sign_and_their_certificate_verifies() {
        let credentials = Credentials::generate("NVIDIA GameStream Client").unwrap();
        let signature = credentials.sign(b"a secret");
        assert!(verify(&credentials.cert_der, b"a secret", &signature));
        assert!(!verify(&credentials.cert_der, b"another", &signature));
        assert_eq!(
            pem_to_der(credentials.cert_pem.as_bytes()).unwrap(),
            credentials.cert_der
        );
        assert_eq!(credentials.signature().len(), 256);
    }
}
