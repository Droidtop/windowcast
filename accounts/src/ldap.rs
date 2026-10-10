//! LDAP and Active Directory as a password source (feature `ldap`): the
//! user's own bind checks the password, and their groups come from
//! `memberOf` or a group search. LDAPS or StartTLS; plain LDAP only to a
//! directory on loopback.

use std::time::Duration;

use ldap3::{dn_escape, ldap_escape, LdapConn, LdapConnSettings, Scope, SearchEntry};
use serde::{Deserialize, Serialize};

use crate::{Account, CheckError, Method};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LdapConfig {
    /// `ldaps://dc.example.org` or `ldap://...` (StartTLS unless on
    /// loopback).
    pub url: String,
    /// Bind DN for the user, `{user}` replaced: `uid={user},ou=people,
    /// dc=example,dc=org`, or `{user}@example.org` (Active Directory's UPN
    /// bind). Empty: search for the user with the service account first.
    pub bind_dn: String,
    /// The service account for searching, when `bind_dn` is empty or
    /// groups are searched for.
    pub search_dn: String,
    pub search_password: String,
    /// Where users are.
    pub user_base: String,
    /// Finds the user, `{user}` replaced (escaped):
    /// `(&(objectClass=person)(uid={user}))`, or `(sAMAccountName={user})`.
    pub user_filter: String,
    /// Where groups are; empty reads the user's `memberOf` instead.
    pub group_base: String,
    /// Finds the user's groups, `{dn}` replaced (escaped):
    /// `(member={dn})`.
    pub group_filter: String,
    /// The group attribute that names it (`cn`).
    pub group_name: String,
    /// Skip certificate checks (a test directory with its own CA only).
    pub insecure_skip_verify: bool,
}

impl Default for LdapConfig {
    fn default() -> Self {
        LdapConfig {
            url: String::new(),
            bind_dn: String::new(),
            search_dn: String::new(),
            search_password: String::new(),
            user_base: String::new(),
            user_filter: "(uid={user})".into(),
            group_base: String::new(),
            group_filter: "(member={dn})".into(),
            group_name: "cn".into(),
            insecure_skip_verify: false,
        }
    }
}

fn unavailable(e: impl std::fmt::Display) -> Option<CheckError> {
    Some(CheckError::Unavailable(format!("LDAP: {e}")))
}

fn connect(config: &LdapConfig) -> Result<LdapConn, Option<CheckError>> {
    // rustls picks its crypto from the process default; set ring's unless
    // something else in the process already has.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = url::Url::parse(&config.url).map_err(unavailable)?;
    // ldap: is not a scheme the URL standard knows, so its host comes back
    // as a name even when it is an address.
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(|c| c == '[' || c == ']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    let settings = LdapConnSettings::new()
        .set_conn_timeout(Duration::from_secs(10))
        .set_starttls(url.scheme() == "ldap" && !loopback)
        .set_no_tls_verify(config.insecure_skip_verify);
    LdapConn::with_settings(settings, &config.url).map_err(unavailable)
}

/// Finds the user's DN with the service account.
fn find_user(
    config: &LdapConfig,
    ldap: &mut LdapConn,
    username: &str,
) -> Result<String, Option<CheckError>> {
    ldap.simple_bind(&config.search_dn, &config.search_password)
        .and_then(|r| r.success())
        .map_err(unavailable)?;
    let filter = config.user_filter.replace("{user}", &ldap_escape(username));
    let (entries, _) = ldap
        .search(&config.user_base, Scope::Subtree, &filter, vec!["1.1"])
        .and_then(|r| r.success())
        .map_err(unavailable)?;
    match entries.len() {
        // Unknown here: the next password source may know them.
        0 => Err(None),
        1 => Ok(SearchEntry::construct(entries.into_iter().next().expect("one entry")).dn),
        _ => Err(Some(CheckError::Refused(format!(
            "LDAP: {username:?} matches more than one entry"
        )))),
    }
}

/// Checks the password by binding as the user, then reads their groups.
pub fn check_password(
    config: &LdapConfig,
    username: &str,
    password: &str,
) -> Result<Account, Option<CheckError>> {
    // An empty password is an unauthenticated bind, which succeeds.
    if password.is_empty() {
        return Err(Some(CheckError::Refused("LDAP: empty password".into())));
    }
    let mut ldap = connect(config)?;
    let dn = if config.bind_dn.is_empty() {
        find_user(config, &mut ldap, username)?
    } else {
        config.bind_dn.replace("{user}", &dn_escape(username))
    };
    let bound = ldap.simple_bind(&dn, password).map_err(unavailable)?;
    match bound.rc {
        0 => {}
        // invalidCredentials (also an unknown DN, so nothing leaks)
        49 => {
            return Err(Some(CheckError::Refused(
                "LDAP: invalid credentials".into(),
            )))
        }
        rc => return Err(unavailable(format!("bind result {rc}: {}", bound.text))),
    }
    let groups = groups(config, &mut ldap, &dn).unwrap_or_default();
    let _ = ldap.unbind();
    Ok(Account {
        name: username.to_owned(),
        groups,
        method: Method::Password,
        provider: "ldap".into(),
    })
}

/// The groups of `username`, looked up with the service account (for a
/// Kerberos sign-in, which brings no groups of its own).
pub fn groups_of(config: &LdapConfig, username: &str) -> Result<Vec<String>, Option<CheckError>> {
    let mut ldap = connect(config)?;
    let dn = find_user(config, &mut ldap, username)?;
    let groups = groups(config, &mut ldap, &dn);
    let _ = ldap.unbind();
    groups
}

/// The group names of `dn`, read on a connection bound as someone allowed
/// to read them (the user, or the service account).
fn groups(
    config: &LdapConfig,
    ldap: &mut LdapConn,
    dn: &str,
) -> Result<Vec<String>, Option<CheckError>> {
    if config.group_base.is_empty() {
        let (entries, _) = ldap
            .search(dn, Scope::Base, "(objectClass=*)", vec!["memberOf"])
            .and_then(|r| r.success())
            .map_err(unavailable)?;
        return Ok(entries
            .into_iter()
            .flat_map(|e| {
                SearchEntry::construct(e)
                    .attrs
                    .remove("memberOf")
                    .unwrap_or_default()
            })
            .map(|group_dn| first_value(&group_dn, &config.group_name))
            .collect());
    }
    let filter = config.group_filter.replace("{dn}", &ldap_escape(dn));
    let (entries, _) = ldap
        .search(
            &config.group_base,
            Scope::Subtree,
            &filter,
            vec![config.group_name.as_str()],
        )
        .and_then(|r| r.success())
        .map_err(unavailable)?;
    Ok(entries
        .into_iter()
        .filter_map(|e| {
            SearchEntry::construct(e)
                .attrs
                .remove(&config.group_name)
                .and_then(|values| values.into_iter().next())
        })
        .collect())
}

/// `cn=admins,ou=groups,dc=example,dc=org` with `cn` gives `admins`; a DN
/// whose first part is another attribute is kept whole.
fn first_value(dn: &str, attribute: &str) -> String {
    dn.split(',')
        .next()
        .and_then(|first| first.split_once('='))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case(attribute))
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_else(|| dn.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_names_from_dns() {
        assert_eq!(
            first_value("cn=admins,ou=groups,dc=example,dc=org", "cn"),
            "admins"
        );
        assert_eq!(
            first_value("CN=Domain Users,CN=Users,DC=corp", "cn"),
            "Domain Users"
        );
        assert_eq!(first_value("ou=x,dc=y", "cn"), "ou=x,dc=y");
    }

    /// Against a directory the CI runs on loopback (`slapd`, seeded by the
    /// workflow): WINDOWCAST_TEST_LDAP=ldap://127.0.0.1:389.
    #[test]
    fn signs_in_against_a_real_directory() {
        let Ok(url) = std::env::var("WINDOWCAST_TEST_LDAP") else {
            eprintln!("WINDOWCAST_TEST_LDAP not set; skipping");
            return;
        };
        let template = LdapConfig {
            url,
            user_base: "ou=people,dc=windowcast,dc=test".into(),
            group_base: "ou=groups,dc=windowcast,dc=test".into(),
            group_filter: "(member={dn})".into(),
            ..LdapConfig::default()
        };
        // Bind DN template.
        let direct = LdapConfig {
            bind_dn: "uid={user},ou=people,dc=windowcast,dc=test".into(),
            ..template.clone()
        };
        let account = check_password(&direct, "alice", "alice-password").unwrap();
        assert_eq!(account.groups, vec!["streamers".to_owned()]);
        assert!(matches!(
            check_password(&direct, "alice", "wrong"),
            Err(Some(CheckError::Refused(_)))
        ));
        assert!(matches!(
            check_password(&direct, "alice", ""),
            Err(Some(CheckError::Refused(_)))
        ));

        // Search, then bind; an unknown user falls through to the next
        // source.
        let search = LdapConfig {
            search_dn: "cn=admin,dc=windowcast,dc=test".into(),
            search_password: "admin-password".into(),
            ..template
        };
        assert_eq!(
            check_password(&search, "alice", "alice-password")
                .unwrap()
                .name,
            "alice"
        );
        assert!(matches!(check_password(&search, "nobody", "x"), Err(None)));
        assert_eq!(
            groups_of(&search, "alice").unwrap(),
            vec!["streamers".to_owned()]
        );
    }
}
