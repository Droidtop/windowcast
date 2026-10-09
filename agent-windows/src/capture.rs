//! Per-window capture with Windows.Graphics.Capture: the window's own
//! DWM surface, whatever covers it, read back as BGRA. A free-threaded
//! frame pool is polled from the stream's thread; nothing needs a message
//! loop.

use std::time::{Duration, Instant};

use windows::core::{factory, Interface};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::convert::Bgra;

const PIXEL_FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
const BUFFERS: i32 = 2;

/// Joins the multithreaded apartment; harmless when already in it.
pub fn init_thread() {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

pub fn supported() -> bool {
    init_thread();
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

pub struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    pool_size: SizeInt32,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    pixels: Vec<u8>,
    width: usize,
    height: usize,
}

fn create_device(
    driver: D3D_DRIVER_TYPE,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((device.expect("device"), context.expect("context")))
}

impl Capture {
    pub fn new(window: HWND) -> windows::core::Result<Self> {
        init_thread();
        // A GPU when there is one; WARP (software) otherwise, e.g. a VM.
        let (device, context) = create_device(D3D_DRIVER_TYPE_HARDWARE)
            .or_else(|_| create_device(D3D_DRIVER_TYPE_WARP))?;
        let dxgi: IDXGIDevice = device.cast()?;
        let winrt_device: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;

        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(window)? };
        let pool_size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            PIXEL_FORMAT,
            BUFFERS,
            pool_size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Windows 11 can drop the yellow capture border; older Windows
        // has no such call and keeps it.
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;
        Ok(Capture {
            device,
            context,
            winrt_device,
            pool,
            session,
            pool_size,
            staging: None,
            pixels: Vec::new(),
            width: 0,
            height: 0,
        })
    }

    /// Waits up to `timeout` for a new picture. True when one arrived; a
    /// window that does not change sends none.
    pub fn poll(&mut self, timeout: Duration) -> windows::core::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(frame) = self.pool.TryGetNextFrame() {
                let size = frame.ContentSize()?;
                if size.Width != self.pool_size.Width || size.Height != self.pool_size.Height {
                    // The window was resized: the pool follows, and the next
                    // frame has the new size.
                    self.pool_size = size;
                    self.pool
                        .Recreate(&self.winrt_device, PIXEL_FORMAT, BUFFERS, size)?;
                    continue;
                }
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
                self.read_back(
                    &texture,
                    size.Width.max(0) as usize,
                    size.Height.max(0) as usize,
                )?;
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The latest picture, if any arrived yet.
    pub fn picture(&self) -> Option<Bgra<'_>> {
        (self.width >= 2 && self.height >= 2).then(|| Bgra {
            data: &self.pixels,
            width: self.width,
            height: self.height,
            stride: self.width * 4,
        })
    }

    fn read_back(
        &mut self,
        texture: &ID3D11Texture2D,
        width: usize,
        height: usize,
    ) -> windows::core::Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let staging = match &self.staging {
            Some((staging, w, h)) if *w == desc.Width && *h == desc.Height => staging.clone(),
            _ => {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    MipLevels: 1,
                    ArraySize: 1,
                    ..desc
                };
                let mut staging = None;
                unsafe {
                    self.device
                        .CreateTexture2D(&staging_desc, None, Some(&mut staging))?
                };
                let staging = staging.expect("staging texture");
                self.staging = Some((staging.clone(), desc.Width, desc.Height));
                staging
            }
        };
        let width = width.min(desc.Width as usize);
        let height = height.min(desc.Height as usize);
        unsafe {
            self.context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let source = std::slice::from_raw_parts(
                mapped.pData as *const u8,
                mapped.RowPitch as usize * height,
            );
            self.pixels.resize(width * height * 4, 0);
            for row in 0..height {
                let from = row * mapped.RowPitch as usize;
                self.pixels[row * width * 4..(row + 1) * width * 4]
                    .copy_from_slice(&source[from..from + width * 4]);
            }
            self.context.Unmap(&staging, 0);
        }
        self.width = width;
        self.height = height;
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
