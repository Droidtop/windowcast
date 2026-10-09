//! Media Foundation decoding into Direct3D 11 textures. The decoder for the
//! codec is found with MFTEnumEx (Microsoft's own H.264 decoder, and the
//! HEVC and AV1 Video Extensions where installed, or a GPU vendor's) and
//! handed the window's D3D11 device through a DXGI device manager, which
//! makes it decode with DXVA on the GPU and hand out NV12 textures.

use std::mem::ManuallyDrop;
use std::sync::Once;

use windowcast_protocol::VideoCodec;
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

/// One decoded picture: a texture (an array slice of it) on the device.
pub struct Picture {
    pub texture: ID3D11Texture2D,
    pub slice: u32,
    /// The visible picture inside the texture (decoders pad to 16).
    pub width: u32,
    pub height: u32,
}

pub struct Decoder {
    transform: IMFTransform,
    _manager: IMFDXGIDeviceManager,
    provides_samples: bool,
    size: (u32, u32),
    time: i64,
    pub name: String,
}

// Created and used on the stream window's one thread.
unsafe impl Send for Decoder {}

fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

fn startup() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    });
}

fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::H265 => MFVideoFormat_HEVC,
        VideoCodec::Av1 => MFVideoFormat_AV1,
    }
}

/// Decoders for `codec` that output NV12, synchronous ones first (they
/// take a D3D manager and decode with DXVA), best first within each.
fn candidates(codec: VideoCodec) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec),
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let mut found = Vec::new();
    for flags in [MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER] {
        let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            if MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                flags,
                Some(&input),
                Some(&output),
                &mut array,
                &mut count,
            )
            .is_err()
                || array.is_null()
            {
                continue;
            }
            for i in 0..count as usize {
                if let Some(activate) = std::ptr::read(array.add(i)) {
                    found.push(activate);
                }
            }
            CoTaskMemFree(Some(array as *const _));
        }
    }
    found
}

/// Codecs with a decoder on this PC, most preferred first.
pub fn decodable() -> Vec<VideoCodec> {
    startup();
    [VideoCodec::H265, VideoCodec::H264, VideoCodec::Av1]
        .into_iter()
        .filter(|codec| !candidates(*codec).is_empty())
        .collect()
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut buffer = [0u16; 256];
    let mut len = 0u32;
    unsafe {
        if activate
            .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buffer, Some(&mut len))
            .is_ok()
        {
            return String::from_utf16_lossy(&buffer[..len as usize]);
        }
    }
    "unnamed decoder".into()
}

impl Decoder {
    /// Opens a hardware-decoding (DXVA) decoder for `codec` on `device`.
    pub fn new(codec: VideoCodec, device: &ID3D11Device) -> Result<Self, String> {
        startup();
        let mut errors = Vec::new();
        for activate in candidates(codec) {
            let name = friendly_name(&activate);
            match unsafe { Self::open(&activate, codec, device, &name) } {
                Ok(decoder) => return Ok(decoder),
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        if errors.is_empty() {
            Err(format!("this PC has no {codec:?} decoder"))
        } else {
            Err(format!(
                "no {codec:?} decoder decodes on the GPU: {}",
                errors.join("; ")
            ))
        }
    }

    unsafe fn open(
        activate: &IMFActivate,
        codec: VideoCodec,
        device: &ID3D11Device,
        name: &str,
    ) -> Result<Self, String> {
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(err("activate"))?;
            let attributes = transform.GetAttributes().map_err(err("attributes"))?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err("does not take a Direct3D 11 device".into());
            }
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(err("device manager"))?;
            let manager = manager.ok_or("no device manager")?;
            manager
                .ResetDevice(device, token)
                .map_err(err("device manager reset"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(err("set Direct3D manager"))?;

            let input = MFCreateMediaType().map_err(err("input type"))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(err("major"))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &subtype(codec))
                .map_err(err("subtype"))?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(err("set input type"))?;
            let mut decoder = Decoder {
                transform,
                _manager: manager,
                provides_samples: false,
                size: (0, 0),
                time: 0,
                name: format!("{name} (Direct3D 11)"),
            };
            decoder.set_output_type()?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(err("begin streaming"))?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(err("start of stream"))?;
            Ok(decoder)
        }
    }

    /// Picks NV12 output and notes the visible picture size.
    fn set_output_type(&mut self) -> Result<(), String> {
        unsafe {
            let mut i = 0;
            loop {
                let available = self
                    .transform
                    .GetOutputAvailableType(0, i)
                    .map_err(err("no NV12 output"))?;
                if available.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    self.transform
                        .SetOutputType(0, &available, 0)
                        .map_err(err("set output type"))?;
                    let packed = available.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                    self.size = ((packed >> 32) as u32, packed as u32);
                    let mut aperture = MFVideoArea::default();
                    if available
                        .GetBlob(
                            &MF_MT_MINIMUM_DISPLAY_APERTURE,
                            std::slice::from_raw_parts_mut(
                                &mut aperture as *mut MFVideoArea as *mut u8,
                                std::mem::size_of::<MFVideoArea>(),
                            ),
                            None,
                        )
                        .is_ok()
                        && aperture.Area.cx > 0
                        && aperture.Area.cy > 0
                    {
                        self.size = (aperture.Area.cx as u32, aperture.Area.cy as u32);
                    }
                    let info = self
                        .transform
                        .GetOutputStreamInfo(0)
                        .map_err(err("output stream info"))?;
                    self.provides_samples = info.dwFlags
                        & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                            | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                            as u32
                        != 0;
                    return Ok(());
                }
                i += 1;
            }
        }
    }

    /// Decodes one access unit; returns the pictures that came out (none
    /// while the decoder still needs input, normally one).
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<Picture>, String> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32).map_err(err("buffer"))?;
            let mut bytes = std::ptr::null_mut();
            buffer.Lock(&mut bytes, None, None).map_err(err("lock"))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), bytes, data.len());
            buffer.Unlock().map_err(err("unlock"))?;
            buffer
                .SetCurrentLength(data.len() as u32)
                .map_err(err("length"))?;
            let sample = MFCreateSample().map_err(err("sample"))?;
            sample.AddBuffer(&buffer).map_err(err("add buffer"))?;
            sample.SetSampleTime(self.time).map_err(err("time"))?;
            self.time += 166_666;
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(err("decode"))?;
            self.pull()
        }
    }

    fn pull(&mut self) -> Result<Vec<Picture>, String> {
        let mut pictures = Vec::new();
        unsafe {
            loop {
                let mut output = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(None),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                if !self.provides_samples {
                    // A decoder that wants our sample still writes into a
                    // texture of its own allocator when it has a D3D
                    // manager; this path is for completeness.
                    let size = self
                        .transform
                        .GetOutputStreamInfo(0)
                        .map_err(err("output stream info"))?
                        .cbSize
                        .max(1);
                    let out = MFCreateSample().map_err(err("sample"))?;
                    out.AddBuffer(&MFCreateMemoryBuffer(size).map_err(err("buffer"))?)
                        .map_err(err("add buffer"))?;
                    output[0].pSample = ManuallyDrop::new(Some(out));
                }
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut output, &mut status);
                let sample = ManuallyDrop::take(&mut output[0].pSample);
                drop(ManuallyDrop::take(&mut output[0].pEvents));
                match result {
                    Ok(()) => {
                        if let Some(picture) = sample.and_then(|s| self.picture(&s)) {
                            pictures.push(picture);
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.set_output_type()?,
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(pictures),
                    Err(e) => return Err(format!("decoder output: {e}")),
                }
            }
        }
    }

    fn picture(&self, sample: &IMFSample) -> Option<Picture> {
        unsafe {
            let buffer = sample.GetBufferByIndex(0).ok()?;
            let dxgi: IMFDXGIBuffer = buffer.cast().ok()?;
            let mut texture: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(
                &ID3D11Texture2D::IID,
                &mut texture as *mut Option<ID3D11Texture2D> as *mut *mut core::ffi::c_void,
            )
            .ok()?;
            let slice = dxgi.GetSubresourceIndex().ok()?;
            Some(Picture {
                texture: texture?,
                slice,
                width: self.size.0,
                height: self.size.1,
            })
        }
    }
}
