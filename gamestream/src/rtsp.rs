//! GameStream's RTSP, as moonlight-common-c (`RtspConnection.c`,
//! `RtspParser.c`) and Sunshine (`rtsp.cpp`) speak it: one request per TCP
//! connection, the host answering and closing; `Name: value` options, a
//! payload after the blank line (its length in `Content-length` on
//! requests; a response's runs to the close). Plain RTSP only: the
//! encrypted `rtspenc://` form is not offered by windowcast's host and a
//! client asks for it only when the host's launch answer names it.

use std::collections::BTreeMap;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::GameStreamError;

/// The GameStream RTSP setup port.
pub const RTSP_PORT: u16 = 48010;
/// Requests and responses larger than this are refused.
const MAX_MESSAGE: usize = 64 * 1024;

/// An RTSP message: a request (`command target`) or a response
/// (`status reason`), options in order, and a payload.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    /// `OPTIONS`, `DESCRIBE`, `SETUP`, `ANNOUNCE`, `PLAY`; empty for a
    /// response.
    pub command: String,
    pub target: String,
    /// For a response.
    pub status: u16,
    pub reason: String,
    pub options: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn request(command: &str, target: &str) -> Self {
        Message {
            command: command.into(),
            target: target.into(),
            ..Default::default()
        }
    }

    pub fn response(status: u16, reason: &str, cseq: Option<&str>) -> Self {
        let mut message = Message {
            status,
            reason: reason.into(),
            ..Default::default()
        };
        if let Some(cseq) = cseq {
            message.options.push(("CSeq".into(), cseq.into()));
        }
        message
    }

    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.options.push((name.into(), value.into()));
        self
    }

    /// An option's value; names compare without regard to case.
    pub fn option(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_request(&self) -> bool {
        !self.command.is_empty()
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = if self.is_request() {
            format!("{} {} RTSP/1.0\r\n", self.command, self.target)
        } else {
            format!("RTSP/1.0 {} {}\r\n", self.status, self.reason)
        };
        for (name, value) in &self.options {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("\r\n");
        let mut out = out.into_bytes();
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parses a whole message (head and payload).
    pub fn parse(bytes: &[u8]) -> Result<Self, GameStreamError> {
        let end = find(bytes, b"\r\n\r\n").ok_or(GameStreamError::Rtsp("no end of header"))?;
        let head =
            std::str::from_utf8(&bytes[..end]).map_err(|_| GameStreamError::Rtsp("not text"))?;
        let mut lines = head.split("\r\n");
        let first = lines.next().unwrap_or_default();
        let mut message = Message::default();
        let parts: Vec<&str> = first.splitn(3, ' ').collect();
        if first.starts_with("RTSP/") {
            message.status = parts
                .get(1)
                .and_then(|s| s.parse().ok())
                .ok_or(GameStreamError::Rtsp("bad status line"))?;
            message.reason = parts.get(2).unwrap_or(&"").to_string();
        } else {
            if parts.len() < 3 {
                return Err(GameStreamError::Rtsp("bad request line"));
            }
            message.command = parts[0].into();
            message.target = parts[1].into();
        }
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                message
                    .options
                    .push((name.trim().into(), value.trim().into()));
            }
        }
        message.payload = bytes[end + 4..].to_vec();
        Ok(message)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Host side: reads one request (its payload by `Content-length`).
pub async fn read_request<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<Message, GameStreamError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            let head = Message::parse(&buf[..end + 4])?;
            let length: usize = head
                .option("Content-length")
                .and_then(|l| l.parse().ok())
                .unwrap_or(0);
            if length > MAX_MESSAGE {
                return Err(GameStreamError::Rtsp("payload too large"));
            }
            while buf.len() < end + 4 + length {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    return Err(GameStreamError::Rtsp("closed mid-payload"));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            return Message::parse(&buf[..end + 4 + length]);
        }
        if buf.len() > MAX_MESSAGE {
            return Err(GameStreamError::Rtsp("header too large"));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(GameStreamError::Rtsp("closed before a request"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Host side: answers and closes.
pub async fn respond<S: AsyncWrite + Unpin>(
    stream: &mut S,
    response: &Message,
) -> Result<(), GameStreamError> {
    stream.write_all(&response.serialize()).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Client side: one request on a new connection, the response read to the
/// close.
pub async fn transact(
    address: std::net::SocketAddr,
    request: &Message,
) -> Result<Message, GameStreamError> {
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect(address),
    )
    .await
    .map_err(|_| GameStreamError::Rtsp("connect timed out"))??;
    stream.set_nodelay(true)?;
    stream.write_all(&request.serialize()).await?;
    let mut buf = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_to_end(&mut buf),
    )
    .await
    .map_err(|_| GameStreamError::Rtsp("no answer"))??;
    if buf.len() > MAX_MESSAGE {
        return Err(GameStreamError::Rtsp("answer too large"));
    }
    Message::parse(&buf)
}

/// The `a=name:value` attributes of an SDP payload (the last wins).
pub fn sdp_attributes(payload: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(payload);
    text.split(['\r', '\n'])
        .filter_map(|line| line.strip_prefix("a="))
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_string(), value.trim_end().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_moonlight_writes_them() {
        let request = Message::request("SETUP", "streamid=video/0/0")
            .with("CSeq", "3")
            .with("X-GS-ClientVersion", "14")
            .with("Host", "192.168.1.5")
            .with("Session", "DEADBEEFCAFE")
            .with("Transport", "unicast;X-GS-ClientPort=50000-50001");
        let parsed = Message::parse(&request.serialize()).unwrap();
        assert_eq!(parsed, request);
        assert_eq!(parsed.option("cseq"), Some("3"));

        let mut response = Message::response(200, "OK", Some("3"))
            .with("Session", "DEADBEEFCAFE;timeout = 90")
            .with("Transport", "server_port=47998");
        response.payload = b"a=x-ss-general.featureFlags:0\n".to_vec();
        let parsed = Message::parse(&response.serialize()).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.option("Transport"), Some("server_port=47998"));
        assert_eq!(
            sdp_attributes(&parsed.payload)
                .get("x-ss-general.featureFlags")
                .map(String::as_str),
            Some("0")
        );
    }
}
