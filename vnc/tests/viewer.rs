//! A VNC viewer's view of compatibility mode's VNC endpoint, spoken here
//! byte by byte as RFC 6143 has it: the password made for the window lets
//! the viewer in (a wrong one does not), the first update is the whole
//! window, the next only what changed, and the viewer's keys and pointer
//! reach the window.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windowcast_host::compat::CompatProvider;
use windowcast_host::{FrameSource, Picture, PictureSource, WindowSource};
use windowcast_protocol::{CompatEndpoint, InputEvent, VideoCodec, WindowId, WindowInfo};
use windowcast_vnc::{vnc_auth_response, VncCompat};

const WINDOW: WindowId = WindowId(4);

/// A 16x8 window: blue, then a red pixel at (5, 2) in every later picture.
struct Window {
    input: Arc<Mutex<Vec<InputEvent>>>,
}

struct Pictures {
    sent: u32,
}

impl PictureSource for Pictures {
    fn next_picture(&mut self) -> Option<Picture> {
        if self.sent > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut data = Vec::new();
        for _ in 0..16 * 8 {
            data.extend_from_slice(&[255, 0, 0, 255]);
        }
        if self.sent > 0 {
            let at = (2 * 16 + 5) * 4;
            data[at..at + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
        self.sent += 1;
        Some(Picture {
            width: 16,
            height: 8,
            stride: 64,
            data,
        })
    }
}

impl WindowSource for Window {
    fn list_windows(&self) -> Vec<WindowInfo> {
        Vec::new()
    }
    fn encoders(&self) -> Vec<VideoCodec> {
        Vec::new()
    }
    fn open(&self, _: WindowId, _: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        Err("pictures only".into())
    }
    fn open_pictures(&self, window: WindowId) -> Option<Result<Box<dyn PictureSource>, String>> {
        assert_eq!(window, WINDOW);
        Some(Ok(Box::new(Pictures { sent: 0 })))
    }
    fn input(&self, event: &InputEvent, _focus: Option<WindowId>) {
        self.input.lock().unwrap().push(event.clone());
    }
}

fn read<const N: usize>(stream: &mut TcpStream) -> [u8; N] {
    let mut out = [0u8; N];
    stream.read_exact(&mut out).unwrap();
    out
}

/// Version and security, up to the SecurityResult.
fn log_in(port: u16, password: &str) -> (TcpStream, u32) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert_eq!(&read::<12>(&mut stream), b"RFB 003.008\n");
    stream.write_all(b"RFB 003.008\n").unwrap();
    assert_eq!(read::<2>(&mut stream), [1, 2], "VNC Authentication only");
    stream.write_all(&[2]).unwrap();
    let challenge = read::<16>(&mut stream);
    stream
        .write_all(&vnc_auth_response(password.as_bytes(), &challenge))
        .unwrap();
    let result = u32::from_be_bytes(read::<4>(&mut stream));
    (stream, result)
}

/// One FramebufferUpdate's rectangles: x, y, width, height, encoding and
/// pixels.
fn update(stream: &mut TcpStream) -> Vec<(u16, u16, u16, u16, i32, Vec<u8>)> {
    let head = read::<4>(stream);
    assert_eq!(head[0], 0, "FramebufferUpdate");
    let count = u16::from_be_bytes([head[2], head[3]]);
    (0..count)
        .map(|_| {
            let r = read::<12>(stream);
            let (x, y, w, h) = (
                u16::from_be_bytes([r[0], r[1]]),
                u16::from_be_bytes([r[2], r[3]]),
                u16::from_be_bytes([r[4], r[5]]),
                u16::from_be_bytes([r[6], r[7]]),
            );
            let encoding = i32::from_be_bytes([r[8], r[9], r[10], r[11]]);
            let mut pixels = vec![
                0u8;
                if encoding == 0 {
                    w as usize * h as usize * 4
                } else {
                    0
                }
            ];
            stream.read_exact(&mut pixels).unwrap();
            (x, y, w, h, encoding, pixels)
        })
        .collect()
}

fn request(stream: &mut TcpStream, incremental: bool) {
    let mut message = vec![3, u8::from(incremental)];
    message.extend_from_slice(&[0, 0, 0, 0, 0, 16, 0, 8]);
    stream.write_all(&message).unwrap();
}

#[test]
fn a_vnc_viewer_sees_the_window_and_drives_it() {
    let input = Arc::new(Mutex::new(Vec::new()));
    let source = Arc::new(Window {
        input: Arc::clone(&input),
    });
    let vnc = VncCompat::new(source, std::net::IpAddr::from([127, 0, 0, 1]));
    let (endpoint, guard) = vnc.open(WINDOW, "test").unwrap();
    let CompatEndpoint::Vnc { port, password } = endpoint else {
        panic!("not a VNC endpoint: {endpoint:?}");
    };
    assert!((5900..6000).contains(&port));
    assert_eq!(password.len(), 8);

    let (_, refused) = log_in(port, "wrong");
    assert_eq!(refused, 1, "a wrong password is refused");

    let (mut stream, result) = log_in(port, &password);
    assert_eq!(result, 0, "the right password is taken");
    stream.write_all(&[1]).unwrap(); // ClientInit, shared
    let init = read::<24>(&mut stream);
    assert_eq!(u16::from_be_bytes([init[0], init[1]]), 16);
    assert_eq!(u16::from_be_bytes([init[2], init[3]]), 8);
    let name_length = u32::from_be_bytes([init[20], init[21], init[22], init[23]]);
    let mut name = vec![0u8; name_length as usize];
    stream.read_exact(&mut name).unwrap();
    assert_eq!(name, b"windowcast");

    // The whole window, blue (BGRx).
    request(&mut stream, false);
    let first = update(&mut stream);
    assert_eq!(first.len(), 1);
    let (x, y, w, h, encoding, pixels) = &first[0];
    assert_eq!((*x, *y, *w, *h, *encoding), (0, 0, 16, 8, 0));
    assert_eq!(&pixels[..4], &[255, 0, 0, 0]);

    // Then only the pixel that changed, red.
    request(&mut stream, true);
    let next = update(&mut stream);
    assert_eq!(next.len(), 1);
    let (x, y, w, h, _, pixels) = &next[0];
    assert_eq!((*x, *y, *w, *h), (5, 2, 1, 1));
    assert_eq!(&pixels[..], &[0, 0, 255, 0]);

    // Keys and the pointer reach the window: "a", Enter, a click at the
    // middle.
    let mut events = Vec::new();
    for (down, keysym) in [(1u8, 0x61u32), (0, 0x61), (1, 0xff0d), (0, 0xff0d)] {
        events.push(4);
        events.push(down);
        events.extend_from_slice(&[0, 0]);
        events.extend_from_slice(&keysym.to_be_bytes());
    }
    for mask in [1u8, 0] {
        events.push(5);
        events.push(mask);
        events.extend_from_slice(&15u16.to_be_bytes());
        events.extend_from_slice(&7u16.to_be_bytes());
    }
    stream.write_all(&events).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while input.lock().unwrap().len() < 7 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let got = input.lock().unwrap().clone();
    assert_eq!(got[0], InputEvent::Text { text: "a".into() });
    assert_eq!(
        got[1],
        InputEvent::Key {
            keycode: 28,
            pressed: true
        }
    );
    assert!(got.contains(&InputEvent::PointerMove {
        window: WINDOW,
        x: 1.0,
        y: 1.0
    }));
    assert!(got.contains(&InputEvent::PointerButton {
        window: WINDOW,
        button: windowcast_protocol::PointerButton::Left,
        pressed: true
    }));

    // Closed with the window's compatibility mode.
    drop(guard);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "still listening"
    );
}
