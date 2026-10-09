//! Video encoders behind one interface. Media Foundation is the common
//! interface to the GPU vendors' encoders on Windows: NVIDIA (NVENC), AMD
//! (AMF/VCE) and Intel (Quick Sync) each ship a hardware encoder MFT, so
//! `MfHardware` is whichever of them the machine has, H.264 and, where the
//! vendor offers it, H.265. `MfSoftware` is Microsoft's own H.264 MFT and
//! `OpenH264` the software encoder of last resort. Direct NVENC/AMF/QSV SDK
//! paths can be added as further `EncoderChoice` values behind the same
//! trait.

use std::mem::ManuallyDrop;
use std::sync::Once;
use std::time::Duration;

use windowcast_protocol::VideoCodec;
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

use crate::convert::{self, Picture};

/// Which encoder to use; an agent option (`--encoder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderChoice {
    /// The best available: hardware, then Microsoft's, then OpenH264.
    Auto,
    /// The first hardware encoder Media Foundation offers.
    MfHardware,
    /// One GPU vendor's hardware encoder MFT: NVIDIA's NVENC, Intel's
    /// Quick Sync, AMD's AMF. For machines with more than one GPU.
    Nvenc,
    QuickSync,
    Amf,
    MfSoftware,
    OpenH264,
}

impl EncoderChoice {
    pub const ALL: [EncoderChoice; 7] = [
        Self::Auto,
        Self::MfHardware,
        Self::Nvenc,
        Self::QuickSync,
        Self::Amf,
        Self::MfSoftware,
        Self::OpenH264,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|choice| choice.name() == name)
    }

    /// The option's name, as `parse` takes it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::MfHardware => "mf-hardware",
            Self::Nvenc => "nvenc",
            Self::QuickSync => "quicksync",
            Self::Amf => "amf",
            Self::MfSoftware => "mf-software",
            Self::OpenH264 => "openh264",
        }
    }

    fn hardware(self) -> bool {
        matches!(
            self,
            Self::MfHardware | Self::Nvenc | Self::QuickSync | Self::Amf
        )
    }

    /// The PCI vendor id the MFT reports (MFT_ENUM_HARDWARE_VENDOR_ID).
    fn vendor(self) -> Option<&'static str> {
        match self {
            Self::Nvenc => Some("VEN_10DE"),
            Self::QuickSync => Some("VEN_8086"),
            Self::Amf => Some("VEN_1002"),
            _ => None,
        }
    }

    /// The concrete encoders to try for `codec`, in order.
    fn candidates(self, codec: VideoCodec) -> Vec<EncoderChoice> {
        let all = match self {
            Self::Auto => vec![Self::MfHardware, Self::MfSoftware, Self::OpenH264],
            one => vec![one],
        };
        all.into_iter()
            .filter(|choice| codec == VideoCodec::H264 || choice.hardware())
            .collect()
    }
}

pub struct Settings {
    pub codec: VideoCodec,
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub bitrate: u32,
}

pub trait Encoder {
    /// Encodes one picture (already the encoder's size). Returns the access
    /// units that came out, each one whole frame, oldest first: none while
    /// an encoder holds on to a picture, more than one when it catches up.
    fn encode(
        &mut self,
        picture: &Picture<'_>,
        keyframe: bool,
        time: Duration,
    ) -> Result<Vec<Vec<u8>>, String>;
    fn describe(&self) -> String;

    /// Whether it takes NV12 textures on the capture device as they are
    /// (`Picture::Texture`), with no copy through memory.
    fn takes_textures(&self) -> bool {
        false
    }
}

/// Opens the first encoder from `choice` that works for `settings`.
pub fn open(choice: EncoderChoice, settings: &Settings) -> Result<Box<dyn Encoder>, String> {
    open_on(choice, settings, None)
}

/// Like [`open`], and a hardware encoder on the same GPU as `device` (the
/// capture device) takes its textures directly (`Encoder::takes_textures`).
pub fn open_on(
    choice: EncoderChoice,
    settings: &Settings,
    device: Option<&ID3D11Device>,
) -> Result<Box<dyn Encoder>, String> {
    let mut errors = Vec::new();
    for candidate in choice.candidates(settings.codec) {
        let opened: Result<Box<dyn Encoder>, String> = match candidate {
            EncoderChoice::OpenH264 => OpenH264Encoder::new(settings).map(|e| Box::new(e) as _),
            EncoderChoice::Auto => unreachable!("expanded by candidates"),
            media_foundation => {
                MfEncoder::new(media_foundation, settings, device).map(|e| Box::new(e) as _)
            }
        };
        match opened {
            Ok(encoder) => return Ok(encoder),
            Err(e) => errors.push(format!("{candidate:?}: {e}")),
        }
    }
    Err(format!(
        "no encoder for {:?}: {}",
        settings.codec,
        errors.join("; ")
    ))
}

/// Codecs `choice` can produce on this machine, most preferred first.
pub fn available_codecs(choice: EncoderChoice) -> Vec<VideoCodec> {
    let mut codecs = Vec::new();
    let hardware = match choice {
        EncoderChoice::Auto => Some(EncoderChoice::MfHardware),
        choice if choice.hardware() => Some(choice),
        _ => None,
    };
    for codec in [VideoCodec::Av1, VideoCodec::H265] {
        if hardware.is_some_and(|hardware| !matching(hardware, codec).is_empty()) {
            codecs.push(codec);
        }
    }
    let h264 = match choice {
        EncoderChoice::Auto | EncoderChoice::OpenH264 => true,
        other => !matching(other, VideoCodec::H264).is_empty(),
    };
    if h264 {
        codecs.push(VideoCodec::H264);
    }
    codecs
}

/// The Media Foundation encoders `choice` stands for, best first.
fn matching(choice: EncoderChoice, codec: VideoCodec) -> Vec<IMFActivate> {
    enumerate(choice.hardware(), codec)
        .into_iter()
        .filter(|activate| {
            choice
                .vendor()
                .is_none_or(|vendor| vendor_id(activate).is_some_and(|id| id.contains(vendor)))
        })
        .collect()
}

fn vendor_id(activate: &IMFActivate) -> Option<String> {
    let mut buffer = [0u16; 128];
    let mut len = 0u32;
    unsafe {
        activate
            .GetString(
                &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
                &mut buffer,
                Some(&mut len),
            )
            .ok()?;
    }
    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

/// One line per Media Foundation encoder on this machine, for `--list-encoders`.
pub fn list() -> Vec<String> {
    let mut lines = Vec::new();
    for hardware in [true, false] {
        for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
            for activate in enumerate(hardware, codec) {
                lines.push(format!(
                    "{} {:?}: {} ({})",
                    if hardware { "hardware" } else { "software" },
                    codec,
                    friendly_name(&activate),
                    vendor_id(&activate).unwrap_or_default()
                ));
            }
        }
    }
    lines.push("software H264: OpenH264".into());
    lines
}

fn startup() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| unsafe {
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

fn enumerate(hardware: bool, codec: VideoCodec) -> Vec<IMFActivate> {
    startup();
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec),
    };
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
    };
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let mut found = Vec::new();
    unsafe {
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut array,
            &mut count,
        )
        .is_err()
            || array.is_null()
        {
            return found;
        }
        for i in 0..count as usize {
            if let Some(activate) = std::ptr::read(array.add(i)) {
                found.push(activate);
            }
        }
        CoTaskMemFree(Some(array as *const _));
    }
    found
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
    "unnamed encoder".into()
}

/// Maps a Windows error to text naming the step that failed.
fn err(what: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{what}: {e}")
}

fn packed(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// Frames between periodic keyframes: two seconds. A client that loses a
/// picture asks for a keyframe at once, so the period only bounds how long
/// a client that joins or misses the request waits.
fn gop(fps: u32) -> u32 {
    fps * 2
}

/// Encoder properties for interactive streaming. Returns the ones the
/// encoder refused (not every MFT supports every property).
fn configure(api: &ICodecAPI, fps: u32) -> Vec<&'static str> {
    let settings: [(&GUID, VARIANT, &'static str); 2] = [
        (
            &CODECAPI_AVLowLatencyMode,
            VARIANT::from(true),
            "low latency",
        ),
        (
            &CODECAPI_AVEncMPVGOPSize,
            VARIANT::from(gop(fps)),
            "GOP size",
        ),
    ];
    settings
        .iter()
        .filter(|(property, value, _)| unsafe { api.SetValue(*property, value) }.is_err())
        .map(|(_, _, name)| *name)
        .collect()
}

/// A Media Foundation encoder MFT, synchronous (Microsoft's) or
/// asynchronous (every hardware one).
struct MfEncoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    input_id: u32,
    output_id: u32,
    provides_samples: bool,
    output_size: u32,
    frame_time: i64,
    /// Input requests an asynchronous encoder made while we had nothing.
    need_input: u32,
    nv12: Vec<u8>,
    name: String,
    /// Set when the encoder has the capture device and takes its textures.
    manager: Option<IMFDXGIDeviceManager>,
}

// Created and used on the stream's one thread, in the multithreaded
// apartment, where these Media Foundation objects are free-threaded.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    fn new(
        choice: EncoderChoice,
        s: &Settings,
        device: Option<&ID3D11Device>,
    ) -> Result<Self, String> {
        // With a capture device, the encoder on that same GPU first.
        let luid = device.and_then(adapter_luid);
        let mut candidates = matching(choice, s.codec);
        candidates.sort_by_key(|activate| {
            luid.is_none()
                || activate
                    .cast::<IMFAttributes>()
                    .ok()
                    .as_ref()
                    .and_then(mft_luid)
                    != luid
        });
        let activate = candidates
            .into_iter()
            .next()
            .ok_or("no such encoder on this machine")?;
        let name = friendly_name(&activate);
        let activate_luid = activate
            .cast::<IMFAttributes>()
            .ok()
            .as_ref()
            .and_then(mft_luid);
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(err("activate"))?;
            let mut events = None;
            if let Ok(attributes) = transform.GetAttributes() {
                if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                    attributes
                        .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                        .map_err(err("unlock"))?;
                    events = Some(
                        transform
                            .cast::<IMFMediaEventGenerator>()
                            .map_err(err("events"))?,
                    );
                }
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            // The capture device, for an encoder on its GPU that takes
            // Direct3D 11 textures: frames then never leave the GPU.
            let encoder_luid = activate_luid.or_else(|| {
                transform
                    .GetAttributes()
                    .ok()
                    .as_ref()
                    .and_then(|a| mft_luid(&a.cast::<IMFAttributes>().ok()?))
            });
            // Same GPU: the adapters' LUIDs match, or, for an encoder that
            // does not say (NVIDIA's), the GPU vendors do; a vendor's
            // Direct3D-aware encoder then works on the device it is given.
            let same_gpu = match (encoder_luid, luid) {
                (Some(encoder), Some(capture)) => encoder == capture,
                (None, Some(_)) => device.and_then(adapter_vendor).is_some_and(|vendor| {
                    vendor_id(&activate).is_some_and(|id| id.contains(&vendor))
                }),
                _ => false,
            };
            let manager = match device {
                Some(device) if same_gpu => give_device(&transform, device),
                _ => None,
            };

            // E_NOTIMPL means the streams are simply numbered from 0.
            let (mut inputs, mut outputs) = ([0u32], [0u32]);
            if transform.GetStreamIDs(&mut inputs, &mut outputs).is_err() {
                (inputs, outputs) = ([0], [0]);
            }
            let (input_id, output_id) = (inputs[0], outputs[0]);

            let (w, h) = (s.width as u32, s.height as u32);
            // Before the types as well as after: NVIDIA's encoders read
            // their rate-control and GOP settings when the types are set.
            if let Ok(api) = transform.cast::<ICodecAPI>() {
                configure(&api, s.fps);
            }
            let output = MFCreateMediaType().map_err(err("output type"))?;
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(err("major"))?;
            output
                .SetGUID(&MF_MT_SUBTYPE, &subtype(s.codec))
                .map_err(err("subtype"))?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, s.bitrate)
                .map_err(err("bitrate"))?;
            output
                .SetUINT64(&MF_MT_FRAME_SIZE, packed(w, h))
                .map_err(err("size"))?;
            output
                .SetUINT64(&MF_MT_FRAME_RATE, packed(s.fps, 1))
                .map_err(err("rate"))?;
            output
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed(1, 1))
                .map_err(err("par"))?;
            output
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(err("interlace"))?;
            // The keyframe interval (GOP) on the type as well as through
            // ICodecAPI: NVIDIA's encoders only take it here.
            let _ = output.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, gop(s.fps));
            if s.codec == VideoCodec::H264 {
                // Constrained Baseline, as the session's H.264 codec says.
                output
                    .SetUINT32(&MF_MT_MPEG2_PROFILE, 66)
                    .map_err(err("profile"))?;
            }
            transform
                .SetOutputType(output_id, &output, 0)
                .map_err(err("set output type"))?;

            let input = MFCreateMediaType().map_err(err("input type"))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(err("major"))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(err("subtype"))?;
            input
                .SetUINT64(&MF_MT_FRAME_SIZE, packed(w, h))
                .map_err(err("size"))?;
            input
                .SetUINT64(&MF_MT_FRAME_RATE, packed(s.fps, 1))
                .map_err(err("rate"))?;
            input
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed(1, 1))
                .map_err(err("par"))?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(err("interlace"))?;
            transform
                .SetInputType(input_id, &input, 0)
                .map_err(err("set input type"))?;

            let codec_api = transform.cast::<ICodecAPI>().ok();
            let rejected = codec_api
                .as_ref()
                .map(|api| configure(api, s.fps))
                .unwrap_or_else(|| vec!["ICodecAPI"]);

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(err("begin streaming"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(err("start of stream"))?;
            let info = transform
                .GetOutputStreamInfo(output_id)
                .map_err(err("output stream info"))?;

            Ok(MfEncoder {
                transform,
                events,
                codec_api,
                input_id,
                output_id,
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                output_size: info.cbSize.max(1 << 20),
                frame_time: 10_000_000 / i64::from(s.fps),
                need_input: 0,
                nv12: Vec::new(),
                name: format!(
                    "Media Foundation {} ({name}){}{}",
                    if choice.hardware() {
                        "hardware"
                    } else {
                        "software"
                    },
                    if manager.is_some() {
                        ", frames stay on the GPU"
                    } else {
                        ""
                    },
                    if rejected.is_empty() {
                        String::new()
                    } else {
                        format!(", ignores {}", rejected.join(", "))
                    }
                ),
                manager,
            })
        }
    }

    fn sample(&mut self, picture: &Picture<'_>, time: Duration) -> Result<IMFSample, String> {
        let buffer = match picture {
            Picture::Texture { texture, .. } => unsafe {
                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)
                    .map_err(err("texture buffer"))?;
                let length = buffer
                    .cast::<IMF2DBuffer>()
                    .and_then(|b| b.GetContiguousLength())
                    .map_err(err("texture length"))?;
                buffer
                    .SetCurrentLength(length)
                    .map_err(err("texture length"))?;
                buffer
            },
            _ => {
                picture.to_nv12(&mut self.nv12)?;
                self.memory_buffer().map_err(err("buffer"))?
            }
        };
        unsafe {
            let sample = MFCreateSample().map_err(err("sample"))?;
            sample.AddBuffer(&buffer).map_err(err("add buffer"))?;
            sample
                .SetSampleTime((time.as_nanos() / 100) as i64)
                .map_err(err("time"))?;
            sample
                .SetSampleDuration(self.frame_time)
                .map_err(err("duration"))?;
            Ok(sample)
        }
    }

    fn memory_buffer(&self) -> windows::core::Result<IMFMediaBuffer> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(self.nv12.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), data, self.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(self.nv12.len() as u32)?;
            Ok(buffer)
        }
    }

    /// One output from the encoder, if it has one.
    fn pull(&self) -> windows::core::Result<Option<Vec<u8>>> {
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                Some(sample)
            };
            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut output, &mut status);
            let sample = ManuallyDrop::take(&mut output[0].pSample);
            drop(ManuallyDrop::take(&mut output[0].pEvents));
            match result {
                Ok(()) => {
                    let Some(sample) = sample else {
                        return Ok(None);
                    };
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let mut data = std::ptr::null_mut();
                    let mut len = 0u32;
                    buffer.Lock(&mut data, None, Some(&mut len))?;
                    let bytes = std::slice::from_raw_parts(data, len as usize).to_vec();
                    buffer.Unlock()?;
                    Ok(Some(bytes))
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let new_type = self.transform.GetOutputAvailableType(self.output_id, 0)?;
                    self.transform.SetOutputType(self.output_id, &new_type, 0)?;
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        }
    }

    fn encode_sync(&mut self, sample: &IMFSample) -> windows::core::Result<Vec<Vec<u8>>> {
        unsafe { self.transform.ProcessInput(self.input_id, sample, 0)? };
        let mut out = Vec::new();
        while let Some(bytes) = self.pull()? {
            out.push(bytes);
        }
        Ok(out)
    }

    /// Asynchronous MFTs say when they want input and when output is
    /// ready. Feeds this picture when asked, and returns the first output
    /// after it (or what there is when the encoder holds on to it).
    fn encode_async(
        &mut self,
        events: &IMFMediaEventGenerator,
        sample: &IMFSample,
    ) -> windows::core::Result<Vec<Vec<u8>>> {
        let mut given = false;
        if self.need_input > 0 {
            self.need_input -= 1;
            unsafe { self.transform.ProcessInput(self.input_id, sample, 0)? };
            given = true;
        }
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if (given && !out.is_empty()) || std::time::Instant::now() >= deadline {
                        return Ok(out);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(e) => return Err(e),
            };
            let kind = unsafe { event.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                if given {
                    self.need_input += 1;
                } else {
                    unsafe { self.transform.ProcessInput(self.input_id, sample, 0)? };
                    given = true;
                }
            } else if kind == METransformHaveOutput.0 as u32 {
                if let Some(bytes) = self.pull()? {
                    out.push(bytes);
                }
                if given {
                    return Ok(out);
                }
            }
        }
    }
}

impl Encoder for MfEncoder {
    fn encode(
        &mut self,
        picture: &Picture<'_>,
        keyframe: bool,
        time: Duration,
    ) -> Result<Vec<Vec<u8>>, String> {
        if keyframe {
            if let Some(api) = &self.codec_api {
                let _ = unsafe {
                    api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32))
                };
            }
        }
        let sample = self.sample(picture, time)?;
        match self.events.clone() {
            Some(events) => self.encode_async(&events, &sample),
            None => self.encode_sync(&sample),
        }
        .map_err(|e| e.to_string())
    }

    fn describe(&self) -> String {
        self.name.clone()
    }

    fn takes_textures(&self) -> bool {
        self.manager.is_some()
    }
}

/// The LUID of the adapter `device` is on.
fn adapter_luid(device: &ID3D11Device) -> Option<u64> {
    unsafe {
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let desc = dxgi.GetAdapter().ok()?.GetDesc().ok()?;
        Some(((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64)
    }
}

/// The PCI vendor of the adapter `device` is on, as MFTs name it
/// ("VEN_10DE").
fn adapter_vendor(device: &ID3D11Device) -> Option<String> {
    unsafe {
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let desc = dxgi.GetAdapter().ok()?.GetDesc().ok()?;
        Some(format!("VEN_{:04X}", desc.VendorId))
    }
}

/// The LUID of the adapter a hardware encoder MFT runs on (a blob or a
/// number, depending on who registered it).
fn mft_luid(attributes: &IMFAttributes) -> Option<u64> {
    unsafe {
        if let Ok(luid) = attributes.GetUINT64(&MFT_ENUM_ADAPTER_LUID) {
            return Some(luid);
        }
        let mut bytes = [0u8; 8];
        let mut size = 0u32;
        attributes
            .GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut bytes, Some(&mut size))
            .ok()?;
        // LUID { LowPart: u32, HighPart: i32 }.
        let low = u32::from_le_bytes(bytes[..4].try_into().ok()?) as u64;
        let high = u32::from_le_bytes(bytes[4..].try_into().ok()?) as u64;
        (size == 8).then_some((high << 32) | low)
    }
}

/// Gives a Direct3D 11-aware encoder the capture device; `None` when it
/// does not take one.
unsafe fn give_device(
    transform: &IMFTransform,
    device: &ID3D11Device,
) -> Option<IMFDXGIDeviceManager> {
    unsafe {
        let attributes = transform.GetAttributes().ok()?;
        if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
            return None;
        }
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager).ok()?;
        let manager = manager?;
        manager.ResetDevice(device, token).ok()?;
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .ok()?;
        Some(manager)
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// OpenH264, in software (host-core's).
struct OpenH264Encoder(windowcast_host::video::OpenH264);

impl OpenH264Encoder {
    fn new(s: &Settings) -> Result<Self, String> {
        windowcast_host::video::OpenH264::new(s.bitrate, s.fps).map(OpenH264Encoder)
    }
}

impl Encoder for OpenH264Encoder {
    fn encode(
        &mut self,
        picture: &Picture<'_>,
        keyframe: bool,
        _time: Duration,
    ) -> Result<Vec<Vec<u8>>, String> {
        let (w, h) = convert::even(picture.width(), picture.height());
        Ok(self
            .0
            .encode(w, h, keyframe, |i420| picture.to_i420(i420))?
            .into_iter()
            .collect())
    }

    fn describe(&self) -> String {
        "OpenH264 (software)".into()
    }
}
