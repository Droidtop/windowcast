//! Playing a window's sound: its Opus packets decoded (libopus) and written
//! to the default output device through WASAPI in shared mode, which
//! converts our 48 kHz stereo float to the device's own format. A small
//! buffer absorbs network jitter; when it grows past a few packets (the
//! device or the network stalled), the oldest sound is dropped so the
//! sound stays close to the picture.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windowcast_client::{AudioPoll, ClientSession};
use windowcast_protocol::WindowId;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::Shared;

/// Samples a second, per channel, and channels.
const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// Sound buffered before playing starts, and the most kept: 60 and 150 ms.
const START_FRAMES: usize = RATE as usize * 60 / 1000;
const MAX_FRAMES: usize = RATE as usize * 150 / 1000;

fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

struct Output {
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_frames: u32,
}

impl Output {
    fn open() -> Result<Self, String> {
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(err("audio devices"))?;
            let device = devices
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(err("no audio output"))?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(err("audio output"))?;
            let format = WAVEFORMATEX {
                wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                nChannels: CHANNELS as u16,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    1_000_000, // 100 ms
                    0,
                    &format,
                    None,
                )
                .map_err(err("audio output initialize"))?;
            let buffer_frames = client.GetBufferSize().map_err(err("buffer size"))?;
            let render: IAudioRenderClient = client.GetService().map_err(err("render service"))?;
            client.Start().map_err(err("audio output start"))?;
            Ok(Output {
                client,
                render,
                buffer_frames,
            })
        }
    }

    /// Writes as much of `queue` (interleaved samples) as the device has
    /// room for.
    fn write(&self, queue: &mut VecDeque<f32>) -> Result<(), String> {
        unsafe {
            let padding = self.client.GetCurrentPadding().map_err(err("padding"))?;
            let room = (self.buffer_frames - padding) as usize;
            let frames = room.min(queue.len() / CHANNELS);
            if frames == 0 {
                return Ok(());
            }
            let data = self
                .render
                .GetBuffer(frames as u32)
                .map_err(err("render buffer"))?;
            let out = std::slice::from_raw_parts_mut(data as *mut f32, frames * CHANNELS);
            for (slot, sample) in out.iter_mut().zip(queue.drain(..frames * CHANNELS)) {
                *slot = sample;
            }
            self.render
                .ReleaseBuffer(frames as u32, 0)
                .map_err(err("render release"))?;
        }
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Plays `window`'s sound until it ends or `stop` is set. Errors end the
/// sound, never the picture.
pub fn play(
    session: Arc<ClientSession>,
    window: WindowId,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
) {
    std::thread::spawn(move || {
        if let Err(e) = run(&session, window, &stop, &shared) {
            shared.stats.lock().expect("stats").audio_error = Some(e);
        }
    });
}

fn run(
    session: &ClientSession,
    window: WindowId,
    stop: &AtomicBool,
    shared: &Shared,
) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut decoder =
        opus::Decoder::new(RATE, opus::Channels::Stereo).map_err(|e| format!("opus: {e}"))?;
    let mut output: Option<Output> = None;
    let mut queue: VecDeque<f32> = VecDeque::new();
    let mut pcm = vec![0f32; 5760 * CHANNELS];
    let mut playing = false;
    while !stop.load(Ordering::SeqCst) {
        match session.next_audio(window, Duration::from_millis(5)) {
            AudioPoll::Packet(packet) => {
                if output.is_none() {
                    output = Some(Output::open()?);
                    shared.stats.lock().expect("stats").audio = true;
                }
                // A lost packet: let Opus conceal it before the next one.
                if packet.after_loss {
                    if let Ok(frames) = decoder.decode_float(&[], &mut pcm, false) {
                        queue.extend(&pcm[..frames * CHANNELS]);
                    }
                }
                let frames = decoder
                    .decode_float(&packet.data, &mut pcm, false)
                    .map_err(|e| format!("opus decode: {e}"))?;
                queue.extend(&pcm[..frames * CHANNELS]);
                shared.stats.lock().expect("stats").audio_packets += 1;
                let muted = shared.muted.load(Ordering::SeqCst);
                if muted {
                    let len = queue.len();
                    queue
                        .range_mut(len - frames * CHANNELS..)
                        .for_each(|s| *s = 0.0);
                }
            }
            AudioPoll::Timeout => {}
            AudioPoll::Ended => break,
        }
        if queue.len() > MAX_FRAMES * CHANNELS {
            let excess = queue.len() - START_FRAMES * CHANNELS;
            queue.drain(..excess - excess % CHANNELS);
        }
        if !playing && queue.len() >= START_FRAMES * CHANNELS {
            playing = true;
        }
        if queue.is_empty() {
            playing = false;
        }
        if playing {
            if let Some(output) = &output {
                output.write(&mut queue)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The output path against the real default device, writing silence.
    #[test]
    fn the_default_output_takes_samples() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let output = match Output::open() {
            Ok(output) => output,
            Err(e) => {
                println!("skipped: {e}");
                return;
            }
        };
        let mut queue: VecDeque<f32> = std::iter::repeat_n(0.0, START_FRAMES * CHANNELS).collect();
        output.write(&mut queue).unwrap();
        assert!(queue.len() < START_FRAMES * CHANNELS, "nothing was written");
    }
}
