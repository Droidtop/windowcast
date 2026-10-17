//! The three privileged operations. No function here accepts a path or command line.

use std::io;

use crate::operations::{MouseAction, Operation, Response};

pub fn execute(operation: &Operation) -> io::Result<Response> {
    operation.validate()?;
    if !crate::desktop::sync_to_input_desktop() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot open the input desktop",
        ));
    }
    #[cfg(windows)]
    {
        windows_impl::execute(operation)
    }
    #[cfg(not(windows))]
    {
        let _ = operation;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "secure screen is not available on this platform",
        ))
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, DXGI_OUTDUPL_FRAME_INFO,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    pub(super) fn execute(operation: &Operation) -> io::Result<Response> {
        match operation {
            Operation::CaptureFrame => capture(),
            Operation::SendInput { text, mouse } => {
                send_input(text.as_deref(), mouse.as_ref())?;
                Ok(Response::InputAccepted)
            }
            Operation::SecureDesktopState => Ok(Response::SecureDesktopState(
                crate::desktop::input_desktop_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("winlogon")),
            )),
        }
    }

    fn capture() -> io::Result<Response> {
        // Microsoft: CreateDXGIFactory1 begins output enumeration for Desktop Duplication.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(io_error)?;
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(0) }.map_err(io_error)?;
        let output: IDXGIOutput1 = unsafe { adapter.EnumOutputs(0) }
            .and_then(|output| output.cast())
            .map_err(io_error)?;

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // Microsoft: D3D11CreateDevice creates the device used by DuplicateOutput.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(io_error)?;
        let device = device.ok_or_else(|| io::Error::other("D3D11 returned no device"))?;
        let context = context.ok_or_else(|| io::Error::other("D3D11 returned no context"))?;
        // Microsoft: IDXGIOutput1::DuplicateOutput exposes the current desktop image.
        let duplication = unsafe { output.DuplicateOutput(&device) }.map_err(io_error)?;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // Microsoft: AcquireNextFrame waits briefly for a complete desktop frame.
        unsafe { duplication.AcquireNextFrame(1_000, &mut info, &mut resource) }
            .map_err(io_error)?;
        let result = (|| {
            let texture: ID3D11Texture2D = resource
                .ok_or_else(|| io::Error::other("Desktop Duplication returned no texture"))?
                .cast()
                .map_err(io_error)?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            if desc.Width == 0 || desc.Height == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "empty desktop frame",
                ));
            }
            let mut staging_desc = desc;
            staging_desc.Usage = D3D11_USAGE_STAGING;
            staging_desc.BindFlags = 0;
            staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            staging_desc.MiscFlags = 0;
            let mut staging = None;
            // Microsoft: CreateTexture2D creates a CPU-readable staging texture.
            unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
                .map_err(io_error)?;
            let staging =
                staging.ok_or_else(|| io::Error::other("D3D11 returned no staging texture"))?;
            // Microsoft: CopyResource copies the GPU desktop texture to staging memory.
            unsafe { context.CopyResource(&staging, &texture) };
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            // Microsoft: Map(D3D11_MAP_READ) exposes staging rows to the CPU.
            unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
                .map_err(io_error)?;
            let stride = desc
                .Width
                .checked_mul(4)
                .ok_or_else(|| io::Error::other("frame too large"))?;
            let length = (stride as usize)
                .checked_mul(desc.Height as usize)
                .ok_or_else(|| io::Error::other("frame too large"))?;
            if length > crate::operations::MAX_RESPONSE_BYTES - 1024 {
                unsafe { context.Unmap(&staging, 0) };
                return Err(io::Error::other("desktop frame exceeds protocol limit"));
            }
            let mut bgra = vec![0u8; length];
            for row in 0..desc.Height as usize {
                let source = unsafe {
                    std::slice::from_raw_parts(
                        mapped
                            .pData
                            .cast::<u8>()
                            .add(row * mapped.RowPitch as usize),
                        stride as usize,
                    )
                };
                bgra[row * stride as usize..(row + 1) * stride as usize].copy_from_slice(source);
            }
            // Microsoft: Unmap ends CPU access to the staging texture.
            unsafe { context.Unmap(&staging, 0) };
            Ok(Response::Frame {
                width: desc.Width,
                height: desc.Height,
                stride,
                bgra,
            })
        })();
        // Microsoft: every successful AcquireNextFrame is paired with ReleaseFrame.
        unsafe { duplication.ReleaseFrame() }.map_err(io_error)?;
        result
    }

    fn send_input(text: Option<&str>, mouse: Option<&MouseAction>) -> io::Result<()> {
        let mut inputs = Vec::new();
        if let Some(text) = text {
            for unit in text.encode_utf16() {
                inputs.push(keyboard(unit, KEYEVENTF_UNICODE));
                inputs.push(keyboard(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
        }
        if let Some(action) = mouse {
            match action {
                MouseAction::Move { x, y } => inputs.push(absolute_mouse(*x, *y)),
                MouseAction::Button { button, pressed } => {
                    use crate::operations::MouseButton::*;
                    let flags = match (button, pressed) {
                        (Left, true) => MOUSEEVENTF_LEFTDOWN,
                        (Left, false) => MOUSEEVENTF_LEFTUP,
                        (Right, true) => MOUSEEVENTF_RIGHTDOWN,
                        (Right, false) => MOUSEEVENTF_RIGHTUP,
                        (Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                        (Middle, false) => MOUSEEVENTF_MIDDLEUP,
                    };
                    inputs.push(mouse_input(0, 0, flags));
                }
            }
        }
        // Microsoft: SendInput serially inserts only the bounded keyboard/mouse INPUT records above.
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn keyboard(
        scan: u16,
        flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS,
    ) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: Default::default(),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn absolute_mouse(x: i32, y: i32) -> INPUT {
        let (left, top, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        let dx =
            ((x.clamp(left, left + width - 1) - left) as i64 * 65_535 / (width - 1) as i64) as i32;
        let dy =
            ((y.clamp(top, top + height - 1) - top) as i64 * 65_535 / (height - 1) as i64) as i32;
        mouse_input(
            dx,
            dy,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )
    }

    fn mouse_input(
        dx: i32,
        dy: i32,
        flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    ) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn io_error(error: windows::core::Error) -> io::Error {
        io::Error::other(error.to_string())
    }
}
