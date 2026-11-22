//! Opus at 48 kHz mono, 20 ms per packet, tuned for speech: about 32 kbit/s, with in-band
//! error correction (each packet carries a rough copy of the one before).

use opus1::{
    Application, Bitrate, Channels, InbandFec, OpusDecoder, OpusEncoder, SampleRate, Signal,
};

use crate::FRAME;

const BITRATE: u32 = 32_000;
/// The loss the encoder plans its error correction for.
const EXPECTED_LOSS: u8 = 10;
/// The largest packet Opus makes for 20 ms.
const MAX_PACKET: usize = 1276;

pub struct Encoder(OpusEncoder);

impl Encoder {
    pub fn new() -> anyhow::Result<Self> {
        let mut encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .map_err(|e| anyhow::anyhow!("opus encoder: {e}"))?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(
            std::num::NonZeroU32::new(BITRATE).expect("not zero"),
        ));
        encoder.set_inband_fec(InbandFec::On);
        encoder
            .set_packet_loss_perc(EXPECTED_LOSS)
            .map_err(|e| anyhow::anyhow!("opus encoder: {e}"))?;
        encoder.set_signal(Signal::Voice);
        Ok(Self(encoder))
    }

    /// Encodes [`FRAME`] samples into one packet.
    pub fn encode(&mut self, samples: &[f32]) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(samples.len() == FRAME, "a frame is {FRAME} samples");
        let mut packet = vec![0u8; MAX_PACKET];
        let len = self
            .0
            .encode(samples, FRAME, &mut packet)
            .map_err(|e| anyhow::anyhow!("opus encode: {e}"))?;
        packet.truncate(len);
        Ok(packet)
    }
}

pub struct Decoder(OpusDecoder);

impl Decoder {
    pub fn new() -> anyhow::Result<Self> {
        OpusDecoder::new(SampleRate::Hz48000, Channels::Mono)
            .map(Self)
            .map_err(|e| anyhow::anyhow!("opus decoder: {e}"))
    }

    /// 20 ms from a packet.
    pub fn decode(&mut self, packet: &[u8]) -> Vec<f32> {
        self.run(|d, out| d.decode(packet, FRAME, out))
    }

    /// 20 ms for a lost packet: rebuilt from the next packet's correction data when given,
    /// else concealed from what came before.
    pub fn lost(&mut self, next: Option<&[u8]>) -> Vec<f32> {
        match next {
            Some(next) => self.run(|d, out| d.decode_fec(next, FRAME, out)),
            None => self.run(|d, out| d.decode(&[], FRAME, out)),
        }
    }

    fn run(
        &mut self,
        f: impl FnOnce(&mut OpusDecoder, &mut [f32]) -> Result<usize, opus1::Error>,
    ) -> Vec<f32> {
        let mut out = vec![0.0f32; FRAME];
        match f(&mut self.0, &mut out) {
            Ok(n) => out.truncate(n.min(FRAME)),
            Err(e) => {
                tracing::debug!("opus decode: {e}");
                out.iter_mut().for_each(|s| *s = 0.0);
            }
        }
        out.resize(FRAME, 0.0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frame: usize) -> Vec<f32> {
        (0..FRAME)
            .map(|i| {
                let t = (frame * FRAME + i) as f32 / crate::RATE as f32;
                0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect()
    }

    fn energy(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn speech_sized_packets_decode_back_and_losses_are_filled() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let packets: Vec<Vec<u8>> = (0..50).map(|i| encoder.encode(&tone(i)).unwrap()).collect();
        // About 32 kbit/s: 80 bytes per 20 ms, give or take.
        let average = packets.iter().map(Vec::len).sum::<usize>() / packets.len();
        assert!((30..200).contains(&average), "{average} bytes per packet");
        let mut last = Vec::new();
        for (i, packet) in packets.iter().enumerate() {
            last = match i {
                // Lose 20, rebuild it from 21's correction data; lose 30 outright.
                20 => decoder.lost(Some(&packets[21])),
                30 => decoder.lost(None),
                _ => decoder.decode(packet),
            };
            assert_eq!(last.len(), FRAME);
            // Right after a loss Opus fades back in; elsewhere the tone is there in full.
            let near_loss = (20..=23).contains(&i) || (30..=33).contains(&i);
            if i > 5 && !near_loss {
                assert!(energy(&last) > 0.1, "packet {i} came out silent");
            }
        }
        assert!(energy(&last) > 0.1);
        assert!(encoder.encode(&[0.0; 10]).is_err());
    }
}
