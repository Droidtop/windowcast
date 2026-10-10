//! Accounts: windowcast's second credential, beside the paired device.
//! See `docs/ACCOUNTS.md` in the repo root for the design.
//!
//! Accounts authorise; device keys identify. A client signs in with an
//! account once (a password the host checks itself, against the OS or a
//! directory; an OpenID Connect ID token; a Kerberos ticket), and the host
//! registers the client's Ed25519 device key to that account
//! ([`Registrations`]). Later connections resume with the device key; the
//! host's [`Policy`] decides what the account may do.
//!
//! This crate is the host's checking ([`Accounts::check`]), the client's
//! sign-in flows ([`oidc`], [`kerberos`]), the sealing of a credential to
//! the host ([`seal`]), and the SSH certificates a sign-in can be turned
//! into ([`ssh`]). LDAP, PAM and Kerberos are cargo features (`ldap`,
//! `pam`, `kerberos`), so a client that needs none of them builds none.

#[cfg(feature = "kerberos")]
pub mod kerberos;
#[cfg(feature = "ldap")]
pub mod ldap;
mod local;
pub mod oidc;
pub mod os;
mod policy;
mod registrations;
pub mod seal;
pub mod ssh;

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use windowcast_identity::PeerId;
use windowcast_protocol::{AccountCredential, OidcProviderInfo, SignInMethod};

pub use local::{LocalAccount, LocalAccounts, LocalError};
pub use policy::{Decision, Policy, Rule};
pub use registrations::{Registration, Registrations};

/// An account a device acts for, as a host knows it after a sign-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// The user's name, as the source that checked it gives it (without a
    /// Kerberos realm or an LDAP DN).
    pub name: String,
    pub groups: Vec<String>,
    pub method: Method,
    /// Where it came from: `local`, `os`, `ldap`, an OIDC provider's name,
    /// or a Kerberos realm.
    pub provider: String,
}

/// How an account signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Password,
    Oidc,
    Kerberos,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Password => "password",
            Method::Oidc => "oidc",
            Method::Kerberos => "kerberos",
        }
    }
}

/// Where a host checks passwords, tried in the order the configuration
/// lists them; the first that knows the user decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasswordSource {
    /// The host's own accounts (`accounts.json`, Argon2id).
    Local,
    /// The host operating system's accounts: PAM on Linux, `LogonUserW`
    /// on Windows.
    Os,
    /// An LDAP directory or Active Directory ([`ldap::LdapConfig`]).
    Ldap,
}

/// A host's sign-in configuration. Absent (in the host's settings) means
/// sign-in is off and only PIN-paired devices connect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Password sources, in order; empty takes no passwords.
    pub password: Vec<PasswordSource>,
    /// The PAM service the OS source uses (Linux).
    pub pam_service: String,
    #[cfg(feature = "ldap")]
    pub ldap: Option<ldap::LdapConfig>,
    #[cfg(not(feature = "ldap"))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ldap: Option<serde_json::Value>,
    /// OpenID Connect providers whose ID tokens this host accepts.
    pub oidc: Vec<oidc::ProviderConfig>,
    /// The Kerberos service principal this host accepts tickets for
    /// (`host/name.example.org@REALM`); `None` takes no tickets. An empty
    /// string accepts any principal in the keytab.
    pub kerberos: Option<String>,
    /// How long a sign-in registers a device for, in days; 0 asks for a
    /// sign-in on every connection.
    pub registration_days: u32,
    /// The OpenSSH user CA key (a file in the host's data folder) that
    /// signs SSH certificates for signed-in accounts; `None` signs none.
    pub ssh_ca: Option<String>,
    /// Minutes an SSH certificate is valid for.
    pub ssh_certificate_minutes: u32,
    /// The name policy rules' `hosts` globs match; empty is the machine's
    /// own host name.
    pub host_name: String,
    pub policy: Policy,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            password: vec![PasswordSource::Local],
            pam_service: "login".into(),
            ldap: None,
            oidc: Vec::new(),
            kerberos: None,
            registration_days: 30,
            ssh_ca: None,
            ssh_certificate_minutes: 10,
            host_name: String::new(),
            policy: Policy::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// The credential is wrong, or no source knows the user. Callers say
    /// only "authentication failed" to the other side.
    #[error("sign-in refused: {0}")]
    Refused(String),
    /// This host does not take that kind of credential.
    #[error("this host does not take {0} sign-ins")]
    NotOffered(&'static str),
    /// A source could not be asked (directory down, provider unreachable).
    #[error("{0}")]
    Unavailable(String),
}

/// A host's account checking: its configuration, local accounts, and the
/// OIDC providers' keys. Thread-safe; checks block (network, PAM), so
/// async callers run them on a blocking thread.
pub struct Accounts {
    config: Config,
    local: Mutex<LocalAccounts>,
    local_path: std::path::PathBuf,
    providers: Vec<oidc::Verifier>,
}

impl Accounts {
    /// Opens a host's accounts: `config`, with local accounts kept in
    /// `data_dir/accounts.json`.
    pub fn open(config: Config, data_dir: &Path) -> Result<Self, LocalError> {
        let local_path = data_dir.join("accounts.json");
        let local = LocalAccounts::load(&local_path)?;
        let providers = config
            .oidc
            .iter()
            .cloned()
            .map(oidc::Verifier::new)
            .collect();
        Ok(Accounts {
            config,
            local: Mutex::new(local),
            local_path,
            providers,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The methods and providers this host offers, as the sign-in offer
    /// carries them.
    pub fn offer(&self) -> (Vec<SignInMethod>, Vec<OidcProviderInfo>, Option<String>) {
        let mut methods = Vec::new();
        if !self.config.password.is_empty() {
            methods.push(SignInMethod::Password);
        }
        if !self.config.oidc.is_empty() {
            methods.push(SignInMethod::Oidc);
        }
        if self.config.kerberos.is_some() {
            methods.push(SignInMethod::Kerberos);
        }
        let providers = self.config.oidc.iter().map(|p| p.info()).collect();
        (methods, providers, self.config.kerberos.clone())
    }

    /// The name policy rules match hosts by.
    pub fn host_name(&self) -> String {
        if self.config.host_name.is_empty() {
            os::host_name()
        } else {
            self.config.host_name.clone()
        }
    }

    /// Checks a credential presented by the device `client`. On success
    /// the account it signs in, which policy must still admit
    /// ([`Self::admits`]).
    pub fn check(
        &self,
        credential: &AccountCredential,
        client: &PeerId,
    ) -> Result<Account, CheckError> {
        match credential {
            AccountCredential::Password { username, password } => {
                self.check_password(username, password)
            }
            AccountCredential::Oidc { provider, id_token } => {
                let verifier = self
                    .providers
                    .iter()
                    .find(|v| v.name() == provider)
                    .ok_or(CheckError::NotOffered("this provider's"))?;
                verifier.verify(id_token, client)
            }
            AccountCredential::Kerberos { token } => self.check_kerberos(token),
        }
    }

    /// Whether the host's policy lets `account` connect at all.
    pub fn admits(&self, account: Option<&Account>) -> bool {
        self.decide(account).allow
    }

    /// The host's policy for `account` (`None`: a PIN-paired device).
    pub fn decide(&self, account: Option<&Account>) -> Decision {
        self.config.policy.decide(account, &self.host_name())
    }

    fn check_password(&self, username: &str, password: &str) -> Result<Account, CheckError> {
        if self.config.password.is_empty() {
            return Err(CheckError::NotOffered("password"));
        }
        for source in &self.config.password {
            let result = match source {
                PasswordSource::Local => self
                    .local
                    .lock()
                    .expect("local accounts")
                    .verify_password(username, password)
                    .map(|a| Account {
                        name: a.username.clone(),
                        groups: a.groups.clone(),
                        method: Method::Password,
                        provider: "local".into(),
                    })
                    .map_err(|e| match e {
                        LocalError::NotFound(_) => None,
                        other => Some(CheckError::Refused(other.to_string())),
                    }),
                PasswordSource::Os => {
                    os::check_password(&self.config.pam_service, username, password)
                }
                PasswordSource::Ldap => self.check_ldap(username, password),
            };
            match result {
                Ok(account) => return Ok(account),
                // This source decided: a wrong password here is final.
                Err(Some(e)) => return Err(e),
                // This source does not know the user: try the next.
                Err(None) => {}
            }
        }
        Err(CheckError::Refused(format!("no source knows {username:?}")))
    }

    #[cfg(feature = "ldap")]
    fn check_ldap(&self, username: &str, password: &str) -> Result<Account, Option<CheckError>> {
        match &self.config.ldap {
            Some(config) => ldap::check_password(config, username, password),
            None => Err(None),
        }
    }

    #[cfg(not(feature = "ldap"))]
    fn check_ldap(&self, _: &str, _: &str) -> Result<Account, Option<CheckError>> {
        Err(Some(CheckError::Unavailable(
            "this build has no LDAP support".into(),
        )))
    }

    #[cfg(feature = "kerberos")]
    fn check_kerberos(&self, token: &[u8]) -> Result<Account, CheckError> {
        let service = self
            .config
            .kerberos
            .as_deref()
            .ok_or(CheckError::NotOffered("Kerberos"))?;
        let principal = kerberos::accept(service, token)?;
        let (name, realm) = match principal.rsplit_once('@') {
            Some((name, realm)) => (name.to_owned(), realm.to_owned()),
            None => (principal.clone(), String::new()),
        };
        #[cfg(feature = "ldap")]
        let groups = match &self.config.ldap {
            Some(config) => ldap::groups_of(config, &name).unwrap_or_default(),
            None => Vec::new(),
        };
        #[cfg(not(feature = "ldap"))]
        let groups = Vec::new();
        Ok(Account {
            name,
            groups,
            method: Method::Kerberos,
            provider: realm,
        })
    }

    #[cfg(not(feature = "kerberos"))]
    fn check_kerberos(&self, _: &[u8]) -> Result<Account, CheckError> {
        Err(CheckError::NotOffered("Kerberos"))
    }

    /// The host's local accounts, to list and change them.
    pub fn local(&self) -> std::sync::MutexGuard<'_, LocalAccounts> {
        self.local.lock().expect("local accounts")
    }

    /// Saves the local accounts after a change.
    pub fn save_local(&self) -> Result<(), LocalError> {
        self.local().save(&self.local_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("windowcast-accounts-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn local_password_signs_in_with_groups() {
        let dir = temp_dir("lib");
        let accounts = Accounts::open(Config::default(), &dir).unwrap();
        accounts
            .local()
            .create("alice", "correct horse battery", &["staff".into()])
            .unwrap();
        let client = PeerId([1; 32]);

        let account = accounts
            .check(
                &AccountCredential::Password {
                    username: "alice".into(),
                    password: "correct horse battery".into(),
                },
                &client,
            )
            .unwrap();
        assert_eq!(account.name, "alice");
        assert_eq!(account.groups, vec!["staff".to_owned()]);
        assert_eq!(account.method, Method::Password);
        assert_eq!(account.provider, "local");

        for (user, pass) in [("alice", "wrong"), ("bob", "anything")] {
            assert!(matches!(
                accounts.check(
                    &AccountCredential::Password {
                        username: user.into(),
                        password: pass.into(),
                    },
                    &client,
                ),
                Err(CheckError::Refused(_))
            ));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn offer_lists_what_is_configured() {
        let dir = temp_dir("offer");
        let config = Config {
            kerberos: Some("host/example".into()),
            ..Config::default()
        };
        let accounts = Accounts::open(config, &dir).unwrap();
        let (methods, providers, service) = accounts.offer();
        assert_eq!(
            methods,
            vec![SignInMethod::Password, SignInMethod::Kerberos]
        );
        assert!(providers.is_empty());
        assert_eq!(service.as_deref(), Some("host/example"));

        let none = Accounts::open(
            Config {
                password: Vec::new(),
                ..Config::default()
            },
            &dir,
        )
        .unwrap();
        assert!(none.offer().0.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
