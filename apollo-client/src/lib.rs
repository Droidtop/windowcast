//! A client for a *local* GameStream-protocol host's (Sunshine/Apollo)
//! unauthenticated `serverinfo` endpoint, plus XML parsing for its
//! authenticated `applist` endpoint's response shape.
//!
//! This is deliberately narrow: `serverinfo` needs no pairing at all (real
//! GameStream hosts answer it to anyone, precisely so a client can check
//! pair state before attempting to pair) and is implemented for real here.
//! `applist` DOES need an already-paired mutual-TLS session — pairing
//! itself isn't implemented anywhere in this repo yet (that's
//! `windowcast-moonlight`'s job, not started — see
//! `windowcast_protocol::StreamBackend::Moonlight`), so this crate
//! only provides [`parse_app_list_xml`] (real, tested, reusable) rather
//! than pretending to fetch it end to end.

use serde::Deserialize;
use windowcast_protocol::{GameEntry, GameId};

#[derive(Debug, thiserror::Error)]
pub enum ApolloError {
    #[error("http request to the GameStream host failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("failed to parse GameStream XML response: {0}")]
    Xml(#[from] quick_xml::de::DeError),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServerInfo {
    #[serde(rename = "@status_code")]
    pub status_code: i32,
    pub hostname: Option<String>,
    pub appversion: String,
    #[serde(rename = "PairStatus")]
    pub pair_status: u8,
    #[serde(rename = "HttpsPort")]
    pub https_port: u16,
}

impl ServerInfo {
    pub fn is_paired(&self) -> bool {
        self.pair_status == 1
    }
}

/// Talks to one local GameStream host's plain-HTTP `serverinfo` endpoint —
/// the one real, unauthenticated GameStream API call, used here purely to
/// discover whether it exists and whether this client is already paired
/// with it (real streaming/pairing itself is `windowcast-moonlight`'s job).
pub struct ApolloClient {
    http: reqwest::Client,
    address: String,
    http_port: u16,
}

impl ApolloClient {
    pub const DEFAULT_HTTP_PORT: u16 = 47989;

    pub fn new(address: impl Into<String>) -> Self {
        ApolloClient {
            http: reqwest::Client::new(),
            address: address.into(),
            http_port: Self::DEFAULT_HTTP_PORT,
        }
    }

    pub fn with_http_port(mut self, port: u16) -> Self {
        self.http_port = port;
        self
    }

    pub async fn server_info(&self) -> Result<ServerInfo, ApolloError> {
        let url = format!("http://{}:{}/serverinfo", self.address, self.http_port);
        let body = self.http.get(url).send().await?.text().await?;
        Ok(quick_xml::de::from_str(&body)?)
    }
}

#[derive(Debug, Deserialize)]
struct AppListRoot {
    #[serde(rename = "App", default)]
    apps: Vec<AppXml>,
}

#[derive(Debug, Deserialize)]
struct AppXml {
    #[serde(rename = "AppTitle")]
    title: String,
    #[serde(rename = "ID")]
    id: u32,
}

/// Parses a GameStream `applist` response body into windowcast's own
/// [`GameEntry`] shape. Real GameStream/Sunshine XML, not guessed at —
/// matches the shape `NvHTTP.getAppListByReader` (moonlight-android's own
/// reference implementation) parses. Artwork isn't populated here: the
/// real endpoint for that (`appasset`) is a separate authenticated
/// per-app request, out of scope for a listing call.
pub fn parse_app_list_xml(xml: &str) -> Result<Vec<GameEntry>, ApolloError> {
    let root: AppListRoot = quick_xml::de::from_str(xml)?;
    Ok(root
        .apps
        .into_iter()
        .map(|app| GameEntry {
            id: GameId(app.id),
            name: app.title,
            artwork_uri: None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_shaped_serverinfo_response() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <hostname>MyPC</hostname>
    <appversion>7.1.431.0</appversion>
    <PairStatus>1</PairStatus>
    <HttpsPort>47984</HttpsPort>
</root>"#;
        let info: ServerInfo = quick_xml::de::from_str(xml).unwrap();
        assert_eq!(info.status_code, 200);
        assert_eq!(info.hostname.as_deref(), Some("MyPC"));
        assert_eq!(info.https_port, 47984);
        assert!(info.is_paired());
    }

    #[test]
    fn parses_an_unpaired_serverinfo_response() {
        let xml = r#"<root status_code="200"><appversion>7.1.431.0</appversion><PairStatus>0</PairStatus><HttpsPort>47984</HttpsPort></root>"#;
        let info: ServerInfo = quick_xml::de::from_str(xml).unwrap();
        assert!(!info.is_paired());
    }

    #[test]
    fn parses_a_real_shaped_applist_response() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <App>
        <IsHdrSupported>1</IsHdrSupported>
        <AppTitle>Big Picture</AppTitle>
        <ID>1</ID>
    </App>
    <App>
        <IsHdrSupported>0</IsHdrSupported>
        <AppTitle>Half-Life 3</AppTitle>
        <ID>2</ID>
    </App>
</root>"#;
        let games = parse_app_list_xml(xml).unwrap();
        assert_eq!(
            games,
            vec![
                GameEntry {
                    id: GameId(1),
                    name: "Big Picture".into(),
                    artwork_uri: None
                },
                GameEntry {
                    id: GameId(2),
                    name: "Half-Life 3".into(),
                    artwork_uri: None
                },
            ]
        );
    }

    #[test]
    fn empty_app_list_parses_to_no_games() {
        let xml = r#"<root status_code="200"></root>"#;
        assert_eq!(parse_app_list_xml(xml).unwrap(), Vec::new());
    }
}
