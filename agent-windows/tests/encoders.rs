//! Every encoder this machine has, without capture or a window: a moving
//! synthetic picture through each, checking keyframes on request and, for
//! H.264, that the output decodes to the right picture. Headless, so it
//! can run on any Windows machine, including ones with real NVENC, AMF or
//! Quick Sync encoders behind Media Foundation.
#![cfg(windows)]

use std::time::Duration;

use windowcast_agent_windows::convert::{Bgra, Picture};
use windowcast_agent_windows::encoder::{self, EncoderChoice, Settings};
use windowcast_cli_tools::H264Check;
use windowcast_protocol::VideoCodec;

const W: usize = 640;
const H: usize = 360;

/// Mid grey with a white bar that moves 8 pixels per frame.
fn picture(frame: usize) -> Vec<u8> {
    let mut data = vec![128u8; W * H * 4];
    let bar = (frame * 8) % W;
    for row in 0..H {
        for x in bar..(bar + 32).min(W) {
            data[(row * W + x) * 4..(row * W + x) * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    data
}

fn has_keyframe(codec: VideoCodec, data: &[u8]) -> bool {
    data.windows(4)
        .filter(|w| w[0] == 0 && w[1] == 0 && w[2] == 1)
        .any(|w| match codec {
            VideoCodec::H264 => w[3] & 0x1f == 5,
            _ => (16..=21).contains(&((w[3] >> 1) & 0x3f)),
        })
}

/// NAL unit types and sizes, and the H.264 SPS profile, for diagnostics.
fn describe(codec: VideoCodec, data: &[u8]) -> String {
    let starts: Vec<usize> = data
        .windows(3)
        .enumerate()
        .filter(|(_, w)| w[0] == 0 && w[1] == 0 && w[2] == 1)
        .map(|(i, _)| i + 3)
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
            let header = data.get(start).copied().unwrap_or(0);
            let kind = match codec {
                VideoCodec::H264 => header & 0x1f,
                _ => (header >> 1) & 0x3f,
            };
            let profile = if codec == VideoCodec::H264 && kind == 7 {
                format!(" profile {}", data.get(start + 1).copied().unwrap_or(0))
            } else {
                String::new()
            };
            format!("{kind}({}b{profile})", end.saturating_sub(start))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn every_encoder_on_this_machine_encodes_and_obeys_keyframe_requests() {
    for line in encoder::list() {
        println!("{line}");
    }
    let mut runs = vec![
        (EncoderChoice::OpenH264, VideoCodec::H264),
        (EncoderChoice::MfSoftware, VideoCodec::H264),
    ];
    for codec in encoder::available_codecs(EncoderChoice::MfHardware) {
        runs.push((EncoderChoice::MfHardware, codec));
    }

    for (choice, codec) in runs {
        let settings = Settings {
            codec,
            width: W,
            height: H,
            fps: 30,
            bitrate: 2_000_000,
        };
        let mut encoder = encoder::open(choice, &settings)
            .unwrap_or_else(|e| panic!("{choice:?} {codec:?}: {e}"));
        let mut check = H264Check::new().unwrap();
        let (mut outputs, mut keyframes, mut bytes) = (0, 0, 0);
        let (mut units, mut openh264_refused) = (Vec::new(), Vec::new());
        for frame in 0..60 {
            let data = picture(frame);
            let bgra = Picture::Bgra(Bgra {
                data: &data,
                width: W,
                height: H,
                stride: W * 4,
            });
            // A keyframe first, and one asked for at frame 30.
            let want_key = frame == 0 || frame == 30;
            let time = Duration::from_millis(33 * frame as u64);
            for out in encoder.encode(&bgra, want_key, time).unwrap() {
                outputs += 1;
                bytes += out.len();
                if has_keyframe(codec, &out) {
                    keyframes += 1;
                }
                if frame == 0 || (29..=31).contains(&frame) {
                    println!(
                        "  {choice:?} {codec:?} frame {frame}: {}",
                        describe(codec, &out)
                    );
                }
                if codec == VideoCodec::H264 {
                    if check.decode(&out).is_err() {
                        openh264_refused.push(frame);
                    }
                    units.push(out);
                }
            }
        }
        println!(
            "{choice:?} {codec:?} ({}): {outputs} outputs, {keyframes} keyframes, {} KiB; decoded {} at {:?}",
            encoder.describe(),
            bytes / 1024,
            check.pictures,
            check.dimensions
        );
        assert!(
            outputs >= 50,
            "{choice:?} {codec:?}: only {outputs} outputs for 60 pictures"
        );
        assert!(
            keyframes >= 2,
            "{choice:?} {codec:?}: the keyframe request was ignored"
        );
        if codec == VideoCodec::H264 {
            // OpenH264's decoder is strict: it refused NVIDIA's H.264 MFT
            // output from the requested keyframe on (slice QP 2, RTX 3060
            // Ti), which Windows' own decoder takes whole. One of the two
            // must take every frame; OpenH264's verdict is printed either way.
            let (mf_pictures, mf_refused) = mf_decode(&units).unwrap();
            println!(
                "  OpenH264 decoded {} (refused frames {openh264_refused:?}); Windows' decoder decoded {mf_pictures} (refused {mf_refused:?})",
                check.pictures
            );
            let openh264_ok = openh264_refused.is_empty() && check.pictures >= 50;
            let mf_ok = mf_refused.is_empty() && mf_pictures >= 50;
            assert!(
                openh264_ok || mf_ok,
                "{choice:?}: neither decoder took the stream"
            );
            if openh264_ok {
                assert_eq!(check.dimensions, Some((W, H)));
            }
        }
    }
}

/// A grainy moving picture, busy enough that any encoder's rate binds.
fn busy(frame: usize, seed: &mut u32) -> Vec<u8> {
    let mut data = vec![0u8; W * H * 4];
    for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let v = ((i % W + i / W + frame * 5) as u8) ^ ((*seed >> 26) as u8);
        px.copy_from_slice(&[v, v.wrapping_add(40), v.wrapping_mul(3), 255]);
    }
    data
}

/// Adaptive quality changes a running encoder's bitrate: each encoder
/// either takes a lower one mid-stream and makes smaller frames, or says
/// it cannot (the agent then opens a new encoder).
#[test]
fn encoders_take_a_lower_bitrate_mid_stream() {
    let mut runs = vec![(EncoderChoice::OpenH264, VideoCodec::H264)];
    for codec in encoder::available_codecs(EncoderChoice::MfHardware) {
        runs.push((EncoderChoice::MfHardware, codec));
    }
    for (choice, codec) in runs {
        let settings = Settings {
            codec,
            width: W,
            height: H,
            fps: 30,
            bitrate: 4_000_000,
        };
        let mut encoder = encoder::open(choice, &settings)
            .unwrap_or_else(|e| panic!("{choice:?} {codec:?}: {e}"));
        let mut seed = 1;
        let mut run = |encoder: &mut Box<dyn encoder::Encoder>, from: usize, frames: usize| {
            let mut bytes = 0;
            for frame in from..from + frames {
                let data = busy(frame, &mut seed);
                let bgra = Picture::Bgra(Bgra {
                    data: &data,
                    width: W,
                    height: H,
                    stride: W * 4,
                });
                let time = Duration::from_millis(33 * frame as u64);
                for out in encoder.encode(&bgra, frame == 0, time).unwrap() {
                    bytes += out.len();
                }
            }
            bytes
        };
        let high = run(&mut encoder, 0, 60);
        let taken = encoder.set_bitrate(500_000);
        run(&mut encoder, 60, 15);
        let low = run(&mut encoder, 75, 60);
        println!(
            "{choice:?} {codec:?} ({}): 60 frames {} KiB at 4 Mbit/s; lower rate {}; then {} KiB",
            encoder.describe(),
            high / 1024,
            if taken { "taken" } else { "refused" },
            low / 1024
        );
        if taken {
            assert!(
                low * 2 < high,
                "{choice:?} {codec:?} took the lower rate but kept its frame sizes"
            );
        }
    }
}

/// Windows' own H.264 decoder (Media Foundation), as a second opinion on
/// streams OpenH264's strict decoder rejects. Returns pictures decoded and
/// the frames it refused.
fn mf_decode(units: &[Vec<u8>]) -> windows::core::Result<(usize, Vec<usize>)> {
    use std::mem::ManuallyDrop;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
        let decoder: IMFTransform =
            CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)?;
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        decoder.SetInputType(0, &input, 0)?;
        let set_nv12 = |decoder: &IMFTransform| -> windows::core::Result<()> {
            let mut i = 0;
            loop {
                let available = decoder.GetOutputAvailableType(0, i)?;
                if available.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    return decoder.SetOutputType(0, &available, 0);
                }
                i += 1;
            }
        };
        set_nv12(&decoder)?;
        decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;

        let pull = |decoder: &IMFTransform,
                    refused: &mut Vec<usize>,
                    n: usize|
         -> windows::core::Result<usize> {
            let mut pictures = 0;
            loop {
                let size = decoder.GetOutputStreamInfo(0)?.cbSize.max(1 << 20);
                let out = MFCreateSample()?;
                out.AddBuffer(&MFCreateMemoryBuffer(size)?)?;
                let mut output = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(out)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = decoder.ProcessOutput(0, &mut output, &mut status);
                drop(ManuallyDrop::take(&mut output[0].pSample));
                drop(ManuallyDrop::take(&mut output[0].pEvents));
                match result {
                    Ok(()) => pictures += 1,
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => set_nv12(decoder)?,
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(pictures),
                    Err(_) => {
                        refused.push(n);
                        return Ok(pictures);
                    }
                }
            }
        };
        let (mut pictures, mut refused) = (0usize, Vec::new());
        for (n, unit) in units.iter().enumerate() {
            let buffer = MFCreateMemoryBuffer(unit.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(unit.as_ptr(), data, unit.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(unit.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(n as i64 * 333_333)?;
            if decoder.ProcessInput(0, &sample, 0).is_err() {
                refused.push(n);
                continue;
            }
            pictures += pull(&decoder, &mut refused, n)?;
        }
        // The decoder holds a few pictures back; draining gives them up.
        decoder.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        pictures += pull(&decoder, &mut refused, units.len())?;
        Ok((pictures, refused))
    }
}
