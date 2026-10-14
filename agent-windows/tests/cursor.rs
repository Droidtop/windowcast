//! The host's pointer (docs/BACKENDS.md, "The session owns the window"):
//! a pointer shape reads back as RGBA with its hotspot, and the Windows
//! client makes a cursor of it again.
#![cfg(windows)]

use windowcast_agent_windows::cursor;
use windows::Win32::UI::WindowsAndMessaging::{DestroyCursor, LoadCursorW, IDC_ARROW, IDC_IBEAM};

#[test]
fn pointer_shapes_read_back_and_become_cursors_again() {
    for (name, id) in [("arrow", IDC_ARROW), ("text beam", IDC_IBEAM)] {
        let handle = unsafe { LoadCursorW(None, id) }.expect("a system cursor");
        let image =
            cursor::image(handle.0 as usize as u64).unwrap_or_else(|| panic!("the {name}'s image"));
        println!(
            "{name}: {}x{}, hotspot {:?}",
            image.width, image.height, image.hotspot
        );
        assert!(image.width >= 8 && image.height >= 8, "the {name}'s size");
        assert_eq!(
            image.rgba.len(),
            (image.width * image.height * 4) as usize,
            "the {name}'s pixels"
        );
        let pixels = image.rgba.as_chunks::<4>().0;
        assert!(
            pixels.iter().any(|p| p[3] == 255),
            "nothing of the {name} shows"
        );
        assert!(
            pixels.iter().any(|p| p[3] == 0),
            "nothing around the {name} is clear"
        );
        assert!(
            image.hotspot.0 < image.width && image.hotspot.1 < image.height,
            "the {name}'s hotspot is outside it"
        );
        let made = windowcast_client_windows::cursor::make(&image)
            .unwrap_or_else(|| panic!("a cursor from the {name}"));
        let _ = unsafe { DestroyCursor(made) };
    }
    // The pointer now names a shape that reads back, where there is one.
    if let Some(now) = cursor::now().filter(|now| now.visible) {
        assert!(
            cursor::image(now.shape).is_some(),
            "the pointer's own shape"
        );
    }
}
