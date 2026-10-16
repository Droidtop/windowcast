//! A terminal started as another user. portable-pty starts its program
//! with CreateProcessW, as whoever calls it, so a host running as SYSTEM
//! starts `windowcast-agent-windows pty-relay` as the signed-in user
//! (`crate::user_process`), and the relay opens the terminal: it runs the
//! shell on a pseudo-terminal of its own, writes the terminal's output to
//! its standard output, and reads frames from its standard input, typed
//! text and resizes. It ends with the shell's exit code. Plain std and
//! windowcast-pty, so it is tested on every platform.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use windowcast_pty::{Pty, Size, Spawn};

/// The relay's subcommand.
pub const SUBCOMMAND: &str = "pty-relay";

/// What the host sends a relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Typed into the terminal.
    Data(Vec<u8>),
    Resize(Size),
}

const DATA: u8 = 0;
const RESIZE: u8 = 1;
/// More than a command channel's chunk ever is.
const MAX_DATA: usize = 1 << 20;

pub fn encode(frame: &Frame) -> Vec<u8> {
    match frame {
        Frame::Data(bytes) => {
            let mut out = Vec::with_capacity(5 + bytes.len());
            out.push(DATA);
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
            out
        }
        Frame::Resize(size) => {
            let mut out = vec![RESIZE];
            out.extend_from_slice(&size.cols.to_le_bytes());
            out.extend_from_slice(&size.rows.to_le_bytes());
            out
        }
    }
}

/// The next frame; `None` when the host closed the relay's input.
pub fn read_frame(input: &mut impl Read) -> std::io::Result<Option<Frame>> {
    let mut tag = [0u8; 1];
    if input.read(&mut tag)? == 0 {
        return Ok(None);
    }
    match tag[0] {
        DATA => {
            let mut len = [0u8; 4];
            input.read_exact(&mut len)?;
            let len = u32::from_le_bytes(len) as usize;
            if len > MAX_DATA {
                return Err(std::io::Error::other("relay frame too large"));
            }
            let mut bytes = vec![0u8; len];
            input.read_exact(&mut bytes)?;
            Ok(Some(Frame::Data(bytes)))
        }
        RESIZE => {
            let mut size = [0u8; 4];
            input.read_exact(&mut size)?;
            Ok(Some(Frame::Resize(Size {
                cols: u16::from_le_bytes([size[0], size[1]]),
                rows: u16::from_le_bytes([size[2], size[3]]),
            })))
        }
        other => Err(std::io::Error::other(format!(
            "unknown relay frame {other}"
        ))),
    }
}

/// The relay's arguments for `spawn` (its variables are the relay's own
/// environment, which the host gave it): `--cols C --rows R --term T`,
/// then `-- program args...` unless it is the user's shell.
pub fn args(spawn: &Spawn) -> Vec<String> {
    let mut args = vec![
        SUBCOMMAND.to_owned(),
        "--cols".into(),
        spawn.size.cols.to_string(),
        "--rows".into(),
        spawn.size.rows.to_string(),
        "--term".into(),
        spawn.term.clone(),
    ];
    if let Some(program) = &spawn.program {
        args.push("--".into());
        args.extend(program.iter().cloned());
    }
    args
}

/// Reads [`args`] back (without the subcommand).
pub fn parse_args(args: &[String]) -> Result<Spawn, String> {
    let mut spawn = Spawn {
        size: Size { cols: 80, rows: 24 },
        term: String::new(),
        program: None,
        env: Vec::new(),
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--cols" => spawn.size.cols = value()?.parse().map_err(|_| "--cols needs a number")?,
            "--rows" => spawn.size.rows = value()?.parse().map_err(|_| "--rows needs a number")?,
            "--term" => spawn.term = value()?.clone(),
            "--" => {
                let program: Vec<String> = args.by_ref().cloned().collect();
                if program.is_empty() {
                    return Err("-- needs a program".into());
                }
                spawn.program = Some(program);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(spawn)
}

/// Runs the terminal until its shell ends; the shell's exit code (1 when
/// it gave none). Ending the input ends the shell.
pub fn serve(
    spawn: &Spawn,
    mut input: impl Read + Send + 'static,
    mut output: impl Write + Send + 'static,
) -> i32 {
    let pty = match Pty::spawn(spawn) {
        Ok(pty) => Arc::new(pty),
        Err(e) => {
            eprintln!("could not start the shell: {e}");
            return 1;
        }
    };
    let Some(mut reader) = pty.take_reader() else {
        eprintln!("no terminal output");
        return 1;
    };
    let (drained, drained_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if output
                        .write_all(&buf[..n])
                        .and_then(|()| output.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = drained.send(());
    });
    {
        let pty = Arc::clone(&pty);
        std::thread::spawn(move || loop {
            match read_frame(&mut input) {
                Ok(Some(Frame::Data(bytes))) => {
                    if pty.write(&bytes).is_err() {
                        return;
                    }
                }
                Ok(Some(Frame::Resize(size))) => {
                    let _ = pty.resize(size);
                }
                Ok(None) | Err(_) => {
                    pty.kill();
                    return;
                }
            }
        });
    }
    let code = pty.wait().unwrap_or(1);
    // ConPTY may hold its output open until the pseudo-console closes:
    // what the shell wrote has had its moment by then.
    let _ = drained_rx.recv_timeout(Duration::from_millis(500));
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_read_back() {
        let frames = [
            Frame::Data(b"echo hi\r".to_vec()),
            Frame::Resize(Size {
                cols: 132,
                rows: 43,
            }),
            Frame::Data(Vec::new()),
        ];
        let bytes: Vec<u8> = frames.iter().flat_map(encode).collect();
        let mut input = &bytes[..];
        for frame in &frames {
            assert_eq!(read_frame(&mut input).unwrap().as_ref(), Some(frame));
        }
        assert_eq!(read_frame(&mut input).unwrap(), None);
    }

    #[test]
    fn an_oversized_or_unknown_frame_is_refused() {
        let mut big = vec![DATA];
        big.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_frame(&mut &big[..]).is_err());
        assert!(read_frame(&mut &[9u8][..]).is_err());
    }

    #[test]
    fn arguments_read_back() {
        for program in [
            None,
            Some(vec!["cmd.exe".to_owned(), "/k".into(), "a b".into()]),
        ] {
            let spawn = Spawn {
                size: Size {
                    cols: 100,
                    rows: 30,
                },
                term: "xterm-256color".into(),
                program,
                env: Vec::new(),
            };
            let args = args(&spawn);
            assert_eq!(args[0], SUBCOMMAND);
            let back = parse_args(&args[1..]).unwrap();
            assert_eq!(back.size, spawn.size);
            assert_eq!(back.term, spawn.term);
            assert_eq!(back.program, spawn.program);
        }
        assert!(parse_args(&["--".to_owned()]).is_err());
        assert!(parse_args(&["--cols".to_owned(), "x".into()]).is_err());
    }

    /// A pipe that ends when its writer is dropped.
    #[cfg(unix)]
    fn pipe() -> (std::sync::mpsc::Sender<Vec<u8>>, impl Read + Send + 'static) {
        struct Channel(std::sync::mpsc::Receiver<Vec<u8>>, Vec<u8>);
        impl Read for Channel {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                while self.1.is_empty() {
                    match self.0.recv() {
                        Ok(bytes) => self.1 = bytes,
                        Err(_) => return Ok(0),
                    }
                }
                let n = buf.len().min(self.1.len());
                buf[..n].copy_from_slice(&self.1[..n]);
                self.1.drain(..n);
                Ok(n)
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        (tx, Channel(rx, Vec::new()))
    }

    #[cfg(unix)]
    #[derive(Clone, Default)]
    struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);
    #[cfg(unix)]
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_relayed_shell_types_resizes_and_exits_with_its_code() {
        let spawn = Spawn {
            size: Size { cols: 90, rows: 20 },
            term: "xterm".into(),
            program: Some(vec!["/bin/sh".into()]),
            env: Vec::new(),
        };
        let (input, relay_input) = pipe();
        let output = Shared::default();
        let relay = {
            let output = output.clone();
            std::thread::spawn(move || serve(&spawn, relay_input, output))
        };
        let seen = |needle: &str| {
            for _ in 0..200 {
                if String::from_utf8_lossy(&output.0.lock().unwrap()).contains(needle) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!(
                "no {needle:?} in {:?}",
                String::from_utf8_lossy(&output.0.lock().unwrap())
            );
        };
        input
            .send(encode(&Frame::Data(
                b"echo relay-$((40+2)); stty size\n".to_vec(),
            )))
            .unwrap();
        seen("relay-42");
        seen("20 90");
        input
            .send(encode(&Frame::Resize(Size { cols: 61, rows: 17 })))
            .unwrap();
        input
            .send(encode(&Frame::Data(b"stty size; exit 5\n".to_vec())))
            .unwrap();
        assert_eq!(relay.join().unwrap(), 5);
        seen("17 61");
    }

    #[cfg(unix)]
    #[test]
    fn closing_the_input_ends_the_shell() {
        let spawn = Spawn {
            size: Size { cols: 80, rows: 24 },
            term: String::new(),
            program: Some(vec!["/bin/sh".into()]),
            env: Vec::new(),
        };
        let (input, relay_input) = pipe();
        let relay = std::thread::spawn(move || serve(&spawn, relay_input, Shared::default()));
        drop(input);
        relay.join().unwrap();
    }
}
