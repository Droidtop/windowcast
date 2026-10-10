//! The RDP host's TLS identity: a self-signed certificate made per host (a
//! windowcast client pins it by the fingerprint the host sends over the
//! paired session; other RDP clients ask their user, as they do for any
//! self-signed host).

use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::RdpError;

pub struct HostIdentity {
    pub cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

impl HostIdentity {
    pub fn generate(name: &str) -> Result<Self, RdpError> {
        let cert = |e: rcgen::Error| RdpError::Certificate(e.to_string());
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(cert)?;
        let mut params = rcgen::CertificateParams::new(vec![name.to_owned()]).map_err(cert)?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2046, 1, 1);
        let made = params.self_signed(&key).map_err(cert)?;
        Ok(HostIdentity {
            cert_der: made.der().to_vec(),
            key_der: key.serialize_der(),
        })
    }

    /// SHA-256 of the certificate, what a windowcast client pins.
    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint(&self.cert_der)
    }

    /// The certificate's public key (its SubjectPublicKey bits), which
    /// CredSSP binds the login to.
    pub fn public_key(&self) -> Result<Vec<u8>, RdpError> {
        public_key(&self.cert_der)
    }

    pub fn acceptor(&self) -> Result<tokio_rustls::TlsAcceptor, RdpError> {
        let config = rustls::ServerConfig::builder_with_provider(crate::crypto_provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| RdpError::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    self.cert_der.clone(),
                )],
                rustls::pki_types::PrivateKeyDer::Pkcs8(self.key_der.clone().into()),
            )
            .map_err(|e| RdpError::Tls(e.to_string()))?;
        Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
    }
}

pub fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    Sha256::digest(cert_der).into()
}

pub fn public_key(cert_der: &[u8]) -> Result<Vec<u8>, RdpError> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| RdpError::Certificate(e.to_string()))?;
    Ok(cert.public_key().subject_public_key.data.to_vec())
}

/// Accepts the server certificate whose fingerprint is `pinned`, or any
/// when there is none (the caller reports the fingerprint it saw).
#[derive(Debug)]
pub(crate) struct Pinned {
    pub pinned: Option<[u8; 32]>,
    pub provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self.pinned {
            Some(pin) if pin != fingerprint(end_entity) => Err(rustls::Error::General(
                "the RDP host's certificate is not the one this host was paired with".into(),
            )),
            _ => Ok(rustls::client::danger::ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
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
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
