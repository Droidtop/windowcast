//! The windows a client may stream, each its own entry (docs/BACKENDS.md,
//! "Window by window, always"): the windows a user would see in Alt+Tab
//! (visible, unowned, not tool windows, with a title), and the dialogs,
//! popups and menus they own, each naming its owner. Cloaked windows
//! (other virtual desktops, suspended UWP frames), minimized ones and ones
//! without a size are left out, and so is a tool window or menu that no
//! listed window owns (the shell's own). A window's id is its HWND.

use std::collections::HashMap;
use std::path::Path;

use windowcast_protocol::{selection, ContentHint, WindowId, WindowInfo, WindowKind};
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindow, GetWindowLongW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, GUITHREADINFO,
    GWL_EXSTYLE, GWL_STYLE, GW_OWNER, WS_CAPTION, WS_EX_TOOLWINDOW,
};

pub fn hwnd(window: WindowId) -> HWND {
    HWND(window.0 as usize as *mut core::ffi::c_void)
}

/// A visible top-level window, before it is placed in the list.
struct Candidate {
    hwnd: HWND,
    title: Option<String>,
    class: String,
    owner: Option<HWND>,
    tool: bool,
    caption: bool,
    thread: u32,
}

impl Candidate {
    fn menu(&self) -> bool {
        // The system's menu window class.
        self.class == "#32768"
    }

    /// A window of a program's own: unowned, not a tool window or menu,
    /// with a title.
    fn normal(&self) -> bool {
        self.owner.is_none() && !self.tool && !self.menu() && self.title.is_some()
    }
}

pub fn list() -> Vec<WindowInfo> {
    let mut found: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut found as *mut Vec<HWND> as isize));
    }
    let foreground = unsafe { GetForegroundWindow() };
    let candidates: Vec<Candidate> = found.into_iter().map(candidate).collect();

    let mut listed: Vec<WindowInfo> = Vec::new();
    let mut index: HashMap<usize, usize> = HashMap::new();
    for c in candidates.iter().filter(|c| c.normal()) {
        let info = describe(c, foreground, None, WindowKind::Normal, None);
        index.insert(c.hwnd.0 as usize, listed.len());
        listed.push(info);
    }
    // Owned windows next, until none is added: a dialog may own a popup.
    let mut placed = true;
    while placed {
        placed = false;
        for c in candidates.iter().filter(|c| !c.normal()) {
            if index.contains_key(&(c.hwnd.0 as usize)) {
                continue;
            }
            let Some(owner) = owner_of(c).filter(|o| index.contains_key(&(o.0 as usize))) else {
                continue;
            };
            let owner_info = listed[index[&(owner.0 as usize)]].clone();
            let kind = if c.menu() {
                WindowKind::Menu
            } else if c.owner.is_some() && c.caption {
                WindowKind::Dialog
            } else {
                WindowKind::Popup
            };
            let info = describe(c, foreground, Some(&owner_info), kind, Some(owner));
            index.insert(c.hwnd.0 as usize, listed.len());
            listed.push(info);
            placed = true;
        }
    }
    listed
}

/// Who owns a window: its owner window, or for a menu or an unowned
/// popup (tooltips, drop-downs) the window its thread has active or
/// shows the menu for.
fn owner_of(c: &Candidate) -> Option<HWND> {
    if let Some(owner) = c.owner {
        return Some(owner);
    }
    if !(c.tool || c.menu()) {
        return None;
    }
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(c.thread, &mut info) }.ok()?;
    [info.hwndMenuOwner, info.hwndActive]
        .into_iter()
        .find(|h| !h.is_invalid() && *h != c.hwnd)
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    if unsafe { shown(hwnd) } {
        found.push(hwnd);
    }
    BOOL(1)
}

/// Visible, not minimized, not cloaked, with a size.
unsafe fn shown(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
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
    }
    size(hwnd).is_some_and(|(w, h)| w > 0 && h > 0)
}

fn candidate(hwnd: HWND) -> Candidate {
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) }
        .ok()
        .filter(|owner| !owner.is_invalid());
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut class) };
    Candidate {
        hwnd,
        title: title(hwnd),
        class: String::from_utf16_lossy(&class[..len.max(0) as usize]),
        owner,
        tool: ex_style & WS_EX_TOOLWINDOW.0 != 0,
        caption: style & WS_CAPTION.0 == WS_CAPTION.0,
        thread: unsafe { GetWindowThreadProcessId(hwnd, None) },
    }
}

fn describe(
    c: &Candidate,
    foreground: HWND,
    owner_info: Option<&WindowInfo>,
    kind: WindowKind,
    owner: Option<HWND>,
) -> WindowInfo {
    let app_id = executable(c.hwnd).unwrap_or_default();
    let (width, height) = size(c.hwnd).unwrap_or((0, 0));
    // An untitled popup or menu is named after its owner.
    let title = c.title.clone().unwrap_or_else(|| {
        let what = match kind {
            WindowKind::Menu => "menu",
            WindowKind::Dialog => "dialog",
            _ => "popup",
        };
        match owner_info {
            Some(o) => format!("{} {what}", o.title),
            None => what.to_owned(),
        }
    });
    // Owned windows take their owner's content, so the rules give them the
    // same backend.
    let content: ContentHint =
        owner_info.map_or_else(|| selection::classify(&app_id, &title), |o| o.content);
    WindowInfo {
        id: WindowId(c.hwnd.0 as usize as u64),
        title,
        app_id,
        width,
        height,
        focused: c.hwnd == foreground,
        content,
        owner: owner.map(|o| WindowId(o.0 as usize as u64)),
        kind,
    }
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
