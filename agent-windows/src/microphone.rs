//! The client's microphone on Windows. Windows has no API for making a
//! microphone, so the client's sound plays into the output side of a
//! virtual audio cable whose input side applications record from: the
//! device named by `--microphone-device` (part of its name), or the first
//! known cable on this PC (VB-Audio's "CABLE Input", VoiceMeeter's input,
//! Virtual Audio Cable, or the "Steam Streaming Microphone" Steam
//! installs). Without one, the host says so and the client's microphone is
//! not used.

use std::collections::VecDeque;

use windowcast_host::audio::MicrophoneSink;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

/// Output devices that are the playback side of a virtual microphone.
const KNOWN_CABLES: [&str; 4] = [
    "CABLE Input",
    "VoiceMeeter Input",
    "Virtual Audio Cable",
    "Steam Streaming Microphone",
];

fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

/// Active audio devices of one direction, with their names.
pub fn endpoints(flow: EDataFlow) -> Vec<(IMMDevice, String)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(devices) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return Vec::new();
        };
        let Ok(collection) = devices.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        let count = collection.GetCount().unwrap_or(0);
        (0..count)
            .filter_map(|i| {
                let device = collection.Item(i).ok()?;
                let store = device.OpenPropertyStore(STGM_READ).ok()?;
                let name = store.GetValue(&PKEY_Device_FriendlyName).ok()?.to_string();
                Some((device, name))
            })
            .collect()
    }
}

/// The output device a client's microphone plays into.
fn cable(named: Option<&str>) -> Result<(IMMDevice, String), String> {
    let outputs = endpoints(eRender);
    let wanted: Vec<&str> = match named {
        Some(name) => vec![name],
        None => KNOWN_CABLES.to_vec(),
    };
    for part in wanted {
        if let Some(found) = outputs
            .iter()
            .find(|(_, name)| name.to_lowercase().contains(&part.to_lowercase()))
        {
            return Ok(found.clone());
        }
    }
    Err(match named {
        Some(name) => format!("no output device called {name}"),
        None => "this PC has no virtual audio cable (install one, e.g. VB-Audio's VB-CABLE, \
                 and record from its output in the application)"
            .into(),
    })
}

pub struct WindowsMicrophone {
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_frames: u32,
    queue: VecDeque<i16>,
    /// Waiting for enough sound to start (again) without gaps.
    filling: bool,
    pub device: String,
}

// Created and used in the multithreaded apartment, where the audio client
// is free-threaded.
unsafe impl Send for WindowsMicrophone {}

/// Sound kept waiting for the device at most: 200 ms of stereo samples.
const MAX_QUEUED: usize = 48_000 / 5 * 2;
/// Sound gathered before playing starts, so network jitter leaves no
/// gaps: 60 ms of stereo samples.
const START_QUEUED: usize = 48_000 * 60 / 1000 * 2;

impl WindowsMicrophone {
    pub fn open(named: Option<&str>) -> Result<Self, String> {
        let (device, name) = cable(named)?;
        unsafe {
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(err("microphone device"))?;
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
                    1_000_000, // 100 ms
                    0,
                    &format,
                    None,
                )
                .map_err(err("microphone device initialize"))?;
            let buffer_frames = client.GetBufferSize().map_err(err("buffer size"))?;
            let render: IAudioRenderClient = client.GetService().map_err(err("render"))?;
            client.Start().map_err(err("microphone device start"))?;
            println!("the client's microphone plays into {name}");
            Ok(WindowsMicrophone {
                client,
                render,
                buffer_frames,
                queue: VecDeque::new(),
                filling: true,
                device: name,
            })
        }
    }
}

impl MicrophoneSink for WindowsMicrophone {
    fn play(&mut self, samples: &[i16]) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        self.queue.extend(samples);
        if self.queue.len() > MAX_QUEUED {
            let excess = self.queue.len() - MAX_QUEUED / 2;
            self.queue.drain(..excess - excess % 2);
        }
        unsafe {
            let Ok(padding) = self.client.GetCurrentPadding() else {
                return;
            };
            // The device ran dry: gather again before playing on.
            if padding == 0 && !self.filling {
                self.filling = true;
            }
            if self.filling {
                if self.queue.len() < START_QUEUED {
                    return;
                }
                self.filling = false;
            }
            let frames = ((self.buffer_frames - padding) as usize).min(self.queue.len() / 2);
            if frames == 0 {
                return;
            }
            let Ok(data) = self.render.GetBuffer(frames as u32) else {
                return;
            };
            let out = std::slice::from_raw_parts_mut(data as *mut i16, frames * 2);
            for (slot, sample) in out.iter_mut().zip(self.queue.drain(..frames * 2)) {
                *slot = sample;
            }
            let _ = self.render.ReleaseBuffer(frames as u32, 0);
        }
    }
}

impl Drop for WindowsMicrophone {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}
