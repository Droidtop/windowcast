//! The host's pointer over a stream window (docs/BACKENDS.md, "The session
//! owns the window"). The host leaves its pointer out of its pictures and
//! sends where it is and its shape; while this window sends input, it
//! shows that shape as its own pointer, so the pointer looks as it does on
//! the host, the same on every carrier, and moves with the mouse at no
//! latency. A pointer the host hides is hidden here too.

use std::collections::HashMap;

use windowcast_client::ClientSession;
use windowcast_protocol::CursorImage;
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyCursor, HCURSOR, ICONINFO,
};

/// The host's pointer shapes made into cursors, by the host's shape id.
#[derive(Default)]
pub struct Pointers {
    made: HashMap<u64, Option<HCURSOR>>,
}

impl Pointers {
    /// The cursor for `shape`, made the first time it is asked for; `None`
    /// when the host has not sent that shape or it cannot be made.
    pub fn get(&mut self, session: &ClientSession, shape: u64) -> Option<HCURSOR> {
        if let Some(made) = self.made.get(&shape) {
            return *made;
        }
        let image = session.cursor_image(shape)?;
        let made = make(&image);
        self.made.insert(shape, made);
        made
    }
}

impl Drop for Pointers {
    fn drop(&mut self) {
        for cursor in self.made.values().flatten() {
            let _ = unsafe { DestroyCursor(*cursor) };
        }
    }
}

/// A cursor from an RGBA image and its hotspot.
pub fn make(image: &CursorImage) -> Option<HCURSOR> {
    let (width, height) = (image.width as i32, image.height as i32);
    if width <= 0 || height <= 0 || image.rgba.len() < (width * height * 4) as usize {
        return None;
    }
    let bgra: Vec<u8> = image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();
    // All of the colour shows where its alpha says: the mask is empty.
    let mask = vec![0u8; (width as usize).div_ceil(16) * 2 * height as usize];
    unsafe {
        let color = CreateBitmap(width, height, 1, 32, Some(bgra.as_ptr() as *const _));
        let mask = CreateBitmap(width, height, 1, 1, Some(mask.as_ptr() as *const _));
        let info = ICONINFO {
            fIcon: false.into(),
            xHotspot: image.hotspot.0,
            yHotspot: image.hotspot.1,
            hbmMask: mask,
            hbmColor: color,
        };
        let cursor = CreateIconIndirect(&info);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        cursor.ok().map(|icon| HCURSOR(icon.0))
    }
}
