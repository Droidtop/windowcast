//! Kerberos single sign-on (feature `kerberos`), through `cross-krb5`:
//! GSS-API (MIT or Heimdal) on Unix, SSPI's Kerberos package on Windows.
//! The tokens are plain Kerberos GSS tokens, which both understand; the
//! SPNEGO wrapping HTTP's Negotiate uses would add only a fallback to
//! NTLM, which windowcast does not take. The client uses the tickets the user already has (a
//! domain login on Windows, `kinit` elsewhere); the host accepts with its
//! keytab (`KRB5_KTNAME`, or the machine account on Windows).
//!
//! One token each way at most: the client's initial token is enough for
//! the host to know the user (the host is already authenticated to the
//! client by its pinned key, so Kerberos mutual authentication adds
//! nothing here).

use cross_krb5::{AcceptFlags, ClientCtx, InitiateFlags, K5ServerCtx, ServerCtx, Step};

use crate::CheckError;

/// The client's token for `service` (`host/name.example.org` or with a
/// realm), from the user's current tickets.
pub fn initiate(service: &str) -> Result<Vec<u8>, String> {
    let (_pending, token) = ClientCtx::new(InitiateFlags::empty(), None, service, None)
        .map_err(|e| format!("no Kerberos ticket for {service}: {e:#}"))?;
    Ok(token.to_vec())
}

/// Accepts the client's token for `service` (empty: any principal in the
/// keytab) and gives the client's principal (`alice@EXAMPLE.ORG`).
pub fn accept(service: &str, token: &[u8]) -> Result<String, CheckError> {
    let principal = (!service.is_empty()).then_some(service);
    let pending = ServerCtx::new(AcceptFlags::empty(), principal)
        .map_err(|e| CheckError::Unavailable(format!("Kerberos: {e:#}")))?;
    let step = pending
        .step(token)
        .map_err(|e| CheckError::Refused(format!("Kerberos: {e:#}")))?;
    match step {
        Step::Finished((mut context, _)) => context
            .client()
            .map_err(|e| CheckError::Refused(format!("Kerberos: {e:#}"))),
        Step::Continue(_) => Err(CheckError::Refused(
            "Kerberos: the exchange needs more than one token".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a KDC the CI runs on the runner: the user's tickets in
    /// KRB5CCNAME (kinit), the service's key in KRB5_KTNAME.
    #[test]
    fn a_ticket_signs_in() {
        let Ok(service) = std::env::var("WINDOWCAST_TEST_KERBEROS_SERVICE") else {
            eprintln!("WINDOWCAST_TEST_KERBEROS_SERVICE not set; skipping");
            return;
        };
        let user = std::env::var("WINDOWCAST_TEST_KERBEROS_USER").unwrap();
        let token = initiate(&service).unwrap();
        assert_eq!(accept(&service, &token).unwrap(), user);
        assert!(accept(&service, b"not a token").is_err());
    }
}
