//! Colour conversion on the GPU: the captured BGRA texture goes through
//! the GPU's video processor (the same unit the client uses on the way
//! out) into an NV12 texture, cut to the window and at the encoder's even
//! size, and only that NV12 picture is read back: 1.5 bytes a pixel
//! instead of 4, and no conversion loop on the processor. The output is
//! BT.601 at studio range, like `convert`'s, which the clients expect.

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

/// The processor and textures for one input and one output size.
struct Stage {
    input: (u32, u32),
    output: (u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    nv12: ID3D11Texture2D,
    staging: ID3D11Texture2D,
}

pub struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    stage: Option<Stage>,
}

impl Converter {
    /// Fails on a device without video support (WARP, some VMs); the
    /// caller then converts on the processor.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> windows::core::Result<Self> {
        Ok(Converter {
            device: device.clone(),
            context: context.clone(),
            video_device: device.cast()?,
            video_context: context.cast()?,
            stage: None,
        })
    }

    fn stage(&mut self, input: (u32, u32), output: (u32, u32)) -> windows::core::Result<&Stage> {
        let fresh = self
            .stage
            .as_ref()
            .is_none_or(|stage| stage.input != input || stage.output != output);
        if fresh {
            unsafe {
                let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                    InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                    InputWidth: input.0,
                    InputHeight: input.1,
                    OutputWidth: output.0,
                    OutputHeight: output.1,
                    Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
                    ..Default::default()
                };
                let enumerator = self.video_device.CreateVideoProcessorEnumerator(&content)?;
                let processor = self.video_device.CreateVideoProcessor(&enumerator, 0)?;
                // Full-range RGB in; BT.601 studio-range YCbCr out.
                let rgb = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                let studio = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 };
                self.video_context
                    .VideoProcessorSetStreamColorSpace(&processor, 0, &rgb);
                self.video_context
                    .VideoProcessorSetOutputColorSpace(&processor, &studio);
                self.video_context.VideoProcessorSetStreamFrameFormat(
                    &processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                let mut desc = D3D11_TEXTURE2D_DESC {
                    Width: output.0,
                    Height: output.1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut nv12 = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut nv12))?;
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                let mut staging = None;
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut staging))?;
                self.stage = Some(Stage {
                    input,
                    output,
                    enumerator,
                    processor,
                    nv12: nv12.expect("NV12 texture"),
                    staging: staging.expect("staging texture"),
                });
            }
        }
        Ok(self.stage.as_ref().expect("stage"))
    }

    /// Converts `rect` (left, top, width, height) of `texture` to NV12 at
    /// the rectangle's even size and reads it into `out`: the Y rows, then
    /// the interleaved UV rows, tightly packed. Returns the picture size.
    pub fn convert(
        &mut self,
        texture: &ID3D11Texture2D,
        rect: (usize, usize, usize, usize),
        out: &mut Vec<u8>,
    ) -> windows::core::Result<(usize, usize)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let (left, top, width, height) = rect;
        let (w, h) = (width & !1, height & !1);
        let video_device = self.video_device.clone();
        let video_context = self.video_context.clone();
        let context = self.context.clone();
        let stage = self.stage((desc.Width, desc.Height), (w as u32, h as u32))?;
        unsafe {
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input = None;
            video_device.CreateVideoProcessorInputView(
                texture,
                &stage.enumerator,
                &input_desc,
                Some(&mut input),
            )?;
            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output = None;
            video_device.CreateVideoProcessorOutputView(
                &stage.nv12,
                &stage.enumerator,
                &output_desc,
                Some(&mut output),
            )?;
            let source = RECT {
                left: left as i32,
                top: top as i32,
                right: (left + w) as i32,
                bottom: (top + h) as i32,
            };
            let target = RECT {
                left: 0,
                top: 0,
                right: w as i32,
                bottom: h as i32,
            };
            video_context.VideoProcessorSetStreamSourceRect(
                &stage.processor,
                0,
                true,
                Some(&source),
            );
            video_context.VideoProcessorSetStreamDestRect(&stage.processor, 0, true, Some(&target));
            video_context.VideoProcessorSetOutputTargetRect(&stage.processor, true, Some(&target));
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(input),
                ..Default::default()
            };
            let streams = [stream];
            let blitted = video_context.VideoProcessorBlt(
                &stage.processor,
                output.as_ref().expect("output view"),
                0,
                &streams,
            );
            let [stream] = streams;
            drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
            blitted?;

            context.CopyResource(&stage.staging, &stage.nv12);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&stage.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            out.resize(w * h * 3 / 2, 0);
            // NV12 in a mapped texture: h rows of Y, then h/2 rows of UV,
            // each `pitch` bytes apart.
            for row in 0..h + h / 2 {
                let from = std::slice::from_raw_parts(base.add(row * pitch), w);
                out[row * w..row * w + w].copy_from_slice(from);
            }
            context.Unmap(&stage.staging, 0);
        }
        Ok((w, h))
    }
}
