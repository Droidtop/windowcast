//! Shared pieces of the reference tools: a test-pattern window source
//! (software H.264 through OpenH264) that lets a host stream real encoded
//! video without any capture, and an H.264 frame checker for clients.

pub mod testpattern;

use std::path::PathBuf;

/// `$XDG_DATA_HOME/windowcast` (or `~/.local/share/windowcast`), or
/// `%APPDATA%\windowcast` on Windows.
pub fn data_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("windowcast");
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").expect("HOME must be set")).join(".local/share")
        });
    base.join("windowcast")
}

/// Decodes received H.264 frames in software, to check what arrived is
/// real, decodable video.
pub struct H264Check {
    decoder: openh264::decoder::Decoder,
    pub pictures: usize,
    pub dimensions: Option<(usize, usize)>,
}

impl H264Check {
    pub fn new() -> Result<Self, openh264::Error> {
        Ok(H264Check {
            decoder: openh264::decoder::Decoder::new()?,
            pictures: 0,
            dimensions: None,
        })
    }

    /// Decodes one access unit; counts the picture it produced.
    pub fn decode(&mut self, frame: &[u8]) -> Result<(), openh264::Error> {
        use openh264::formats::YUVSource;
        if let Some(picture) = self.decoder.decode(frame)? {
            self.pictures += 1;
            self.dimensions = Some(picture.dimensions());
        }
        Ok(())
    }
}
