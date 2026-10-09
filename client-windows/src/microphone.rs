//! Sending this PC's microphone to the host: the default recording device
//! through WASAPI (48 kHz stereo 16-bit, Windows converting), cut into
//! 20 ms Opus packets for the session's microphone track.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windowcast_client::ClientSession;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

/// The microphone being sent; dropping it stops sending.
pub struct Microphone {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Result<(), String>>>,
}

impl Microphone {
    /// Starts sending the default recording device's sound over `session`.
    pub fn start(session: Arc<ClientSession>) -> Result<Self, String> {
        session
            .start_microphone()
            .map_err(|e| format!("microphone: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let result = run(&session, &stop);
                let _ = session.stop_microphone();
                result
            })
        };
        Ok(Microphone {
            stop,
            thread: Some(thread),
        })
    }

    /// Why sending stopped by itself, if it did.
    pub fn failed(&mut self) -> Option<String> {
        if self.thread.as_ref().is_some_and(|t| t.is_finished()) {
            return self.thread.take()?.join().ok()?.err();
        }
        None
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(session: &ClientSession, stop: &AtomicBool) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(err("audio devices"))?;
        let device = devices
            .GetDefaultAudioEndpoint(eCapture, eCommunications)
            .map_err(err("no microphone"))?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(err("microphone"))?;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                1_000_000,
                0,
                &format,
                None,
            )
            .map_err(err("microphone initialize"))?;
        let capture: IAudioCaptureClient = client.GetService().map_err(err("capture"))?;
        client.Start().map_err(err("microphone start"))?;
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip)
                .map_err(|e| format!("opus: {e}"))?;
        let mut pending: Vec<i16> = Vec::new();
        let mut packet = vec![0u8; 4000];
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
            loop {
                let size = capture.GetNextPacketSize().map_err(err("microphone"))?;
                if size == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(err("microphone"))?;
                let count = frames as usize * 2;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    pending.resize(pending.len() + count, 0);
                } else {
                    pending
                        .extend_from_slice(std::slice::from_raw_parts(data as *const i16, count));
                }
                let _ = capture.ReleaseBuffer(frames);
            }
            while pending.len() >= 1920 {
                let len = encoder
                    .encode(&pending[..1920], &mut packet)
                    .map_err(|e| format!("opus: {e}"))?;
                pending.drain(..1920);
                session
                    .send_microphone(&packet[..len])
                    .map_err(|e| format!("microphone: {e}"))?;
            }
        }
        let _ = client.Stop();
    }
    Ok(())
}
