//! sway's IPC (the i3 IPC protocol over `$SWAYSOCK`): where each window is
//! on the output layout, and focusing one. Wayland gives a client no window
//! positions, so pointer input (mapped from the picture onto the window)
//! needs the compositor's own geometry; sway reports it per window along
//! with the window's ext-foreign-toplevel-list identifier.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use serde_json::Value;

const RUN_COMMAND: u32 = 0;
const GET_OUTPUTS: u32 = 3;
const GET_TREE: u32 = 4;

/// A rectangle on the output layout, in layout pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Rect {
            x: value.get("x")?.as_i64()? as i32,
            y: value.get("y")?.as_i64()? as i32,
            width: value.get("width")?.as_i64()? as i32,
            height: value.get("height")?.as_i64()? as i32,
        })
    }
}

/// One window in sway's tree.
#[derive(Debug, Clone)]
pub struct Window {
    /// sway's container id, for commands.
    pub con_id: i64,
    /// The ext-foreign-toplevel-list identifier.
    pub identifier: String,
    /// The window's content on the layout, without decorations.
    pub rect: Rect,
    /// The output it is on.
    pub output: String,
}

pub struct Sway {
    socket: UnixStream,
}

impl Sway {
    /// Connects to the sway named by `$SWAYSOCK`; `None` under any other
    /// compositor.
    pub fn connect() -> Option<Self> {
        let path = std::env::var_os("SWAYSOCK")?;
        UnixStream::connect(path).ok().map(|socket| Sway { socket })
    }

    fn request(&mut self, kind: u32, payload: &str) -> std::io::Result<Value> {
        let mut message = Vec::with_capacity(14 + payload.len());
        message.extend_from_slice(b"i3-ipc");
        message.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        message.extend_from_slice(&kind.to_le_bytes());
        message.extend_from_slice(payload.as_bytes());
        self.socket.write_all(&message)?;
        let mut header = [0u8; 14];
        self.socket.read_exact(&mut header)?;
        let length = u32::from_le_bytes(header[6..10].try_into().expect("length")) as usize;
        let mut body = vec![0u8; length];
        self.socket.read_exact(&mut body)?;
        serde_json::from_slice(&body).map_err(std::io::Error::other)
    }

    /// Every window with an identifier, with its place on the layout.
    pub fn windows(&mut self) -> std::io::Result<Vec<Window>> {
        let tree = self.request(GET_TREE, "")?;
        let mut found = Vec::new();
        walk(&tree, "", &mut found);
        Ok(found)
    }

    /// The bounding box of every output: the extent absolute pointer
    /// positions are given in.
    pub fn layout(&mut self) -> std::io::Result<Rect> {
        let outputs = self.request(GET_OUTPUTS, "")?;
        let rects: Vec<Rect> = outputs
            .as_array()
            .into_iter()
            .flatten()
            .filter(|output| output.get("active").and_then(Value::as_bool) != Some(false))
            .filter_map(|output| Rect::from_json(output.get("rect")?))
            .collect();
        let right = rects.iter().map(|r| r.x + r.width).max().unwrap_or(0);
        let bottom = rects.iter().map(|r| r.y + r.height).max().unwrap_or(0);
        Ok(Rect {
            x: 0,
            y: 0,
            width: right,
            height: bottom,
        })
    }

    /// The rectangle of each output by name.
    pub fn outputs(&mut self) -> std::io::Result<Vec<(String, Rect)>> {
        let outputs = self.request(GET_OUTPUTS, "")?;
        Ok(outputs
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|output| {
                Some((
                    output.get("name")?.as_str()?.to_owned(),
                    Rect::from_json(output.get("rect")?)?,
                ))
            })
            .collect())
    }

    /// Focuses a window (switching to its workspace).
    pub fn focus(&mut self, con_id: i64) -> std::io::Result<()> {
        self.request(RUN_COMMAND, &format!("[con_id={con_id}] focus"))
            .map(|_| ())
    }
}

fn walk(node: &Value, output: &str, found: &mut Vec<Window>) {
    let output = if node.get("type").and_then(Value::as_str) == Some("output") {
        node.get("name").and_then(Value::as_str).unwrap_or(output)
    } else {
        output
    };
    if let (Some(identifier), Some(con_id), Some(outer)) = (
        node.get("foreign_toplevel_identifier")
            .and_then(Value::as_str),
        node.get("id").and_then(Value::as_i64),
        node.get("rect").and_then(Rect::from_json),
    ) {
        // `window_rect` is the content inside the borders, relative to
        // `rect`.
        let rect = match node.get("window_rect").and_then(Rect::from_json) {
            Some(inner) if inner.width > 0 && inner.height > 0 => Rect {
                x: outer.x + inner.x,
                y: outer.y + inner.y,
                width: inner.width,
                height: inner.height,
            },
            _ => outer,
        };
        found.push(Window {
            con_id,
            identifier: identifier.to_owned(),
            rect,
            output: output.to_owned(),
        });
    }
    for key in ["nodes", "floating_nodes"] {
        for child in node
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(child, output, found);
        }
    }
}
