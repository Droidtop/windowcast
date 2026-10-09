//! A window's sound: WASAPI process loopback (Windows 10 2004 and later)
//! captures what the window's process, and the processes it started,
//! play, and nothing else (a browser's sound comes from a child process).
//! Windows converts it to 48 kHz stereo 16-bit for us.

use std::time::Duration;

use windowcast_host::audio::AudioSource;
use windowcast_protocol::WindowId;
use windows::core::{implement, Interface, HRESULT};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

use crate::windows_list;

fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

/// Signals an event when the asynchronous activation completes.
#[implement(
    IActivateAudioInterfaceCompletionHandler,
    windows::Win32::System::Com::IAgileObject
)]
struct Activated(isize);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe { SetEvent(HANDLE(self.0 as *mut _)) }
    }
}

impl windows::Win32::System::Com::IAgileObject_Impl for Activated_Impl {}

/// A PROPVARIANT holding a VT_BLOB, laid out as the real one is (x64 and
/// x86 alike: the type, three reserved words, then the BLOB).
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const u8,
}

const VT_BLOB: u16 = 65;

pub struct ProcessAudio {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
}

// Created in, and used from, the multithreaded apartment; the audio
// client and its capture service are free-threaded there.
unsafe impl Send for ProcessAudio {}

impl ProcessAudio {
    /// Starts capturing what `window`'s process tree plays.
    pub fn open(window: WindowId) -> Result<Self, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let mut pid = 0u32;
            GetWindowThreadProcessId(windows_list::hwnd(window), Some(&mut pid));
            if pid == 0 {
                return Err("no such window".into());
            }
            let params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: pid,
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            let variant = BlobVariant {
                vt: VT_BLOB,
                reserved: [0; 3],
                size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                data: &params as *const AUDIOCLIENT_ACTIVATION_PARAMS as *const u8,
            };
            let done = CreateEventW(None, false, false, None).map_err(err("event"))?;
            let handler: IActivateAudioInterfaceCompletionHandler =
                Activated(done.0 as isize).into();
            let operation = ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&variant as *const BlobVariant as *const _),
                &handler,
            )
            .map_err(err("process loopback (needs Windows 10 2004 or later)"))?;
            let waited = WaitForSingleObject(done, 5000);
            let _ = CloseHandle(done);
            if waited != WAIT_OBJECT_0 {
                return Err("the audio activation did not finish".into());
            }
            let mut result = HRESULT(0);
            let mut unknown: Option<windows::core::IUnknown> = None;
            operation
                .GetActivateResult(&mut result, &mut unknown)
                .map_err(err("activation result"))?;
            result.ok().map_err(err("activation"))?;
            let client: IAudioClient = unknown
                .ok_or("no audio client")?
                .cast()
                .map_err(err("audio client"))?;

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
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    2_000_000, // 200 ms
                    0,
                    &format,
                    None,
                )
                .map_err(err("audio client initialize"))?;
            let event = CreateEventW(None, false, false, None).map_err(err("event"))?;
            client.SetEventHandle(event).map_err(err("event handle"))?;
            let capture: IAudioCaptureClient =
                client.GetService().map_err(err("capture service"))?;
            client.Start().map_err(err("start"))?;
            Ok(ProcessAudio {
                client,
                capture,
                event,
            })
        }
    }
}

impl AudioSource for ProcessAudio {
    fn next_samples(&mut self) -> Option<Vec<i16>> {
        let mut samples = Vec::new();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = WaitForSingleObject(self.event, Duration::from_millis(100).as_millis() as u32);
            loop {
                let pending = match self.capture.GetNextPacketSize() {
                    Ok(pending) => pending,
                    Err(_) => return None,
                };
                if pending == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if self
                    .capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    return None;
                }
                let count = frames as usize * 2;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    samples.resize(samples.len() + count, 0);
                } else {
                    samples
                        .extend_from_slice(std::slice::from_raw_parts(data as *const i16, count));
                }
                let _ = self.capture.ReleaseBuffer(frames);
            }
        }
        Some(samples)
    }
}

impl Drop for ProcessAudio {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}
