//! Captured BGRA pixels to the 4:2:0 layouts encoders take: NV12 (Media
//! Foundation) and I420 (OpenH264). BT.601 limited range, the default a
//! decoder assumes when the stream does not say. Odd edges are dropped:
//! encoders need even dimensions.

/// A captured picture: 8-bit BGRA rows, `stride` bytes apart.
pub struct Bgra<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

/// The even dimensions a picture is encoded at.
pub fn even(width: usize, height: usize) -> (usize, usize) {
    (width & !1, height & !1)
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
