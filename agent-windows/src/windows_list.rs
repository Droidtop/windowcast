//! The windows a user would see in Alt+Tab: visible, unowned, not tool
//! windows, not cloaked (other virtual desktops, suspended UWP frames),
//! with a title. A window's id is its HWND.

use std::path::Path;

use windowcast_protocol::{selection, WindowId, WindowInfo};
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindow, GetWindowLongW, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, GW_OWNER,
    WS_EX_TOOLWINDOW,
};

pub fn hwnd(window: WindowId) -> HWND {
    HWND(window.0 as usize as *mut core::ffi::c_void)
}

pub fn list() -> Vec<WindowInfo> {
    let mut found: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut found as *mut Vec<HWND> as isize));
    }
    let foreground = unsafe { GetForegroundWindow() };
    found
        .into_iter()
        .filter_map(|hwnd| describe(hwnd, hwnd == foreground))
        .collect()
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    if unsafe { is_user_window(hwnd) } {
        found.push(hwnd);
    }
    BOOL(1)
}

unsafe fn is_user_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
            return false;
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return false;
        }
        GetWindowTextLengthW(hwnd) > 0
    }
}

fn describe(hwnd: HWND, focused: bool) -> Option<WindowInfo> {
    let title = title(hwnd)?;
    let app_id = executable(hwnd).unwrap_or_default();
    let (width, height) = size(hwnd).unwrap_or((0, 0));
    Some(WindowInfo {
        id: WindowId(hwnd.0 as usize as u64),
        content: selection::classify(&app_id, &title),
        title,
        app_id,
        width,
        height,
        focused,
    })
}

fn title(hwnd: HWND) -> Option<String> {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    (len > 0).then(|| String::from_utf16_lossy(&buffer[..len as usize]))
}

/// The executable's file name, e.g. `notepad.exe`.
fn executable(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let named = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        named.ok()?;
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }
}

/// The window's visible bounds (without the invisible resize border).
fn size(hwnd: HWND) -> Option<(u32, u32)> {
    let mut rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
    }
    Some((
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    ))
}
