//! The clipboard's text, shared both ways. Windows counts every clipboard
//! change (GetClipboardSequenceNumber), which is how a change is noticed
//! without reading the clipboard each time.

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// The change counter.
pub fn sequence() -> u64 {
    u64::from(unsafe { GetClipboardSequenceNumber() })
}

/// The clipboard's text, if it holds text.
pub fn text() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let text = (|| {
            let handle = GetClipboardData(u32::from(CF_UNICODETEXT.0)).ok()?;
            let global = HGLOBAL(handle.0);
            let data = GlobalLock(global) as *const u16;
            if data.is_null() {
                return None;
            }
            let mut len = 0;
            while *data.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(data, len));
            let _ = GlobalUnlock(global);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

pub fn set_text(text: &str) {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2) else {
            return;
        };
        let data = GlobalLock(global) as *mut u16;
        if data.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), data, units.len());
        let _ = GlobalUnlock(global);
        if OpenClipboard(None).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        // On success the clipboard owns the memory.
        let _ = SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(global.0)));
        let _ = CloseClipboard();
    }
}
