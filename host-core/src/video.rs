//! Pictures for software paths: captured BGRA to the 4:2:0 layouts
//! encoders take, NV12 (Media Foundation, most hardware) and I420
//! (OpenH264), and OpenH264 itself, the software H.264 encoder every host
//! can fall back to. BT.601 limited range, the default a decoder assumes
//! when the stream does not say. Odd edges are dropped: encoders need even
//! dimensions. Pictures can be scaled down on the way (adaptive quality).
//! Shared by the agents (agent-windows, agent-linux).

/// A captured picture: 8-bit BGRA rows, `stride` bytes apart.
pub struct Bgra<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

/// A picture already in NV12 at even dimensions: the Y rows, then the
/// interleaved UV rows, tightly packed.
pub struct Nv12<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub height: usize,
}

/// Splits NV12's interleaved chroma into I420's two planes.
pub fn nv12_to_i420(src: &Nv12<'_>, out: &mut Vec<u8>) {
    let luma = src.width * src.height;
    let quarter = luma / 4;
    out.resize(luma + 2 * quarter, 0);
    out[..luma].copy_from_slice(&src.data[..luma]);
    let (u_plane, v_plane) = out[luma..].split_at_mut(quarter);
    for (i, uv) in src.data[luma..luma + 2 * quarter]
        .as_chunks::<2>()
        .0
        .iter()
        .enumerate()
    {
        u_plane[i] = uv[0];
        v_plane[i] = uv[1];
    }
}

/// The even dimensions a picture is encoded at.
pub fn even(width: usize, height: usize) -> (usize, usize) {
    (width & !1, height & !1)
}

/// The even size `(width, height)` comes to at `scale` (at least 2x2).
pub fn scaled(width: usize, height: usize, scale: f32) -> (usize, usize) {
    if scale >= 1.0 {
        return even(width, height);
    }
    let at = |n: usize| (((n as f32 * scale).round() as usize) & !1).max(2);
    (at(width), at(height))
}

/// Scales `src` down to `size` into `out` by averaging the source pixels
/// each output pixel covers (a box filter: no aliasing on the way down),
/// and returns the scaled picture. Rows of `out` are tightly packed.
pub fn scale_bgra<'a>(src: &Bgra<'_>, size: (usize, usize), out: &'a mut Vec<u8>) -> Bgra<'a> {
    let (w, h) = size;
    out.resize(w * h * 4, 0);
    // The source span each output column and row covers.
    let span = |i: usize, to: usize, from: usize| {
        let start = i * from / to;
        let end = ((i + 1) * from / to).max(start + 1).min(from);
        start..end
    };
    let columns: Vec<_> = (0..w).map(|x| span(x, w, src.width)).collect();
    for y in 0..h {
        let rows = span(y, h, src.height);
        for (x, cols) in columns.iter().enumerate() {
            let mut sum = [0u32; 4];
            for row in rows.clone() {
                let line = &src.data[row * src.stride..];
                for col in cols.clone() {
                    for (c, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(line[col * 4 + c]);
                    }
                }
            }
            let count = (rows.len() * cols.len()) as u32;
            let o = (y * w + x) * 4;
            for c in 0..4 {
                out[o + c] = (sum[c] / count) as u8;
            }
        }
    }
    Bgra {
        data: out,
        width: w,
        height: h,
        stride: w * 4,
    }
}

#[inline]
fn luma(b: u8, g: u8, r: u8) -> u8 {
    (((66 * r as i32 + 129 * g as i32 + 25 * b as i32 + 128) >> 8) + 16) as u8
}

/// Chroma of the average of a 2x2 block.
#[inline]
fn chroma(b: i32, g: i32, r: i32) -> (u8, u8) {
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

/// Writes the Y plane and calls `put_uv(index, u, v)` for each chroma sample.
fn convert(src: &Bgra<'_>, y_plane: &mut [u8], mut put_uv: impl FnMut(usize, u8, u8)) {
    let (w, h) = even(src.width, src.height);
    for row in 0..h {
        let line = &src.data[row * src.stride..row * src.stride + w * 4];
        let out = &mut y_plane[row * w..row * w + w];
        for (x, px) in line.as_chunks::<4>().0.iter().enumerate() {
            out[x] = luma(px[0], px[1], px[2]);
        }
    }
    for cy in 0..h / 2 {
        let top = &src.data[cy * 2 * src.stride..];
        let bottom = &src.data[(cy * 2 + 1) * src.stride..];
        for cx in 0..w / 2 {
            let o = cx * 8;
            let sum = |c: usize| {
                top[o + c] as i32
                    + top[o + 4 + c] as i32
                    + bottom[o + c] as i32
                    + bottom[o + 4 + c] as i32
            };
            let (u, v) = chroma(sum(0) / 4, sum(1) / 4, sum(2) / 4);
            put_uv(cy * (w / 2) + cx, u, v);
        }
    }
}

/// NV12: the Y plane, then interleaved U/V at half resolution.
pub fn to_nv12(src: &Bgra<'_>, out: &mut Vec<u8>) {
    let (w, h) = even(src.width, src.height);
    out.resize(w * h * 3 / 2, 0);
    let (y_plane, uv) = out.split_at_mut(w * h);
    convert(src, y_plane, |i, u, v| {
        uv[i * 2] = u;
        uv[i * 2 + 1] = v;
    });
}

/// I420: the Y plane, then the U plane, then the V plane.
pub fn to_i420(src: &Bgra<'_>, out: &mut Vec<u8>) {
    let (w, h) = even(src.width, src.height);
    out.resize(w * h * 3 / 2, 0);
    let (y_plane, chroma) = out.split_at_mut(w * h);
    let (u_plane, v_plane) = chroma.split_at_mut(w * h / 4);
    convert(src, y_plane, |i, u, v| {
        u_plane[i] = u;
        v_plane[i] = v;
    });
}

/// OpenH264, the software H.264 encoder.
pub struct OpenH264 {
    encoder: openh264::encoder::Encoder,
    i420: Vec<u8>,
}

// The encoder is only used from the stream's one thread.
unsafe impl Send for OpenH264 {}

impl OpenH264 {
    /// An encoder at `bitrate` bits a second and `fps` frames a second,
    /// with a keyframe every two seconds.
    pub fn new(bitrate: u32, fps: u32) -> Result<Self, String> {
        use openh264::encoder::{BitRate, EncoderConfig, FrameRate, IntraFramePeriod};
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 2));
        let encoder = openh264::encoder::Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        )
        .map_err(|e| e.to_string())?;
        Ok(OpenH264 {
            encoder,
            i420: Vec::new(),
        })
    }

    /// Changes the bitrate and frame rate of the running encoder, from the
    /// next picture on, without a keyframe.
    pub fn set_rate(&mut self, bitrate: u32, fps: u32) -> Result<(), String> {
        use openh264_sys2::{
            SBitrateInfo, ENCODER_OPTION_BITRATE, ENCODER_OPTION_FRAME_RATE, SPATIAL_LAYER_ALL,
        };
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: bitrate.min(i32::MAX as u32) as i32,
        };
        let mut rate = fps.max(1) as f32;
        // SAFETY: both options take a pointer to the type given here, and
        // the encoder is initialised (made in `new`).
        let (bitrate_set, rate_set) = unsafe {
            let api = self.encoder.raw_api();
            (
                api.set_option(ENCODER_OPTION_BITRATE, (&raw mut info).cast()),
                api.set_option(ENCODER_OPTION_FRAME_RATE, (&raw mut rate).cast()),
            )
        };
        if bitrate_set != 0 || rate_set != 0 {
            return Err(format!(
                "OpenH264 refused the new rate ({bitrate_set}, {rate_set})"
            ));
        }
        Ok(())
    }

    /// Encodes one picture, given as I420 by `fill` (which gets the
    /// buffer to fill) at `width` by `height` (even). Returns the access
    /// unit, or nothing when the encoder skipped the picture.
    pub fn encode(
        &mut self,
        width: usize,
        height: usize,
        keyframe: bool,
        fill: impl FnOnce(&mut Vec<u8>) -> Result<(), String>,
    ) -> Result<Option<Vec<u8>>, String> {
        fill(&mut self.i420)?;
        let yuv =
            openh264::formats::YUVBuffer::from_vec(std::mem::take(&mut self.i420), width, height);
        if keyframe {
            self.encoder.force_intra_frame();
        }
        let out = self
            .encoder
            .encode(&yuv)
            .map_err(|e| e.to_string())?
            .to_vec();
        Ok((!out.is_empty()).then_some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(b: u8, g: u8, r: u8, width: usize, height: usize, stride: usize) -> Vec<u8> {
        let mut data = vec![0u8; stride * height];
        for row in 0..height {
            for x in 0..width {
                data[row * stride + x * 4..row * stride + x * 4 + 4]
                    .copy_from_slice(&[b, g, r, 255]);
            }
        }
        data
    }

    #[test]
    fn white_and_black_hit_the_limited_range_ends() {
        let white = solid(255, 255, 255, 4, 2, 16);
        let mut nv12 = Vec::new();
        to_nv12(
            &Bgra {
                data: &white,
                width: 4,
                height: 2,
                stride: 16,
            },
            &mut nv12,
        );
        assert_eq!(&nv12[..8], &[235; 8]);
        assert_eq!(&nv12[8..], &[128; 4]);

        let black = solid(0, 0, 0, 4, 2, 16);
        to_nv12(
            &Bgra {
                data: &black,
                width: 4,
                height: 2,
                stride: 16,
            },
            &mut nv12,
        );
        assert_eq!(&nv12[..8], &[16; 8]);
    }

    #[test]
    fn scaling_down_averages_what_each_pixel_covers() {
        // 4x2: left half black, right half white; to 2x1.
        let mut data = solid(0, 0, 0, 4, 2, 16);
        for row in 0..2 {
            data[row * 16 + 8..row * 16 + 16].copy_from_slice(&[255; 8]);
        }
        let src = Bgra {
            data: &data,
            width: 4,
            height: 2,
            stride: 16,
        };
        let mut out = Vec::new();
        let small = scale_bgra(&src, (2, 1), &mut out);
        assert_eq!(small.data, [0, 0, 0, 255, 255, 255, 255, 255]);
        // Three quarters of 1920x1080, even.
        assert_eq!(scaled(1920, 1080, 0.75), (1440, 810));
        assert_eq!(scaled(1921, 1081, 1.0), (1920, 1080));
    }

    #[test]
    fn a_lower_rate_makes_smaller_frames_without_a_new_encoder() {
        let (w, h) = (320, 240);
        let mut encoder = OpenH264::new(1_500_000, 30).unwrap();
        // A moving gradient with a little grain: busy enough that the rate
        // binds, regular enough that the low rate still codes frames.
        let mut seed = 1u32;
        let mut t = 0usize;
        let mut frame = |encoder: &mut OpenH264| {
            t += 1;
            let len = encoder
                .encode(w, h, false, |i420| {
                    i420.resize(w * h * 3 / 2, 128);
                    for (i, byte) in i420[..w * h].iter_mut().enumerate() {
                        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                        let (x, y) = (i % w, i / w);
                        *byte = ((x + y + t * 5) as u8) ^ ((seed >> 28) as u8);
                    }
                    Ok(())
                })
                .unwrap()
                .map_or(0, |data| data.len());
            len
        };
        let high: usize = (0..30).map(|_| frame(&mut encoder)).sum();
        encoder.set_rate(150_000, 30).unwrap();
        for _ in 0..10 {
            frame(&mut encoder);
        }
        let low: usize = (0..30).map(|_| frame(&mut encoder)).sum();
        println!("30 frames: {high} bytes at 1.5 Mbit/s, {low} at 150 kbit/s");
        assert!(low > 0, "the low rate coded nothing");
        assert!(
            low * 3 < high,
            "{low} bytes at the low rate, {high} at the high"
        );
    }

    #[test]
    fn nv12_splits_into_i420_planes() {
        // 4x2: eight luma samples, then two UV pairs.
        let nv12 = [1, 2, 3, 4, 5, 6, 7, 8, 10, 20, 11, 21];
        let mut i420 = Vec::new();
        nv12_to_i420(
            &Nv12 {
                data: &nv12,
                width: 4,
                height: 2,
            },
            &mut i420,
        );
        assert_eq!(i420, [1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 20, 21]);
    }

    #[test]
    fn red_has_high_v_and_odd_edges_are_dropped() {
        // 5x3 with padding in each row: encoded as 4x2.
        let red = solid(0, 0, 255, 5, 3, 24);
        let mut i420 = Vec::new();
        to_i420(
            &Bgra {
                data: &red,
                width: 5,
                height: 3,
                stride: 24,
            },
            &mut i420,
        );
        assert_eq!(i420.len(), 4 * 2 * 3 / 2);
        let (u, v) = (i420[8], i420[10]);
        assert!(v > 200 && u < 128, "u {u} v {v}");
    }
}
