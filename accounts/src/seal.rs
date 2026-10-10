//! Sealing a credential to the host: HPKE (RFC 9180) in base mode with
//! DHKEM(X25519, HKDF-SHA256), HKDF-SHA256 and ChaCha20-Poly1305. The host
//! makes a fresh key pair for each sign-in and signs its public half with
//! its identity; the client seals to it with the signaling transcript as
//! HPKE's `info`, so a sealed credential opens only on that connection.

use hpke::aead::ChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem, OpModeR, OpModeS, Serializable};
use rand_core::OsRng;

type HostKem = X25519HkdfSha256;

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("malformed key")]
    Key,
    #[error("sealing failed")]
    Seal,
    /// Wrong key, wrong `info` or a modified ciphertext.
    #[error("the sealed credential does not open")]
    Open,
}

/// The host's key for one sign-in.
pub struct HostKey {
    private: <HostKem as Kem>::PrivateKey,
    public: <HostKem as Kem>::PublicKey,
}

impl HostKey {
    pub fn generate() -> Self {
        let (private, public) = HostKem::gen_keypair(&mut OsRng);
        HostKey { private, public }
    }

    /// The public half as it goes on the wire (32 bytes).
    pub fn public(&self) -> Vec<u8> {
        self.public.to_bytes().to_vec()
    }

    /// Opens what [`seal`] made for this key with the same `info`.
    pub fn open(
        &self,
        encapsulated: &[u8],
        sealed: &[u8],
        info: &[u8],
    ) -> Result<Vec<u8>, SealError> {
        let encapsulated =
            <HostKem as Kem>::EncappedKey::from_bytes(encapsulated).map_err(|_| SealError::Key)?;
        hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, HostKem>(
            &OpModeR::Base,
            &self.private,
            &encapsulated,
            info,
            sealed,
            &[],
        )
        .map_err(|_| SealError::Open)
    }
}

/// Seals `plaintext` to the host's public key: returns the encapsulated
/// key and the ciphertext.
pub fn seal(
    host_public: &[u8],
    info: &[u8],
    plaintext: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), SealError> {
    let public =
        <HostKem as Kem>::PublicKey::from_bytes(host_public).map_err(|_| SealError::Key)?;
    let (encapsulated, sealed) =
        hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, HostKem, _>(
            &OpModeS::Base,
            &public,
            info,
            plaintext,
            &[],
            &mut OsRng,
        )
        .map_err(|_| SealError::Seal)?;
    Ok((encapsulated.to_bytes().to_vec(), sealed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens() {
        let key = HostKey::generate();
        let (encapsulated, sealed) = seal(&key.public(), b"transcript", b"secret").unwrap();
        assert_eq!(
            key.open(&encapsulated, &sealed, b"transcript").unwrap(),
            b"secret"
        );
    }

    #[test]
    fn opens_only_with_the_same_key_and_info() {
        let key = HostKey::generate();
        let (encapsulated, mut sealed) = seal(&key.public(), b"transcript", b"secret").unwrap();
        assert!(key.open(&encapsulated, &sealed, b"other").is_err());
        assert!(HostKey::generate()
            .open(&encapsulated, &sealed, b"transcript")
            .is_err());
        sealed[0] ^= 1;
        assert!(key.open(&encapsulated, &sealed, b"transcript").is_err());
    }
}
