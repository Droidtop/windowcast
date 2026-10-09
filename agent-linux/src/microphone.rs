//! The client's microphone on Linux: a virtual microphone made through the
//! PulseAudio API (PulseAudio, or PipeWire's pulse server), a null sink
//! the client's sound plays into and a source remapped from its monitor,
//! "windowcast-microphone", that applications record from like any
//! microphone. Both are unloaded again when the client stops.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet as ContextFlags, State as ContextState};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::operation::{Operation, State as OperationState};
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlags, SeekMode, State as StreamState, Stream};
use windowcast_host::audio::MicrophoneSink;

/// The source applications record from, and the sink behind it.
pub const SOURCE: &str = "windowcast_microphone";
const SINK: &str = "windowcast_microphone_sink";

pub struct LinuxMicrophone {
    samples: Sender<Vec<i16>>,
}

impl LinuxMicrophone {
    /// Makes the virtual microphone on a thread that owns the Pulse
    /// connection (its objects are not Send).
    pub fn open() -> Result<Self, String> {
        let (samples, received) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut device = match Device::open() {
                Ok(device) => {
                    let _ = ready_tx.send(Ok(()));
                    device
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            device.run(received);
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the Pulse server did not answer".to_owned())??;
        Ok(LinuxMicrophone { samples })
    }
}

impl MicrophoneSink for LinuxMicrophone {
    fn play(&mut self, samples: &[i16]) {
        let _ = self.samples.send(samples.to_vec());
    }
}

/// Fields drop in order: stream, context, main loop.
struct Device {
    stream: Option<Stream>,
    modules: Vec<u32>,
    context: Context,
    mainloop: Mainloop,
}

impl Device {
    fn open() -> Result<Self, String> {
        let mut mainloop = Mainloop::new().ok_or("no Pulse main loop")?;
        let mut context = Context::new(&mainloop, "windowcast").ok_or("no Pulse context")?;
        context
            .connect(None, ContextFlags::NOAUTOSPAWN, None)
            .map_err(|e| format!("no Pulse server: {e}"))?;
        loop {
            match context.get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated => {
                    return Err("could not connect to the Pulse server".into())
                }
                _ => {}
            }
            if let IterateResult::Err(e) = mainloop.iterate(true) {
                return Err(format!("Pulse: {e}"));
            }
        }
        let mut device = Device {
            stream: None,
            modules: Vec::new(),
            context,
            mainloop,
        };
        let speakers = device.default_sink()?;
        device.load(
            "module-null-sink",
            &format!(
                "sink_name={SINK} sink_properties=device.description=windowcast-microphone-sink"
            ),
        )?;
        device.load(
            "module-remap-source",
            &format!(
                "master={SINK}.monitor source_name={SOURCE} source_properties=device.description=windowcast-microphone"
            ),
        )?;
        device.keep_speakers(speakers)?;
        let spec = Spec {
            format: Format::S16le,
            channels: 2,
            rate: 48_000,
        };
        let mut stream = Stream::new(&mut device.context, "client microphone", &spec, None)
            .ok_or("no Pulse stream")?;
        stream
            .connect_playback(Some(SINK), None, StreamFlags::NOFLAGS, None, None)
            .map_err(|e| format!("playback: {e}"))?;
        loop {
            match stream.get_state() {
                StreamState::Ready => break,
                StreamState::Failed | StreamState::Terminated => {
                    return Err("the playback stream failed".into())
                }
                _ => {}
            }
            if let IterateResult::Err(e) = device.mainloop.iterate(true) {
                return Err(format!("Pulse: {e}"));
            }
        }
        device.stream = Some(stream);
        Ok(device)
    }

    /// Runs the main loop until `operation` is done.
    fn wait<C: ?Sized>(&mut self, operation: &Operation<C>) -> Result<(), String> {
        while operation.get_state() == OperationState::Running {
            if let IterateResult::Err(e) = self.mainloop.iterate(true) {
                return Err(format!("Pulse: {e}"));
            }
        }
        Ok(())
    }

    fn default_sink(&mut self) -> Result<Option<String>, String> {
        let name = Rc::new(RefCell::new(None));
        let into = Rc::clone(&name);
        let operation = self.context.introspect().get_server_info(move |info| {
            *into.borrow_mut() = info.default_sink_name.as_ref().map(|n| n.to_string());
        });
        self.wait(&operation)?;
        Ok(name.take())
    }

    fn sink_index(&mut self, name: &str) -> Result<Option<u32>, String> {
        let index = Rc::new(Cell::new(None));
        let into = Rc::clone(&index);
        let operation = self
            .context
            .introspect()
            .get_sink_info_by_name(name, move |result| {
                if let ListResult::Item(sink) = result {
                    into.set(Some(sink.index));
                }
            });
        self.wait(&operation)?;
        Ok(index.get())
    }

    /// A new sink can take over as the default and pull the desktop's
    /// playback with it (module-switch-on-connect does; so does
    /// module-always-sink, giving up its placeholder when nothing else
    /// plays), which would send the desktop's sound into the virtual
    /// microphone. Puts the default back and moves those streams back to
    /// it. Where the speakers were only that placeholder there is nothing to
    /// go back to.
    fn keep_speakers(&mut self, speakers: Option<String>) -> Result<(), String> {
        let Some(speakers) = speakers.filter(|s| s != SINK) else {
            return Ok(());
        };
        let (Some(to), Some(ours)) = (self.sink_index(&speakers)?, self.sink_index(SINK)?) else {
            return Ok(());
        };
        if self.default_sink()?.as_deref() == Some(SINK) {
            let operation = self.context.set_default_sink(&speakers, |_| {});
            self.wait(&operation)?;
        }
        let moved = Rc::new(RefCell::new(Vec::new()));
        let into = Rc::clone(&moved);
        let operation = self
            .context
            .introspect()
            .get_sink_input_info_list(move |result| {
                if let ListResult::Item(input) = result {
                    if input.sink == ours {
                        into.borrow_mut().push(input.index);
                    }
                }
            });
        self.wait(&operation)?;
        for input in moved.take() {
            let operation = self
                .context
                .introspect()
                .move_sink_input_by_index(input, to, None);
            self.wait(&operation)?;
        }
        Ok(())
    }

    /// Loads a module and remembers it for unloading.
    fn load(&mut self, name: &str, arguments: &str) -> Result<(), String> {
        let index = Rc::new(Cell::new(None));
        let into = Rc::clone(&index);
        let operation = self
            .context
            .introspect()
            .load_module(name, arguments, move |i| into.set(Some(i)));
        self.wait(&operation)?;
        match index.get() {
            Some(i) if i != u32::MAX => {
                self.modules.push(i);
                Ok(())
            }
            _ => Err(format!("the Pulse server would not load {name}")),
        }
    }

    /// Plays what arrives until the client stops.
    fn run(&mut self, samples: Receiver<Vec<i16>>) {
        loop {
            match samples.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => {
                    let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                    if let Some(stream) = self.stream.as_mut() {
                        let _ = stream.write(&bytes, None, 0, SeekMode::Relative);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            while let IterateResult::Success(n) = self.mainloop.iterate(false) {
                if n == 0 {
                    break;
                }
            }
        }
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.disconnect();
        }
        for module in std::mem::take(&mut self.modules).into_iter().rev() {
            let operation = self.context.introspect().unload_module(module, |_| {});
            while operation.get_state() == OperationState::Running {
                if let IterateResult::Err(_) = self.mainloop.iterate(true) {
                    break;
                }
            }
        }
    }
}
