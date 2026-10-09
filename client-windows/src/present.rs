//! Showing decoded pictures: the Direct3D 11 device both the decoder and
//! this use, a flip-model swap chain on the stream window, and the GPU's
//! video processor, which converts NV12 to RGB and scales the picture into
//! the window (letterboxed) in one pass. Nothing is copied to the CPU.

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use crate::decoder::Picture;

fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

/// A hardware device with video support, safe to use from the decoder's
/// threads and ours.
pub fn create_device() -> Result<ID3D11Device, String> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .map_err(err("Direct3D 11 device"))?;
        let device: ID3D11Device = device.ok_or("no Direct3D 11 device")?;
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            let _ = multithread.SetMultithreadProtected(true);
        }
        Ok(device)
    }
}

/// The video processor for one input size and one window size.
struct Processor {
    input: (u32, u32),
    output: (u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
}

pub struct Presenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    swap_chain: IDXGISwapChain1,
    size: (u32, u32),
    processor: Option<Processor>,
}

const BUFFERS: u32 = 2;

impl Presenter {
    pub fn new(device: &ID3D11Device, window: HWND, size: (u32, u32)) -> Result<Self, String> {
        unsafe {
            let context = device.GetImmediateContext().map_err(err("context"))?;
            let video_device: ID3D11VideoDevice = device.cast().map_err(err("video device"))?;
            let video_context: ID3D11VideoContext = context.cast().map_err(err("video context"))?;
            let dxgi: IDXGIDevice = device.cast().map_err(err("DXGI device"))?;
            let adapter = dxgi.GetAdapter().map_err(err("adapter"))?;
            let factory: IDXGIFactory2 = adapter.GetParent().map_err(err("factory"))?;
            let description = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0.max(1),
                Height: size.1.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: 0,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(device, window, &description, None, None)
                .map_err(err("swap chain"))?;
            // Alt+Enter would switch to exclusive fullscreen; the window
            // does its own borderless fullscreen instead.
            let _ = factory.MakeWindowAssociation(window, DXGI_MWA_NO_ALT_ENTER);
            Ok(Presenter {
                device: device.clone(),
                context,
                video_device,
                video_context,
                swap_chain,
                size: (size.0.max(1), size.1.max(1)),
                processor: None,
            })
        }
    }

    /// Follows the window's client size.
    pub fn resize(&mut self, size: (u32, u32)) -> Result<(), String> {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            return Ok(());
        }
        unsafe {
            self.context.ClearState();
            self.swap_chain
                .ResizeBuffers(
                    BUFFERS,
                    size.0,
                    size.1,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
                .map_err(err("resize"))?;
        }
        self.size = size;
        Ok(())
    }

    fn processor(&mut self, input: (u32, u32)) -> Result<&Processor, String> {
        let fresh = self
            .processor
            .as_ref()
            .is_none_or(|p| p.input != input || p.output != self.size);
        if fresh {
            unsafe {
                let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                    InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                    InputWidth: input.0,
                    InputHeight: input.1,
                    OutputWidth: self.size.0,
                    OutputHeight: self.size.1,
                    Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
                    ..Default::default()
                };
                let enumerator = self
                    .video_device
                    .CreateVideoProcessorEnumerator(&content)
                    .map_err(err("video processor enumerator"))?;
                let processor = self
                    .video_device
                    .CreateVideoProcessor(&enumerator, 0)
                    .map_err(err("video processor"))?;
                // The host encodes BT.601 at studio range (agent-windows
                // convert.rs); the window is full-range RGB.
                let input_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 };
                let output_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                self.video_context
                    .VideoProcessorSetStreamColorSpace(&processor, 0, &input_space);
                self.video_context
                    .VideoProcessorSetOutputColorSpace(&processor, &output_space);
                self.video_context.VideoProcessorSetStreamFrameFormat(
                    &processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                self.processor = Some(Processor {
                    input,
                    output: self.size,
                    enumerator,
                    processor,
                });
            }
        }
        Ok(self.processor.as_ref().expect("processor"))
    }

    /// Shows one picture, scaled to fit the window with black bars.
    pub fn present(&mut self, picture: &Picture) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { picture.texture.GetDesc(&mut desc) };
        let input = (
            picture.width.min(desc.Width).max(2),
            picture.height.min(desc.Height).max(2),
        );
        let size = self.size;
        let video_device = self.video_device.clone();
        let video_context = self.video_context.clone();
        let back: ID3D11Texture2D =
            unsafe { self.swap_chain.GetBuffer(0) }.map_err(err("back buffer"))?;
        let processor = self.processor((desc.Width, desc.Height))?;
        unsafe {
            let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: picture.slice,
                    },
                },
            };
            let mut input_view = None;
            video_device
                .CreateVideoProcessorInputView(
                    &picture.texture,
                    &processor.enumerator,
                    &input_view_desc,
                    Some(&mut input_view),
                )
                .map_err(err("input view"))?;
            let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output_view = None;
            video_device
                .CreateVideoProcessorOutputView(
                    &back,
                    &processor.enumerator,
                    &output_view_desc,
                    Some(&mut output_view),
                )
                .map_err(err("output view"))?;

            let source = RECT {
                left: 0,
                top: 0,
                right: input.0 as i32,
                bottom: input.1 as i32,
            };
            let target = letterbox(input, size);
            let whole = RECT {
                left: 0,
                top: 0,
                right: size.0 as i32,
                bottom: size.1 as i32,
            };
            video_context.VideoProcessorSetStreamSourceRect(
                &processor.processor,
                0,
                true,
                Some(&source),
            );
            video_context.VideoProcessorSetStreamDestRect(
                &processor.processor,
                0,
                true,
                Some(&target),
            );
            video_context.VideoProcessorSetOutputTargetRect(
                &processor.processor,
                true,
                Some(&whole),
            );
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            video_context.VideoProcessorSetOutputBackgroundColor(
                &processor.processor,
                false,
                &black,
            );
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(input_view),
                ..Default::default()
            };
            let streams = [stream];
            let blitted = video_context.VideoProcessorBlt(
                &processor.processor,
                output_view.as_ref().expect("output view"),
                0,
                &streams,
            );
            let [stream] = streams;
            drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
            blitted.map_err(err("video processor"))?;
            // Interval 0: shown at the next composition, no extra frame of
            // waiting for vertical blank (the compositor never tears).
            self.swap_chain
                .Present(0, DXGI_PRESENT(0))
                .ok()
                .map_err(err("present"))?;
        }
        let _ = &self.device;
        Ok(())
    }
}

/// The largest rectangle of the picture's aspect ratio centred in `out`.
fn letterbox(picture: (u32, u32), out: (u32, u32)) -> RECT {
    let (pw, ph) = (picture.0 as u64, picture.1 as u64);
    let (ow, oh) = (out.0 as u64, out.1 as u64);
    let (w, h) = if pw * oh > ph * ow {
        (ow, ph * ow / pw)
    } else {
        (pw * oh / ph, oh)
    };
    let left = (ow - w) / 2;
    let top = (oh - h) / 2;
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + w) as i32,
        bottom: (top + h) as i32,
    }
}

/// Where a point in the window falls on the picture, from 0 to 1 on each
/// axis; `None` in the black bars.
pub fn to_picture(picture: (u32, u32), out: (u32, u32), x: i32, y: i32) -> Option<(f32, f32)> {
    if picture.0 == 0 || picture.1 == 0 {
        return None;
    }
    let target = letterbox(picture, (out.0.max(1), out.1.max(1)));
    let w = (target.right - target.left).max(1) as f32;
    let h = (target.bottom - target.top).max(1) as f32;
    let px = (x - target.left) as f32 / w;
    let py = (y - target.top) as f32 / h;
    ((0.0..=1.0).contains(&px) && (0.0..=1.0).contains(&py)).then_some((px, py))
}
