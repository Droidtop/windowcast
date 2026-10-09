//! A window's sound on Linux, through the PulseAudio API (PulseAudio
//! itself, or PipeWire's pulse server): the window's process comes from
//! sway's tree, its playback streams (sink inputs) are found by their
//! `application.process.id` (the process or any it started, as a browser
//! plays from a child), and one of them is recorded through a monitor
//! stream of just that sink input: what the application plays, nothing
//! else. An application that is not playing yet is looked for again every
//! two seconds; 48 kHz stereo 16-bit, Pulse converting.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet as ContextFlags, State as ContextState};
use pulse::def::BufferAttr;
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::operation::State as OperationState;
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlags, PeekResult, State as StreamState, Stream};
use windowcast_host::audio::AudioSource;
use windowcast_protocol::WindowId;

use crate::sway::Sway;
use crate::toplevels;

/// How often an application that is not playing is looked for again.
const RESCAN: Duration = Duration::from_secs(2);

/// The process ids of `pid` and every process it started, from /proc.
fn process_tree(pid: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(child) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // "pid (comm) state ppid ...": the command may hold spaces and
            // parentheses, so read after the last ')'.
            let Some(rest) = stat.rfind(')').map(|at| &stat[at + 1..]) else {
                continue;
            };
            if let Some(parent) = rest.split_whitespace().nth(1).and_then(|p| p.parse().ok()) {
                children.entry(parent).or_default().push(child);
            }
        }
    }
    let mut tree = HashSet::from([pid]);
    let mut queue = vec![pid];
    while let Some(next) = queue.pop() {
        for &child in children.get(&next).into_iter().flatten() {
            if tree.insert(child) {
                queue.push(child);
            }
        }
    }
    tree
}

/// The Pulse connection and the recording, made on the stream's thread
/// (the Pulse objects are not Send). Fields drop in order: stream,
/// context, then the main loop.
struct Recording {
    stream: Option<Stream>,
    context: Context,
    mainloop: Mainloop,
}

impl Recording {
    fn connect() -> Result<Self, String> {
        let mut mainloop = Mainloop::new().ok_or("no Pulse main loop")?;
        let mut context = Context::new(&mainloop, "windowcast").ok_or("no Pulse context")?;
        context
            .connect(None, ContextFlags::NOAUTOSPAWN, None)
            .map_err(|e| format!("no Pulse server: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match context.get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated => {
                    return Err("could not connect to the Pulse server".into())
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err("the Pulse server did not answer".into());
            }
            if let IterateResult::Err(e) = mainloop.iterate(true) {
                return Err(format!("Pulse: {e}"));
            }
        }
        Ok(Recording {
            stream: None,
            context,
            mainloop,
        })
    }

    /// Runs the main loop until `done` says the operation finished.
    fn wait(&mut self, state: impl Fn() -> OperationState) -> Result<(), String> {
        while state() == OperationState::Running {
            if let IterateResult::Err(e) = self.mainloop.iterate(true) {
                return Err(format!("Pulse: {e}"));
            }
        }
        Ok(())
    }

    /// A sink input played by one of `pids`, and its sink's monitor source.
    fn find(&mut self, pids: &HashSet<u32>) -> Result<Option<(u32, String)>, String> {
        let found: Rc<RefCell<Option<(u32, u32)>>> = Rc::default();
        let into = Rc::clone(&found);
        let pids = pids.clone();
        let operation = self
            .context
            .introspect()
            .get_sink_input_info_list(move |result| {
                if let ListResult::Item(info) = result {
                    let pid = info
                        .proplist
                        .get_str("application.process.id")
                        .and_then(|p| p.parse::<u32>().ok());
                    if pid.is_some_and(|pid| pids.contains(&pid)) && into.borrow().is_none() {
                        *into.borrow_mut() = Some((info.index, info.sink));
                    }
                }
            });
        self.wait(|| operation.get_state())?;
        let Some((input, sink)) = *found.borrow() else {
            return Ok(None);
        };
        let monitor: Rc<RefCell<Option<String>>> = Rc::default();
        let into = Rc::clone(&monitor);
        let operation = self
            .context
            .introspect()
            .get_sink_info_by_index(sink, move |result| {
                if let ListResult::Item(info) = result {
                    *into.borrow_mut() = info.monitor_source_name.as_ref().map(|n| n.to_string());
                }
            });
        self.wait(|| operation.get_state())?;
        let monitor = monitor.borrow().clone();
        Ok(monitor.map(|monitor| (input, monitor)))
    }

    /// Records sink input `input` through `monitor`.
    fn record(&mut self, input: u32, monitor: &str) -> Result<(), String> {
        let spec = Spec {
            format: Format::S16le,
            channels: 2,
            rate: 48_000,
        };
        let mut stream =
            Stream::new(&mut self.context, "window sound", &spec, None).ok_or("no Pulse stream")?;
        stream
            .set_monitor_stream(input)
            .map_err(|e| format!("monitor stream: {e}"))?;
        // Fragments of 20 ms, for little delay.
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: 3840,
        };
        stream
            .connect_record(
                Some(monitor),
                Some(&attr),
                StreamFlags::ADJUST_LATENCY | StreamFlags::DONT_MOVE,
            )
            .map_err(|e| format!("record: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match stream.get_state() {
                StreamState::Ready => break,
                StreamState::Failed | StreamState::Terminated => {
                    return Err("the recording failed".into())
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err("the recording did not start".into());
            }
            if let IterateResult::Err(e) = self.mainloop.iterate(true) {
                return Err(format!("Pulse: {e}"));
            }
        }
        self.stream = Some(stream);
        Ok(())
    }

    /// Up to `timeout` of the main loop, then whatever was recorded. `None`
    /// when the recording ended (the application stopped playing).
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<i16>>, String> {
        let deadline = Instant::now() + timeout;
        let mut samples = Vec::new();
        loop {
            let stream = self.stream.as_mut().expect("recording");
            match stream.get_state() {
                StreamState::Failed | StreamState::Terminated => return Ok(None),
                _ => {}
            }
            loop {
                match stream.peek().map_err(|e| format!("peek: {e}"))? {
                    PeekResult::Empty => break,
                    PeekResult::Hole(_) => stream.discard().map_err(|e| format!("discard: {e}"))?,
                    PeekResult::Data(data) => {
                        samples.extend(
                            data.as_chunks::<2>()
                                .0
                                .iter()
                                .map(|pair| i16::from_le_bytes(*pair)),
                        );
                        stream.discard().map_err(|e| format!("discard: {e}"))?;
                    }
                }
            }
            if !samples.is_empty() || Instant::now() >= deadline {
                return Ok(Some(samples));
            }
            if let IterateResult::Err(e) = self.mainloop.iterate(false) {
                return Err(format!("Pulse: {e}"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub struct WindowAudio {
    pid: u32,
    recording: Option<Recording>,
    last_scan: Option<Instant>,
    /// The last problem reported, so a rescan does not repeat it.
    reported: Option<String>,
}

// The Pulse objects in `recording` are made on, and only used from, the
// stream's thread, after the move (open_audio hands this over empty).
unsafe impl Send for WindowAudio {}

impl WindowAudio {
    /// Finds `window`'s process (needs sway); the recording starts on the
    /// stream's thread.
    pub fn open(window: WindowId) -> Result<Self, String> {
        let mut sway = Sway::connect().ok_or("window sound needs sway (no $SWAYSOCK)")?;
        let placed = sway
            .windows()
            .map_err(|e| format!("sway: {e}"))?
            .into_iter()
            .find(|w| toplevels::window_id(&w.identifier) == window)
            .ok_or("sway does not know this window")?;
        let pid = placed
            .pid
            .ok_or("sway does not know the window's process")?;
        Ok(WindowAudio {
            pid,
            recording: None,
            last_scan: None,
            reported: None,
        })
    }

    /// Starts recording if the application is playing; false if not (yet).
    fn start(&mut self) -> Result<bool, String> {
        if self.last_scan.is_some_and(|at| at.elapsed() < RESCAN) {
            return Ok(false);
        }
        self.last_scan = Some(Instant::now());
        let mut recording = Recording::connect()?;
        let Some((input, monitor)) = recording.find(&process_tree(self.pid))? else {
            return Ok(false);
        };
        recording.record(input, &monitor)?;
        self.recording = Some(recording);
        Ok(true)
    }
}

impl AudioSource for WindowAudio {
    fn next_samples(&mut self) -> Option<Vec<i16>> {
        if self.recording.is_none() {
            match self.start() {
                Ok(true) => {}
                Ok(false) => {
                    std::thread::sleep(Duration::from_millis(100));
                    return Some(Vec::new());
                }
                Err(e) => {
                    if self.reported.as_ref() != Some(&e) {
                        eprintln!("window sound: {e}");
                        self.reported = Some(e);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    return Some(Vec::new());
                }
            }
        }
        let recording = self.recording.as_mut().expect("recording");
        match recording.read(Duration::from_millis(100)) {
            Ok(Some(samples)) => Some(samples),
            // The application stopped playing: look for it again.
            Ok(None) | Err(_) => {
                self.recording = None;
                Some(Vec::new())
            }
        }
    }
}
