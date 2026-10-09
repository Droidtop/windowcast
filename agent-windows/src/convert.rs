//! Captured pictures to the 4:2:0 layouts encoders take. Capture
//! normally hands over NV12 already converted on the GPU (gpu_convert.rs),
//! or a texture an encoder on the same GPU takes as it is; the BGRA
//! conversions (host-core's `video`) are the fallback for devices without
//! a video processor.

pub use windowcast_host::video::{even, nv12_to_i420, to_i420, to_nv12, Bgra, Nv12};

/// A captured picture, in whichever layout capture produced.
pub enum Picture<'a> {
    Bgra(Bgra<'a>),
    Nv12(Nv12<'a>),
    /// An NV12 texture on the capture device, for an encoder on the same
    /// GPU to take without a copy (see `Encoder::takes_textures`).
    #[cfg(windows)]
    Texture {
        texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        width: usize,
        height: usize,
    },
}

impl Picture<'_> {
    pub fn width(&self) -> usize {
        match self {
            Picture::Bgra(p) => p.width,
            Picture::Nv12(p) => p.width,
            #[cfg(windows)]
            Picture::Texture { width, .. } => *width,
        }
    }

    pub fn height(&self) -> usize {
        match self {
            Picture::Bgra(p) => p.height,
            Picture::Nv12(p) => p.height,
            #[cfg(windows)]
            Picture::Texture { height, .. } => *height,
        }
    }

    /// NV12 at even dimensions, converting BGRA on the way.
    pub fn to_nv12(&self, out: &mut Vec<u8>) -> Result<(), String> {
        match self {
            Picture::Bgra(p) => to_nv12(p, out),
            Picture::Nv12(p) => {
                out.clear();
                out.extend_from_slice(p.data);
            }
            #[cfg(windows)]
            Picture::Texture { .. } => return Err("a texture is not in memory".into()),
        }
        Ok(())
    }

    /// I420 at even dimensions.
    pub fn to_i420(&self, out: &mut Vec<u8>) -> Result<(), String> {
        match self {
            Picture::Bgra(p) => to_i420(p, out),
            Picture::Nv12(p) => nv12_to_i420(p, out),
            #[cfg(windows)]
            Picture::Texture { .. } => return Err("a texture is not in memory".into()),
        }
        Ok(())
    }
}
