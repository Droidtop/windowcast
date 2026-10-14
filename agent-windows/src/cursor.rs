//! The pointer, for the session to draw on the client (docs/BACKENDS.md,
//! "The session owns the window"): where it is, whether it shows, and its
//! shape as RGBA. Captures leave it out (`Capture` turns
//! Windows.Graphics.Capture's cursor off), so every carrier shows the
//! window the same and the client draws the pointer itself.

use windowcast_host::CursorState;
use windowcast_protocol::CursorImage;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, HICON, ICONINFO,
};

/// The pointer now; its shape's id is its cursor handle.
pub fn now() -> Option<CursorState> {
    let mut info = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetCursorInfo(&mut info) }.ok()?;
    let shown = info.flags.0 & CURSOR_SHOWING.0 != 0 && !info.hCursor.is_invalid();
    Some(CursorState {
        position: (info.ptScreenPos.x, info.ptScreenPos.y),
        visible: shown,
        shape: info.hCursor.0 as usize as u64,
    })
}

/// The shape a cursor handle draws, as RGBA with its hotspot.
pub fn image(shape: u64) -> Option<CursorImage> {
    let mut icon = ICONINFO::default();
    unsafe { GetIconInfo(HICON(shape as usize as *mut _), &mut icon) }.ok()?;
    let pixels = unsafe { pixels(&icon) };
    unsafe {
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(icon.hbmColor.into());
        }
        if !icon.hbmMask.is_invalid() {
            let _ = DeleteObject(icon.hbmMask.into());
        }
    }
    let (width, height, rgba) = pixels?;
    Some(CursorImage {
        id: shape,
        width,
        height,
        hotspot: (icon.xHotspot, icon.yHotspot),
        rgba,
    })
}

/// A bitmap's pixels as 32-bit BGRA, rows from the top.
unsafe fn bits(bitmap: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    let mut header = BITMAP::default();
    let got = unsafe {
        GetObjectW(
            bitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut header as *mut BITMAP as *mut _),
        )
    };
    if got == 0 || header.bmWidth <= 0 || header.bmHeight <= 0 {
        return None;
    }
    let (width, height) = (header.bmWidth as u32, header.bmHeight as u32);
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut data = vec![0u8; (width * height * 4) as usize];
    let dc = unsafe { GetDC(None) };
    let lines = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height,
            Some(data.as_mut_ptr() as *mut _),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe { ReleaseDC(None, dc) };
    (lines != 0).then_some((width, height, data))
}

/// A cursor's pixels as RGBA. A colour cursor is its colour bitmap, with
/// its alpha, or its mask's where it has none; a black-and-white one is
/// its mask's two halves (AND over XOR): transparent, black or white, and
/// inverting pixels drawn black.
unsafe fn pixels(icon: &ICONINFO) -> Option<(u32, u32, Vec<u8>)> {
    if !icon.hbmColor.is_invalid() {
        let (width, height, bgra) = unsafe { bits(icon.hbmColor) }?;
        let has_alpha = bgra.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
        let mask = if has_alpha {
            None
        } else {
            unsafe { bits(icon.hbmMask) }
        };
        let mut rgba = Vec::with_capacity(bgra.len());
        for (i, p) in bgra.as_chunks::<4>().0.iter().enumerate() {
            let alpha = match (&mask, has_alpha) {
                (_, true) => p[3],
                (Some((_, _, m)), false) if m.get(i * 4).is_some_and(|v| *v != 0) => 0,
                _ => 255,
            };
            rgba.extend_from_slice(&[p[2], p[1], p[0], alpha]);
        }
        return Some((width, height, rgba));
    }
    let (width, twice, mask) = unsafe { bits(icon.hbmMask) }?;
    let height = twice / 2;
    let at = |x: u32, y: u32| mask[((y * width + x) * 4) as usize] != 0;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let pixel = match (at(x, y), at(x, y + height)) {
                (true, false) => [0, 0, 0, 0],
                (false, false) => [0, 0, 0, 255],
                (false, true) => [255, 255, 255, 255],
                (true, true) => [0, 0, 0, 255],
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    Some((width, height, rgba))
}
