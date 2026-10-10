# Accounts: who may connect, besides paired devices

windowcast's first credential is the paired device (docs/SECURITY.md): a PIN
once, then a pinned Ed25519 key. That suits one person and their own
machines. It does not suit a household where several people share a host,
or an organisation whose users already have accounts somewhere (an
OpenID Connect provider, Active Directory, LDAP, Kerberos). This note is
the design of windowcast's second credential, the **account**, in the
`windowcast-accounts` crate, and of how it fits beside the first.

## The rule: accounts authorise, device keys identify

Every endpoint still has its persistent Ed25519 identity, and every
session is still authenticated by signatures with it over the signaling
transcript (SECURITY.md, "How the descriptions are authenticated"). An
account never replaces that key. What an account adds is the answer to
*who is this device acting for, and what may they do*:

- A client signs in with an account once on a host. The host checks the
  account (local password, the OS, LDAP, an OIDC token, a Kerberos
  ticket) and **registers** the client's device key to that account: a
  record of key, account, groups, how it signed in, and until when.
- Later connections resume with the device key exactly as a paired device
  does. The host looks up the registration, re-applies its policy to the
  account behind it, and refuses the device once the registration has
  expired, the account is gone, or policy no longer admits it.
- PIN pairing stays as it is. A paired device has no account; policy
  rules reach it with `method:pin`, and with no policy written it may do
  everything it may do today.

## The base: windowcast-directory, kept in part

The old `windowcast-directory` crate (removed in 5bf0514, restored for this
work) had two halves:

1. **`AccountStore`**: usernames with Argon2id password hashes (the
   argon2 crate's defaults: Argon2id, a random salt per hash, PHC
   strings), revocation, JSON on disk. Kept as the store of **local
   accounts**, with groups in place of its one opaque `role`.
2. **`DirectoryCa`**: a certificate authority we would run, minting
   PASETO tokens that bind an account to a session key, so many hosts
   could trust one directory. **Dropped.** The central account directory
   an organisation trusts is its own identity provider (OIDC, AD/LDAP,
   Kerberos); windowcast running a second one beside it is a second
   mechanism for the same job, and we would also have to build and secure
   its server. Registrations on each host do the per-device binding the
   session certificates did. rusty_paseto leaves the dependency tree.

The crate is renamed `windowcast-accounts` (it is no longer a directory)
and lives in `accounts/`.

## Signing in on the wire

A new connect mode, `Account`, beside `Pair` and `Resume`:

1. `Hello` both ways as today (version, identity, nonce, mode).
2. The host sends `AccountOffer`: the sign-in methods it takes
   (`password`, `oidc`, `kerberos`), its OIDC providers (name, issuer,
   client id, scopes), its Kerberos service name, and a fresh **HPKE**
   public key (RFC 9180, X25519-HKDF-SHA256 with ChaCha20-Poly1305,
   the `hpke` crate), all signed with the host's identity over the
   transcript so far.
3. The client checks that it trusts the host key (below), then sends
   `AccountProof`: its credential sealed to that HPKE key, with the
   transcript as HPKE `info`. Only the host can read it; a captured proof
   is useless on any other connection (fresh key, nonces).
4. The host opens it and checks the credential. Any failure is the same
   generic "authentication failed" and a closed connection, as for PINs,
   and failures count against the same lockout.
5. Offer and answer are signed as on `Resume`, with a hash of the proof in
   the transcript, so the device key that signs the offer is the one the
   proof was made on.

A client can also stop after step 2: that is how it asks a host which
sign-in methods and providers it offers (`Client::sign_in_options`).

### The client must know the host first

A password, a Kerberos ticket or an ID token sent to the wrong machine is
lost. So a client sends a credential only to a host key it already trusts:
pinned by an earlier PIN pairing or sign-in, provisioned into its trusted
hosts (an organisation's device management can drop the file), or
**confirmed by the user** from the fingerprint the host shows, the way SSH
asks about a new host key. `Client::connect_account` refuses an unknown
host with `HostNotTrusted(key)`; the application shows the fingerprint, and
calls again with that key accepted. The OIDC provider a host names is only
used once its key is trusted, so a stranger cannot send the user to an
identity provider of its choosing.

## Methods

- **Local accounts** (`password`): the host's own `accounts.json`,
  Argon2id. Made and removed in the host application or its config.
- **The host OS's accounts** (`password`): PAM on Linux (service name
  configurable, default `login`; `pam_authenticate` then `pam_acct_mgmt`),
  `LogonUserW` (network logon) on Windows. Group membership from the OS
  (`getgrouplist`; the token's groups on Windows).
- **LDAP / Active Directory** (`password`): `ldap3` (rustls). Either a
  bind DN template (`uid={user},ou=people,dc=example,dc=org`, or
  `{user}@example.org` for AD's UPN bind) or search-then-bind with a
  service account; groups from `memberOf` or a group search
  (`(member={dn})`). LDAPS or StartTLS; plain LDAP only to loopback.
  Password backends are tried in the order the host lists them; the first
  that knows the user decides.
- **OpenID Connect** (`oidc`): the client signs in with the provider in
  the user's browser (authorization code with PKCE, redirected to a
  loopback port as `http://localhost:<port>`, RFC 8252), or with the device-code flow (RFC 8628) where
  there is no browser. The client asks for the `openid` scope with a
  **nonce derived from its device key** (SHA-256 of a label, the key and a
  salt it sends along), and presents the ID token. The host fetches the
  provider's discovery document and keys, and checks signature (RS*, PS*,
  ES*, EdDSA; never HS*), issuer, audience (its configured client id),
  expiry, the nonce against the presenting device's key when the token
  has one, and remembers the token's hash until it expires so it registers
  one device only. Username from a configurable claim
  (`preferred_username`, then `email`, then `sub`), groups from a
  configurable claim (`groups`). Issuers must be https, except on
  loopback (tests). The client id is a public client: no secret ships.
- **SAML** goes through an OIDC broker (Keycloak, Dex, Authentik, Azure
  AD/Entra all front SAML IdPs with OIDC). windowcast speaks only OIDC;
  there is no SAML code.
- **Kerberos** (`kerberos`): `cross-krb5`, which is GSS-API (MIT or
  Heimdal) on Unix and SSPI's Kerberos package on Windows. The client
  uses the user's existing tickets (a domain login on Windows, `kinit`
  elsewhere) for the service principal the host names, and sends the
  Kerberos GSS token sealed like any credential; the host accepts with its
  keytab (or the machine account on Windows). That is single sign-on: no
  password typed. The tokens are plain Kerberos rather than SPNEGO:
  SPNEGO, what HTTP's Negotiate wraps them in, adds only a fallback to
  NTLM, which windowcast does not take, and cross-krb5's Unix side speaks
  Kerberos only. Groups come from LDAP when the host has it configured.

## Policy

Each host has a policy: an ordered list of rules, first match wins.

```json
{ "rules": [
  { "who": ["group:admins"], "windows": ["*"], "input": true, "commands": true },
  { "who": ["group:staff", "provider:corp"], "hosts": ["build-*"], "windows": ["*Terminal*", "code"], "input": true },
  { "who": ["*"], "allow": false }
] }
```

- `who`: `user:<name>`, `group:<name>`, `method:<pin|password|oidc|kerberos>`,
  `provider:<name>` (the OIDC provider, `ldap`, `os`, `local`), or `*`.
  A rule matches when any entry does.
- `hosts`: globs on the host's name, so one policy file can be shared by
  many hosts (distributed by whatever manages them); absent means any.
- `windows`: globs on a window's app id or title; absent means all. A
  window outside them is not listed to the session and cannot be
  streamed or given input.
- `input`, `commands` (the command stream of Droidtop/tracker#444:
  shells, launching apps): what the session may do; absent means the
  host's own settings decide.
- `allow: false` refuses the connection.

No rules means today's behaviour: every authenticated principal may see
every window. When rules exist and none matches, the connection is
refused.

## SSH and the command stream

The command stream (Droidtop/tracker#444) runs over SSH. The account layer
authorises it through a narrow interface, without the SSH crate depending
on how accounts are checked:

- `windowcast_accounts::Account` (name, groups, method, provider) is what
  fills the command stream's `Principal::account` for a session's peer
  (`None` for a plain PIN-paired device), and
  `Policy::decide(Some(&account), host).commands` answers it.
- An SSH server outside windowcast sessions can check a sign-in itself
  through `Accounts::check(credential)`: a password (PAM, LDAP, local),
  or a Kerberos token (GSSAPI).
- **SSH user certificates from a sign-in**: a host with an SSH CA key
  configured signs a short-lived OpenSSH user certificate (`ssh-key`) for
  an account-registered client's SSH public key, principals the account
  name, valid for minutes (default 10), on request over the session
  (`ControlMessage::SshCertificate`). Any sshd that trusts the CA
  (`TrustedUserCAKeys`) then admits that user, so OIDC, LDAP or Kerberos
  sign-ins reach plain SSH servers without passwords there.

## What lives where

- `protocol`: the wire types (`ConnectMode::Account`, `AccountOffer`,
  `AccountProof`, the credential enum).
- `accounts`: everything else: local store, PAM/LogonUser, LDAP, OIDC
  (client flows and token checks), Kerberos, policy, registrations,
  sealing, SSH certificates. LDAP, PAM and Kerberos are cargo features
  (`ldap`, `pam`, `kerberos`) so the Android client builds none of them.
- `transport::signaling`: the `Account` exchange.
- `host-core`: registrations, the policy applied to window lists,
  streams, input and commands; config read from the host's data folder.
- `client-core`: `sign_in_options`, `connect_account`, the OIDC flows,
  and the C interface for them.
- Reference app: sign-in on the client (username and password, or the
  browser for a provider); accounts, providers and policy in the host's
  config.
