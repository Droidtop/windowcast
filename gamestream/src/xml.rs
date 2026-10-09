//! GameStream's XML: flat `<root status_code="200">` documents with one
//! level of elements (and `<App>` groups in the app list). Read and
//! written by hand: the shapes are fixed and small.

use crate::GameStreamError;

/// The text of the first `<name>` element, entities decoded.
pub fn text(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    Some(unescape(xml[start..end].trim()))
}

/// The `<name>` element's text decoded from hex.
pub fn hex(xml: &str, name: &str) -> Option<Vec<u8>> {
    decode_hex(&text(xml, name)?)
}

/// Every `<name>...</name>` block's inside, in order.
pub fn blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let inner = &rest[start + open.len()..];
        let Some(end) = inner.find(&close) else { break };
        out.push(&inner[..end]);
        rest = &inner[end + close.len()..];
    }
    out
}

/// The root's `status_code`, and its `status_message` if any.
pub fn status(xml: &str) -> (Option<i64>, Option<String>) {
    let attribute = |name: &str| {
        let root = &xml[xml.find("<root")?..];
        let root = &root[..root.find('>')?];
        let at = root.find(&format!("{name}=\""))? + name.len() + 2;
        let end = at + root[at..].find('"')?;
        Some(unescape(&root[at..end]))
    };
    (
        attribute("status_code").and_then(|c| c.parse().ok()),
        attribute("status_message"),
    )
}

/// Fails unless the root says 200.
pub fn ok(xml: &str) -> Result<(), GameStreamError> {
    match status(xml) {
        (Some(200), _) => Ok(()),
        (code, message) => Err(GameStreamError::Status(
            code.unwrap_or(0),
            message.unwrap_or_default(),
        )),
    }
}

pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// A response document: `status_code`, an optional message, and elements.
pub fn response(status_code: u16, message: Option<&str>, elements: &[(&str, String)]) -> String {
    let mut out =
        format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"{status_code}\"");
    if let Some(message) = message {
        out.push_str(&format!(" status_message=\"{}\"", escape(message)));
    }
    out.push('>');
    for (name, value) in elements {
        out.push_str(&format!("<{name}>{}</{name}>", escape(value)));
    }
    out.push_str("</root>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_sunshine_writes() {
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><hostname>PC &amp; co</hostname><paired>1</paired><plaincert>4142</plaincert><App><AppTitle>Desktop</AppTitle><ID>1</ID></App><App><AppTitle>Steam</AppTitle><ID>2</ID></App></root>";
        assert_eq!(status(xml), (Some(200), None));
        assert_eq!(text(xml, "hostname").as_deref(), Some("PC & co"));
        assert_eq!(hex(xml, "plaincert").as_deref(), Some(&b"AB"[..]));
        let apps = blocks(xml, "App");
        assert_eq!(apps.len(), 2);
        assert_eq!(text(apps[1], "AppTitle").as_deref(), Some("Steam"));
        let refused = response(400, Some("Invalid uniqueid"), &[]);
        assert_eq!(
            status(&refused),
            (Some(400), Some("Invalid uniqueid".into()))
        );
        assert!(ok(&refused).is_err());
    }
}
