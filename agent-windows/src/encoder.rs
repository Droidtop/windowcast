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

use crate::convert::{self, Bgra};

/// Which encoder to use; an agent option (`--encoder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderChoice {
    /// The best available: hardware, then Microsoft's, then OpenH264.
    Auto,
    MfHardware,
    MfSoftware,
    OpenH264,
}

impl EncoderChoice {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "mf-hardware" => Some(Self::MfHardware),
            "mf-software" => Some(Self::MfSoftware),
            "openh264" => Some(Self::OpenH264),
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
            .filter(|choice| codec == VideoCodec::H264 || *choice == Self::MfHardware)
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
        picture: &Bgra<'_>,
        keyframe: bool,
        time: Duration,
    ) -> Result<Vec<Vec<u8>>, String>;
    fn describe(&self) -> String;
}

/// Opens the first encoder from `choice` that works for `settings`.
pub fn open(choice: EncoderChoice, settings: &Settings) -> Result<Box<dyn Encoder>, String> {
    let mut errors = Vec::new();
    for candidate in choice.candidates(settings.codec) {
        let opened: Result<Box<dyn Encoder>, String> = match candidate {
            EncoderChoice::MfHardware => MfEncoder::new(true, settings).map(|e| Box::new(e) as _),
            EncoderChoice::MfSoftware => MfEncoder::new(false, settings).map(|e| Box::new(e) as _),
            EncoderChoice::OpenH264 => OpenH264Encoder::new(settings).map(|e| Box::new(e) as _),
            EncoderChoice::Auto => unreachable!("expanded by candidates"),
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
    let hardware_allowed = matches!(choice, EncoderChoice::Auto | EncoderChoice::MfHardware);
    if hardware_allowed && !enumerate(true, VideoCodec::H265).is_empty() {
        codecs.push(VideoCodec::H265);
    }
    let h264 = match choice {
        EncoderChoice::Auto | EncoderChoice::OpenH264 => true,
        EncoderChoice::MfHardware => !enumerate(true, VideoCodec::H264).is_empty(),
        EncoderChoice::MfSoftware => !enumerate(false, VideoCodec::H264).is_empty(),
    };
    if h264 {
        codecs.push(VideoCodec::H264);
    }
    codecs
}

/// One line per Media Foundation encoder on this machine, for `--list-encoders`.
pub fn list() -> Vec<String> {
    let mut lines = Vec::new();
    for hardware in [true, false] {
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            for activate in enumerate(hardware, codec) {
                lines.push(format!(
                    "{} {:?}: {}",
                    if hardware { "hardware" } else { "software" },
                    codec,
                    friendly_name(&activate)
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
            VARIANT::from(fps * 2),
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
}

// Created and used on the stream's one thread, in the multithreaded
// apartment, where these Media Foundation objects are free-threaded.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    fn new(hardware: bool, s: &Settings) -> Result<Self, String> {
        let activate = enumerate(hardware, s.codec)
            .into_iter()
            .next()
            .ok_or("no such encoder on this machine")?;
        let name = friendly_name(&activate);
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

            // E_NOTIMPL means the streams are simply numbered from 0.
            let (mut inputs, mut outputs) = ([0u32], [0u32]);
            if transform.GetStreamIDs(&mut inputs, &mut outputs).is_err() {
                (inputs, outputs) = ([0], [0]);
            }
            let (input_id, output_id) = (inputs[0], outputs[0]);

            let (w, h) = (s.width as u32, s.height as u32);
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
                    "Media Foundation {} ({name}){}",
                    if hardware { "hardware" } else { "software" },
                    if rejected.is_empty() {
                        String::new()
                    } else {
                        format!(", ignores {}", rejected.join(", "))
                    }
                ),
            })
        }
    }

    fn sample(&self, time: Duration) -> windows::core::Result<IMFSample> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(self.nv12.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), data, self.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(self.nv12.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((time.as_nanos() / 100) as i64)?;
            sample.SetSampleDuration(self.frame_time)?;
            Ok(sample)
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
        picture: &Bgra<'_>,
        keyframe: bool,
        time: Duration,
    ) -> Result<Vec<Vec<u8>>, String> {
        convert::to_nv12(picture, &mut self.nv12);
        if keyframe {
            if let Some(api) = &self.codec_api {
                let _ = unsafe {
                    api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32))
                };
            }
        }
        let sample = self.sample(time).map_err(|e| e.to_string())?;
        match self.events.clone() {
            Some(events) => self.encode_async(&events, &sample),
            None => self.encode_sync(&sample),
        }
        .map_err(|e| e.to_string())
    }

    fn describe(&self) -> String {
        self.name.clone()
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

/// OpenH264, in software.
struct OpenH264Encoder {
    encoder: openh264::encoder::Encoder,
    i420: Vec<u8>,
}

unsafe impl Send for OpenH264Encoder {}

impl OpenH264Encoder {
    fn new(s: &Settings) -> Result<Self, String> {
        use openh264::encoder::{BitRate, EncoderConfig, FrameRate, IntraFramePeriod};
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(s.bitrate))
            .max_frame_rate(FrameRate::from_hz(s.fps as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(s.fps * 2));
        let encoder = openh264::encoder::Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        )
        .map_err(|e| e.to_string())?;
        Ok(OpenH264Encoder {
            encoder,
            i420: Vec::new(),
        })
    }
}

impl Encoder for OpenH264Encoder {
    fn encode(
        &mut self,
        picture: &Bgra<'_>,
        keyframe: bool,
        _time: Duration,
    ) -> Result<Vec<Vec<u8>>, String> {
        convert::to_i420(picture, &mut self.i420);
        let (w, h) = convert::even(picture.width, picture.height);
        let yuv = openh264::formats::YUVBuffer::from_vec(std::mem::take(&mut self.i420), w, h);
        if keyframe {
            self.encoder.force_intra_frame();
        }
        let out = self
            .encoder
            .encode(&yuv)
            .map_err(|e| e.to_string())?
            .to_vec();
        self.i420 = Vec::new();
        Ok(if out.is_empty() {
            Vec::new()
        } else {
            vec![out]
        })
    }

    fn describe(&self) -> String {
        "OpenH264 (software)".into()
    }
}
