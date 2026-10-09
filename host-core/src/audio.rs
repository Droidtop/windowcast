//! Window audio on the host: what an agent captures (48 kHz stereo 16-bit
//! PCM) cut into 20 ms Opus packets for the window's audio track.

/// Samples a second, per channel.
pub const RATE: u32 = 48_000;
/// Channels, interleaved.
pub const CHANNELS: usize = 2;

/// An agent's capture of one window's sound.
pub trait AudioSource: Send {
    /// Waits up to about 100 ms for more sound: interleaved stereo 16-bit
    /// samples at 48 kHz, possibly none (the window is quiet). `None` ends
    /// the audio (the window went away).
    fn next_samples(&mut self) -> Option<Vec<i16>>;
}

/// Where a client's microphone plays on the host: a virtual microphone
/// that the host's applications record from.
pub trait MicrophoneSink: Send {
    /// Plays interleaved stereo 16-bit samples at 48 kHz.
    fn play(&mut self, samples: &[i16]);
}

/// PCM in, Opus packets out (20 ms each unless asked otherwise).
pub struct OpusPackets {
    encoder: opus::Encoder,
    pending: Vec<i16>,
    out: Vec<u8>,
    /// Samples (all channels) in one packet.
    packet_samples: usize,
}

impl OpusPackets {
    pub fn new() -> Result<Self, String> {
        Self::with_duration(20)
    }

    /// Packets of `milliseconds` (2.5, 5, 10, 20, 40 or 60 ms are Opus's
    /// sizes; GameStream clients ask for 5 or 10).
    pub fn with_duration(milliseconds: u32) -> Result<Self, String> {
        let mut encoder =
            opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::LowDelay)
                .map_err(|e| e.to_string())?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(128_000))
            .map_err(|e| e.to_string())?;
        Ok(OpusPackets {
            encoder,
            pending: Vec::new(),
            out: vec![0; 4000],
            packet_samples: (RATE / 1000 * milliseconds) as usize * CHANNELS,
        })
    }

    /// Every packet the same size from here on (no variable bitrate).
    pub fn constant_rate(&mut self) -> Result<(), String> {
        self.encoder.set_vbr(false).map_err(|e| e.to_string())
    }

    /// Adds samples and returns every whole packet they complete.
    pub fn push(&mut self, samples: &[i16]) -> Result<Vec<Vec<u8>>, String> {
        self.pending.extend_from_slice(samples);
        let mut packets = Vec::new();
        while self.pending.len() >= self.packet_samples {
            let length = self
                .encoder
                .encode(&self.pending[..self.packet_samples], &mut self.out)
                .map_err(|e| e.to_string())?;
            packets.push(self.out[..length].to_vec());
            self.pending.drain(..self.packet_samples);
        }
        Ok(packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twenty_milliseconds_make_a_packet_that_decodes() {
        let mut packets = OpusPackets::new().unwrap();
        // 30 ms of a 440 Hz tone: one packet, 10 ms left over.
        let samples: Vec<i16> = (0..1440)
            .flat_map(|i| {
                let v =
                    ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 8000.0) as i16;
                [v, v]
            })
            .collect();
        let out = packets.push(&samples).unwrap();
        assert_eq!(out.len(), 1);
        let mut decoder = opus::Decoder::new(RATE, opus::Channels::Stereo).unwrap();
        let mut pcm = vec![0i16; 960 * 2];
        assert_eq!(decoder.decode(&out[0], &mut pcm, false).unwrap(), 960);
    }
}
