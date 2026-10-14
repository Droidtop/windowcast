//! VNC for windowcast: compatibility mode's VNC endpoint (docs/BACKENDS.md,
//! "Third-party clients and RemoteApp"). One window is the whole
//! framebuffer of an RFB server of its own (RFB 3.3 to 3.8, RFC 6143), for
//! any VNC viewer (TigerVNC, RealVNC, Remmina): VNC Authentication with a
//! password made for the window, Raw updates of what changed (DesktopSize
//! when the window changes size and the viewer can follow), any true-colour
//! pixel format the viewer asks for, and the viewer's keys and pointer
//! through the host's input rules. Written from RFC 6143.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use rand_core::RngCore;
use windowcast_host::compat::CompatProvider;
use windowcast_host::{Picture, WindowSource};
use windowcast_protocol::{CompatEndpoint, InputEvent, PointerButton, WindowId};

pub mod keys;

/// The ports VNC viewers try first (display :0 to :99).
const PORTS: std::ops::Range<u16> = 5900..6000;

/// The DesktopSize pseudo-encoding.
const DESKTOP_SIZE: i32 = -223;

/// Compatibility mode's VNC endpoint: an RFB server per window, on the
/// first free VNC port, with a password made for it.
pub struct VncCompat {
    source: Arc<dyn WindowSource>,
    bind: IpAddr,
}

impl VncCompat {
    pub fn new(source: Arc<dyn WindowSource>, bind: IpAddr) -> Self {
        VncCompat { source, bind }
    }
}

/// Stops a window's VNC server when dropped.
struct Stop(Arc<AtomicBool>);

impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A password VNC Authentication can carry (eight characters) that a
/// person can type.
fn new_password() -> String {
    const LETTERS: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut random = [0u8; 8];
    rand_core::OsRng.fill_bytes(&mut random);
    random
        .iter()
        .map(|b| LETTERS[*b as usize % LETTERS.len()] as char)
        .collect()
}

impl CompatProvider for VncCompat {
    fn open(
        &self,
        window: WindowId,
        _title: &str,
    ) -> Result<(CompatEndpoint, Box<dyn Send + Sync>), String> {
        let listener = PORTS
            .clone()
            .find_map(|port| TcpListener::bind((self.bind, port)).ok())
            .ok_or("no VNC port (5900 to 5999) is free")?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let password = new_password();
        let stop = Arc::new(AtomicBool::new(false));
        let server = Server {
            source: Arc::clone(&self.source),
            window,
            password: password.clone(),
            stop: Arc::clone(&stop),
        };
        std::thread::spawn(move || server.accept(listener));
        Ok((CompatEndpoint::Vnc { port, password }, Box::new(Stop(stop))))
    }
}

/// One window's RFB server.
#[derive(Clone)]
struct Server {
    source: Arc<dyn WindowSource>,
    window: WindowId,
    password: String,
    stop: Arc<AtomicBool>,
}

impl Server {
    fn accept(self, listener: TcpListener) {
        while !self.stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, peer)) => {
                    let server = self.clone();
                    std::thread::spawn(move || {
                        eprintln!("vnc: {peer} connected to window {}", server.window.0);
                        match server.serve(stream) {
                            Ok(()) => eprintln!("vnc: {peer} left"),
                            Err(e) => eprintln!("vnc: {peer}: {e}"),
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    eprintln!("vnc: {e}");
                    return;
                }
            }
        }
    }

    fn serve(&self, mut stream: TcpStream) -> std::io::Result<()> {
        stream.set_nodelay(true)?;
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        handshake(&mut stream, &self.password)?;
        stream.set_read_timeout(None)?;
        let mut pictures = match self.source.open_pictures(self.window) {
            Some(Ok(pictures)) => pictures,
            Some(Err(e)) => return Err(std::io::Error::other(e)),
            None => return Err(std::io::Error::other("this host has no raw pictures")),
        };
        let first = pictures
            .next_picture()
            .ok_or_else(|| std::io::Error::other("the window is gone"))?;
        server_init(&mut stream, &first)?;

        // The viewer's messages, read on a thread of their own; input goes
        // straight to the host.
        let (requests, wanted) = mpsc::channel();
        let deliver = windowcast_host::deliver_input(Arc::clone(&self.source));
        let reader = Reader {
            stream: stream.try_clone()?,
            window: self.window,
            deliver,
            requests,
            size: (first.width, first.height),
            buttons: 0,
            keys: keys::Modifiers::default(),
        };
        std::thread::spawn(move || reader.run());
        Updates {
            stream,
            format: PixelFormat::SERVER,
            resize: false,
            last: first,
            sent_size: None,
            wanted,
            stop: Arc::clone(&self.stop),
        }
        .run(pictures)
    }
}

/// RFB's version exchange, VNC Authentication and ClientInit.
fn handshake(stream: &mut TcpStream, password: &str) -> std::io::Result<()> {
    stream.write_all(b"RFB 003.008\n")?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version)?;
    let minor = match &version {
        b"RFB 003.003\n" => 3,
        b"RFB 003.007\n" => 7,
        _ if version.starts_with(b"RFB 003.") => 8,
        _ => return Err(std::io::Error::other("not an RFB viewer")),
    };
    const VNC_AUTHENTICATION: u8 = 2;
    if minor == 3 {
        stream.write_all(&u32::from(VNC_AUTHENTICATION).to_be_bytes())?;
    } else {
        stream.write_all(&[1, VNC_AUTHENTICATION])?;
        let mut chosen = [0u8; 1];
        stream.read_exact(&mut chosen)?;
        if chosen[0] != VNC_AUTHENTICATION {
            return Err(std::io::Error::other(
                "the viewer chose no security we offer",
            ));
        }
    }
    let mut challenge = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut challenge);
    stream.write_all(&challenge)?;
    let mut response = [0u8; 16];
    stream.read_exact(&mut response)?;
    if response != vnc_auth_response(password.as_bytes(), &challenge) {
        stream.write_all(&1u32.to_be_bytes())?;
        if minor == 8 {
            let reason = b"wrong password";
            stream.write_all(&(reason.len() as u32).to_be_bytes())?;
            stream.write_all(reason)?;
        }
        return Err(std::io::Error::other("wrong password"));
    }
    stream.write_all(&0u32.to_be_bytes())?;
    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared)?;
    Ok(())
}

/// What a viewer answers VNC Authentication's `challenge` with: the
/// challenge DES-encrypted with the password (eight bytes, zero-padded,
/// each byte's bits reversed, as VNC has always done).
pub fn vnc_auth_response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
    use des::cipher::{BlockCipherEncrypt, KeyInit};
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password) {
        *k = b.reverse_bits();
    }
    let cipher = des::Des::new_from_slice(&key).expect("an eight-byte key");
    let mut out = [0u8; 16];
    for (half, chunk) in challenge.chunks(8).enumerate() {
        let mut block = des::cipher::Array::<u8, _>::try_from(chunk).expect("eight bytes");
        cipher.encrypt_block(&mut block);
        out[half * 8..half * 8 + 8].copy_from_slice(&block);
    }
    out
}

/// A viewer's pixel format (true colour only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelFormat {
    bits: u8,
    big_endian: bool,
    max: [u16; 3],
    shift: [u8; 3],
}

impl PixelFormat {
    /// 32 bits, red, green and blue in bytes 2, 1 and 0 of a
    /// little-endian word: BGRA as hosts capture it.
    const SERVER: PixelFormat = PixelFormat {
        bits: 32,
        big_endian: false,
        max: [255, 255, 255],
        shift: [16, 8, 0],
    };

    fn bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0] = self.bits;
        out[1] = 24;
        out[2] = u8::from(self.big_endian);
        out[3] = 1;
        out[4..6].copy_from_slice(&self.max[0].to_be_bytes());
        out[6..8].copy_from_slice(&self.max[1].to_be_bytes());
        out[8..10].copy_from_slice(&self.max[2].to_be_bytes());
        out[10] = self.shift[0];
        out[11] = self.shift[1];
        out[12] = self.shift[2];
        out
    }

    /// A SetPixelFormat's format, if true colour at 8, 16 or 32 bits.
    fn parse(raw: &[u8; 16]) -> Option<PixelFormat> {
        let bits = raw[0];
        if raw[3] == 0 || !matches!(bits, 8 | 16 | 32) {
            return None;
        }
        Some(PixelFormat {
            bits,
            big_endian: raw[2] != 0,
            max: [
                u16::from_be_bytes([raw[4], raw[5]]),
                u16::from_be_bytes([raw[6], raw[7]]),
                u16::from_be_bytes([raw[8], raw[9]]),
            ],
            shift: [raw[10], raw[11], raw[12]],
        })
    }

    /// One BGRA pixel in this format, appended to `out`.
    fn put(&self, bgra: &[u8], out: &mut Vec<u8>) {
        if *self == PixelFormat::SERVER {
            out.extend_from_slice(&[bgra[0], bgra[1], bgra[2], 0]);
            return;
        }
        let scale = |value: u8, max: u16| u32::from(value) * u32::from(max) / 255;
        let value = scale(bgra[2], self.max[0]) << self.shift[0]
            | scale(bgra[1], self.max[1]) << self.shift[1]
            | scale(bgra[0], self.max[2]) << self.shift[2];
        match (self.bits, self.big_endian) {
            (32, false) => out.extend_from_slice(&value.to_le_bytes()),
            (32, true) => out.extend_from_slice(&value.to_be_bytes()),
            (16, false) => out.extend_from_slice(&(value as u16).to_le_bytes()),
            (16, true) => out.extend_from_slice(&(value as u16).to_be_bytes()),
            _ => out.push(value as u8),
        }
    }
}

fn server_init(stream: &mut TcpStream, first: &Picture) -> std::io::Result<()> {
    let name = b"windowcast";
    let mut init = Vec::with_capacity(24 + name.len());
    init.extend_from_slice(&(first.width as u16).to_be_bytes());
    init.extend_from_slice(&(first.height as u16).to_be_bytes());
    init.extend_from_slice(&PixelFormat::SERVER.bytes());
    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init.extend_from_slice(name);
    stream.write_all(&init)
}

/// What the reader hands the updater.
enum Wanted {
    Update { incremental: bool },
    Format(Option<PixelFormat>),
    Resize(bool),
}

/// Reads a viewer's messages: input to the host, the rest to the updater.
struct Reader {
    stream: TcpStream,
    window: WindowId,
    deliver: Sender<(InputEvent, Option<WindowId>)>,
    requests: Sender<Wanted>,
    /// The framebuffer's size, which pointer positions are in.
    size: (u32, u32),
    /// The pointer buttons held, RFB's mask.
    buttons: u8,
    keys: keys::Modifiers,
}

impl Reader {
    fn run(mut self) {
        let _ = self.read();
    }

    fn input(&self, event: InputEvent) {
        let _ = self.deliver.send((event, Some(self.window)));
    }

    fn read(&mut self) -> std::io::Result<()> {
        loop {
            let mut kind = [0u8; 1];
            self.stream.read_exact(&mut kind)?;
            match kind[0] {
                // SetPixelFormat
                0 => {
                    let mut raw = [0u8; 19];
                    self.stream.read_exact(&mut raw)?;
                    let format: [u8; 16] = raw[3..19].try_into().expect("16 bytes");
                    let _ = self
                        .requests
                        .send(Wanted::Format(PixelFormat::parse(&format)));
                }
                // SetEncodings
                2 => {
                    let mut head = [0u8; 3];
                    self.stream.read_exact(&mut head)?;
                    let count = u16::from_be_bytes([head[1], head[2]]) as usize;
                    let mut list = vec![0u8; count * 4];
                    self.stream.read_exact(&mut list)?;
                    let resize = list
                        .chunks(4)
                        .any(|e| i32::from_be_bytes([e[0], e[1], e[2], e[3]]) == DESKTOP_SIZE);
                    let _ = self.requests.send(Wanted::Resize(resize));
                }
                // FramebufferUpdateRequest
                3 => {
                    let mut raw = [0u8; 9];
                    self.stream.read_exact(&mut raw)?;
                    let _ = self.requests.send(Wanted::Update {
                        incremental: raw[0] != 0,
                    });
                }
                // KeyEvent
                4 => {
                    let mut raw = [0u8; 7];
                    self.stream.read_exact(&mut raw)?;
                    let down = raw[0] != 0;
                    let keysym = u32::from_be_bytes([raw[3], raw[4], raw[5], raw[6]]);
                    for event in self.keys.event(keysym, down) {
                        self.input(event);
                    }
                }
                // PointerEvent
                5 => {
                    let mut raw = [0u8; 5];
                    self.stream.read_exact(&mut raw)?;
                    let mask = raw[0];
                    let x = u16::from_be_bytes([raw[1], raw[2]]);
                    let y = u16::from_be_bytes([raw[3], raw[4]]);
                    self.pointer(mask, x, y);
                }
                // ClientCutText: the session's clipboard is the session's.
                6 => {
                    let mut head = [0u8; 7];
                    self.stream.read_exact(&mut head)?;
                    let length = u32::from_be_bytes([head[3], head[4], head[5], head[6]]);
                    std::io::copy(
                        &mut (&mut self.stream).take(u64::from(length)),
                        &mut std::io::sink(),
                    )?;
                }
                other => {
                    return Err(std::io::Error::other(format!(
                        "unknown viewer message {other}"
                    )))
                }
            }
        }
    }

    fn pointer(&mut self, mask: u8, x: u16, y: u16) {
        let window = self.window;
        let fx = f32::from(x) / self.size.0.saturating_sub(1).max(1) as f32;
        let fy = f32::from(y) / self.size.1.saturating_sub(1).max(1) as f32;
        self.input(InputEvent::PointerMove {
            window,
            x: fx.clamp(0.0, 1.0),
            y: fy.clamp(0.0, 1.0),
        });
        let pressed = mask & !self.buttons;
        let released = self.buttons & !mask;
        for (bit, button) in [
            (0, PointerButton::Left),
            (1, PointerButton::Middle),
            (2, PointerButton::Right),
        ] {
            if pressed & (1 << bit) != 0 || released & (1 << bit) != 0 {
                self.input(InputEvent::PointerButton {
                    window,
                    button,
                    pressed: pressed & (1 << bit) != 0,
                });
            }
        }
        // Buttons 4 to 7 are the wheel, a click a notch.
        for (bit, dx, dy) in [(3, 0.0, 1.0), (4, 0.0, -1.0), (5, -1.0, 0.0), (6, 1.0, 0.0)] {
            if pressed & (1 << bit) != 0 {
                self.input(InputEvent::PointerScroll { window, dx, dy });
            }
        }
        self.buttons = mask;
    }
}

/// Answers the viewer's update requests with what changed.
struct Updates {
    stream: TcpStream,
    format: PixelFormat,
    /// The viewer follows DesktopSize.
    resize: bool,
    last: Picture,
    /// The framebuffer size the viewer has, once an update was sent.
    sent_size: Option<(u32, u32)>,
    wanted: Receiver<Wanted>,
    stop: Arc<AtomicBool>,
}

impl Updates {
    fn run(mut self, mut pictures: Box<dyn windowcast_host::PictureSource>) -> std::io::Result<()> {
        let mut previous: Option<Picture> = None;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                // Ends the reader too: compatibility mode is off.
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
                return Ok(());
            }
            let incremental = match self.wanted.recv_timeout(Duration::from_millis(500)) {
                Ok(Wanted::Update { incremental }) => incremental,
                Ok(Wanted::Format(Some(format))) => {
                    self.format = format;
                    previous = None;
                    continue;
                }
                Ok(Wanted::Format(None)) => {
                    return Err(std::io::Error::other(
                        "the viewer asked for a colour-map pixel format",
                    ))
                }
                Ok(Wanted::Resize(on)) => {
                    self.resize = on;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            };
            if incremental && previous.is_some() {
                // Waits for a change (or the source's once-a-second picture).
                self.last = pictures
                    .next_picture()
                    .ok_or_else(|| std::io::Error::other("the window is gone"))?;
            } else {
                previous = None;
            }
            self.send(previous.as_ref())?;
            previous = Some(self.last.clone());
        }
    }

    /// One FramebufferUpdate: the part of the picture that differs from
    /// `previous` (all of it without one), after a DesktopSize when the
    /// window changed size and the viewer can follow.
    fn send(&mut self, previous: Option<&Picture>) -> std::io::Result<()> {
        let picture = &self.last;
        let size = (picture.width, picture.height);
        let mut rects: Vec<u8> = Vec::new();
        let mut count = 0u16;
        let mut previous = previous;
        let shown = match self.sent_size {
            Some(sent) if sent != size && self.resize => {
                rects.extend_from_slice(&[0, 0, 0, 0]);
                rects.extend_from_slice(&(size.0 as u16).to_be_bytes());
                rects.extend_from_slice(&(size.1 as u16).to_be_bytes());
                rects.extend_from_slice(&DESKTOP_SIZE.to_be_bytes());
                count += 1;
                previous = None;
                size
            }
            Some(sent) => sent,
            None => size,
        };
        let rect = match previous {
            Some(before) if (before.width, before.height) == size => changed(before, picture),
            _ => Some((0, 0, shown.0, shown.1)),
        };
        if let Some((x, y, w, h)) = rect {
            rects.extend_from_slice(&(x as u16).to_be_bytes());
            rects.extend_from_slice(&(y as u16).to_be_bytes());
            rects.extend_from_slice(&(w as u16).to_be_bytes());
            rects.extend_from_slice(&(h as u16).to_be_bytes());
            rects.extend_from_slice(&0i32.to_be_bytes());
            rects.reserve((w * h * 4) as usize);
            for row in y..y + h {
                for column in x..x + w {
                    // Outside a smaller picture (a viewer that cannot
                    // resize): black.
                    if row < picture.height && column < picture.width {
                        let at = row as usize * picture.stride + column as usize * 4;
                        self.format.put(&picture.data[at..at + 4], &mut rects);
                    } else {
                        self.format.put(&[0, 0, 0, 0], &mut rects);
                    }
                }
            }
            count += 1;
        }
        self.sent_size = Some(shown);
        let mut message = Vec::with_capacity(4 + rects.len());
        message.extend_from_slice(&[0, 0]);
        message.extend_from_slice(&count.to_be_bytes());
        message.extend_from_slice(&rects);
        self.stream.write_all(&message)
    }
}

/// The smallest rectangle (x, y, width, height) holding every pixel that
/// differs between two pictures of one size; `None` when none does.
fn changed(before: &Picture, after: &Picture) -> Option<(u32, u32, u32, u32)> {
    let row_bytes = after.width as usize * 4;
    fn row(p: &Picture, y: u32, bytes: usize) -> &[u8] {
        &p.data[y as usize * p.stride..][..bytes]
    }
    let row = |p, y| row(p, y, row_bytes);
    let differs = |y: u32| row(before, y) != row(after, y);
    let top = (0..after.height).find(|&y| differs(y))?;
    let bottom = (0..after.height).rev().find(|&y| differs(y))?;
    let mut left = after.width;
    let mut right = 0;
    for y in top..=bottom {
        let (a, b) = (row(before, y), row(after, y));
        if let Some(first) =
            (0..after.width).find(|&x| a[x as usize * 4..][..4] != b[x as usize * 4..][..4])
        {
            left = left.min(first);
            let last = (0..after.width)
                .rev()
                .find(|&x| a[x as usize * 4..][..4] != b[x as usize * 4..][..4])
                .expect("one differs");
            right = right.max(last);
        }
    }
    Some((left, top, right - left + 1, bottom - top + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32, fill: u8) -> Picture {
        Picture {
            width,
            height,
            stride: width as usize * 4,
            data: vec![fill; (width * height * 4) as usize],
        }
    }

    #[test]
    fn only_the_changed_rectangle_is_sent() {
        let before = picture(10, 8, 0);
        let mut after = before.clone();
        assert_eq!(changed(&before, &after), None);
        for (x, y) in [(2, 3), (6, 5)] {
            let at = y * after.stride + x * 4;
            after.data[at] = 9;
        }
        assert_eq!(changed(&before, &after), Some((2, 3, 5, 3)));
    }

    #[test]
    fn pixels_follow_the_viewers_format() {
        let mut out = Vec::new();
        PixelFormat::SERVER.put(&[1, 2, 3, 255], &mut out);
        assert_eq!(out, [1, 2, 3, 0]);
        // 16-bit 5-6-5, little endian: pure red.
        let rgb565 = PixelFormat {
            bits: 16,
            big_endian: false,
            max: [31, 63, 31],
            shift: [11, 5, 0],
        };
        out.clear();
        rgb565.put(&[0, 0, 255, 255], &mut out);
        assert_eq!(out, 0xf800u16.to_le_bytes());
        assert_eq!(
            PixelFormat::parse(&rgb565.bytes()).map(|f| f.max),
            Some([31, 63, 31])
        );
    }

    #[test]
    fn vnc_authentication_is_des_with_the_key_bits_reversed() {
        // FIPS 81's example: key 0123456789ABCDEF, "Now is t" gives
        // 3FA40E8A984D4815. VNC reverses each password byte's bits first,
        // so this password is that key reversed.
        let password: Vec<u8> = [0x01u8, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
            .iter()
            .map(|b| b.reverse_bits())
            .collect();
        let mut challenge = [0u8; 16];
        challenge[..8].copy_from_slice(b"Now is t");
        challenge[8..].copy_from_slice(b"Now is t");
        let response = vnc_auth_response(&password, &challenge);
        let expected = [0x3f, 0xa4, 0x0e, 0x8a, 0x98, 0x4d, 0x48, 0x15];
        assert_eq!(&response[..8], &expected);
        assert_eq!(&response[8..], &expected);
    }
}
