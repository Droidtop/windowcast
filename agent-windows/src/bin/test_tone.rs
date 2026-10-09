//! A process that plays a 440 Hz tone, for the agent's audio tests: it
//! owns a hidden window (what a host names a process by) and renders the
//! tone to an output device. Process loopback hears what the process plays
//! after its session volume (a muted session is captured as silence), so a
//! test that must stay quiet in the room names a virtual output device in
//! `WINDOWCAST_TONE_DEVICE` (an endpoint id); otherwise the default one
//! plays it, quietly.
//!
//! Usage: `windowcast-test-tone`. Prints `window HWND` once playing, and
//! runs until killed.

#[cfg(windows)]
unsafe extern "system" fn window_proc(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

#[cfg(windows)]
fn main() {
    use std::time::Duration;

    use windows::core::w;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("windowcast-test-tone"),
            ..Default::default()
        };
        RegisterClassW(&class);
        // Never shown.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("windowcast-test-tone"),
            w!("windowcast test tone"),
            WS_POPUP,
            0,
            0,
            16,
            16,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap();

        let devices: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let device = match std::env::var("WINDOWCAST_TONE_DEVICE") {
            Ok(id) => {
                let id: Vec<u16> = id.encode_utf16().chain([0]).collect();
                devices
                    .GetDevice(windows::core::PCWSTR(id.as_ptr()))
                    .unwrap()
            }
            Err(_) => devices.GetDefaultAudioEndpoint(eRender, eConsole).unwrap(),
        };
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
        let format = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000,
                0,
                &format,
                None,
            )
            .unwrap();
        let render: IAudioRenderClient = client.GetService().unwrap();
        let size = client.GetBufferSize().unwrap();
        client.Start().unwrap();
        println!("window {}", hwnd.0 as usize);
        let mut phase = 0f32;
        loop {
            let padding = client.GetCurrentPadding().unwrap();
            let frames = size - padding;
            if frames > 0 {
                let data = render.GetBuffer(frames).unwrap() as *mut f32;
                let out = std::slice::from_raw_parts_mut(data, frames as usize * 2);
                for pair in out.chunks_mut(2) {
                    let v = phase.sin() * 0.25;
                    pair[0] = v;
                    pair[1] = v;
                    phase += 440.0 * std::f32::consts::TAU / 48_000.0;
                    if phase > std::f32::consts::TAU {
                        phase -= std::f32::consts::TAU;
                    }
                }
                render.ReleaseBuffer(frames, 0).unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(not(windows))]
fn main() {}
