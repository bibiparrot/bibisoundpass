use anyhow::{Result, ensure};
use rustfft::FftPlanner;
use rustfft::num_complex::Complex32;
use std::sync::Arc;

const FFT_LEN: usize = 256;
const CP_LEN: usize = 32;
const SYMBOL_LEN: usize = FFT_LEN + CP_LEN;
const GUARD_LEN: usize = 32;
const FIRST_BIN: usize = 6;
const CARRIERS: usize = 112;
const BITS_PER_CARRIER: usize = 4;
const BYTES_PER_SYMBOL: usize = CARRIERS * BITS_PER_CARRIER / 8;
const MAX_DATA_SYMBOLS: usize = 3;
const CAPACITY: usize = BYTES_PER_SYMBOL * MAX_DATA_SYMBOLS;
const ECC_BYTES: usize = 32;
const SCALE: f32 = 0.9;
const SYNC_THRESHOLD: f32 = 0.40;

pub(crate) struct Encoder {
    inverse: Arc<dyn rustfft::Fft<f32>>,
    preamble: Vec<f32>,
}

impl Encoder {
    pub(crate) fn new() -> Self {
        let mut planner = FftPlanner::new();
        let inverse = planner.plan_fft_inverse(FFT_LEN);
        let preamble = make_preamble(&inverse);
        Self { inverse, preamble }
    }

    pub(crate) fn encode(&self, bytes: &[u8]) -> Result<Vec<f32>> {
        ensure!(
            bytes.len() + ECC_BYTES <= CAPACITY,
            "OFDM frame exceeds {CAPACITY} bytes"
        );
        let protected = reed_solomon::Encoder::new(ECC_BYTES).encode(bytes);
        let data_symbols = protected.len().div_ceil(BYTES_PER_SYMBOL);
        let mut result = Vec::with_capacity(SYMBOL_LEN * (1 + data_symbols) + GUARD_LEN);
        result.extend_from_slice(&self.preamble);
        for symbol in 0..data_symbols {
            let mut frequency = vec![Complex32::ZERO; FFT_LEN];
            for carrier in 0..CARRIERS {
                let bit = symbol * BYTES_PER_SYMBOL * 8 + carrier * BITS_PER_CARRIER;
                let i = bits(&protected, bit, 2);
                let q = bits(&protected, bit + 2, 2);
                let value = Complex32::new(level(i), level(q)) / 10.0_f32.sqrt();
                set_real_carrier(&mut frequency, FIRST_BIN + carrier, value);
            }
            result.extend_from_slice(&to_time_symbol(frequency, &self.inverse));
        }
        result.resize(result.len() + GUARD_LEN, 0.0);
        Ok(result)
    }
}

pub(crate) struct Decoder {
    forward: Arc<dyn rustfft::Fft<f32>>,
    preamble: Vec<f32>,
    known: Vec<Complex32>,
    buffer: Vec<f32>,
    frame_bytes: usize,
    data_symbols: usize,
}

impl Decoder {
    pub(crate) fn new(frame_bytes: usize) -> Result<Self> {
        ensure!(
            frame_bytes + ECC_BYTES <= CAPACITY,
            "OFDM frame exceeds {CAPACITY} bytes"
        );
        let mut planner = FftPlanner::new();
        let inverse = planner.plan_fft_inverse(FFT_LEN);
        let forward = planner.plan_fft_forward(FFT_LEN);
        let known = preamble_carriers();
        let preamble = make_preamble(&inverse);
        Ok(Self {
            forward,
            preamble,
            known,
            buffer: Vec::new(),
            frame_bytes,
            data_symbols: (frame_bytes + ECC_BYTES).div_ceil(BYTES_PER_SYMBOL),
        })
    }

    pub(crate) fn push(&mut self, samples: &[f32]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(samples);
        let mut frames = Vec::new();
        let frame_samples = SYMBOL_LEN * (1 + self.data_symbols) + GUARD_LEN;
        loop {
            if self.buffer.len() < self.preamble.len() {
                break;
            }
            let Some(start) = find_preamble(&self.buffer, &self.preamble) else {
                let keep = self.preamble.len().saturating_sub(1);
                if self.buffer.len() > keep {
                    self.buffer.drain(..self.buffer.len() - keep);
                }
                break;
            };
            if self.buffer.len() < start + frame_samples {
                if start > 0 {
                    self.buffer.drain(..start);
                }
                break;
            }
            if let Some(frame) = self.decode_at(start) {
                frames.push(frame);
            }
            self.buffer.drain(..start + frame_samples);
        }
        frames
    }

    fn decode_at(&self, start: usize) -> Option<Vec<u8>> {
        let preamble_start = start + CP_LEN;
        let mut preamble: Vec<Complex32> = self.buffer[preamble_start..preamble_start + FFT_LEN]
            .iter()
            .map(|&x| Complex32::new(x, 0.0))
            .collect();
        self.forward.process(&mut preamble);
        let mut result = vec![0u8; self.frame_bytes + ECC_BYTES];
        for symbol in 0..self.data_symbols {
            let data_start = start + SYMBOL_LEN * (1 + symbol) + CP_LEN;
            let mut data: Vec<Complex32> = self.buffer[data_start..data_start + FFT_LEN]
                .iter()
                .map(|&x| Complex32::new(x, 0.0))
                .collect();
            self.forward.process(&mut data);
            for carrier in 0..CARRIERS {
                let bin = FIRST_BIN + carrier;
                if preamble[bin].norm_sqr() < 1e-8 {
                    return None;
                }
                let equalized = data[bin] * self.known[carrier] / preamble[bin];
                let i = nearest_level(equalized.re * 10.0_f32.sqrt());
                let q = nearest_level(equalized.im * 10.0_f32.sqrt());
                let bit = symbol * BYTES_PER_SYMBOL * 8 + carrier * BITS_PER_CARRIER;
                put_bits(&mut result, bit, 2, i);
                put_bits(&mut result, bit + 2, 2, q);
            }
        }
        let corrected = reed_solomon::Decoder::new(ECC_BYTES)
            .correct(&mut result, None)
            .ok()?;
        Some(corrected.data().to_vec())
    }
}

fn make_preamble(inverse: &Arc<dyn rustfft::Fft<f32>>) -> Vec<f32> {
    let mut frequency = vec![Complex32::ZERO; FFT_LEN];
    for (carrier, &value) in preamble_carriers().iter().enumerate() {
        set_real_carrier(&mut frequency, FIRST_BIN + carrier, value);
    }
    to_time_symbol(frequency, inverse)
}

fn preamble_carriers() -> Vec<Complex32> {
    let mut state = 0x6d2b_79f5u32;
    (0..CARRIERS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            Complex32::new(if state & 1 == 0 { -1.0 } else { 1.0 }, 0.0)
        })
        .collect()
}

fn set_real_carrier(frequency: &mut [Complex32], bin: usize, value: Complex32) {
    frequency[bin] = value;
    frequency[FFT_LEN - bin] = value.conj();
}

fn to_time_symbol(mut frequency: Vec<Complex32>, inverse: &Arc<dyn rustfft::Fft<f32>>) -> Vec<f32> {
    inverse.process(&mut frequency);
    let body: Vec<f32> = frequency
        .iter()
        .map(|value| value.re * SCALE / FFT_LEN as f32)
        .collect();
    let mut symbol = Vec::with_capacity(SYMBOL_LEN);
    symbol.extend_from_slice(&body[FFT_LEN - CP_LEN..]);
    symbol.extend_from_slice(&body);
    symbol
}

fn find_preamble(samples: &[f32], preamble: &[f32]) -> Option<usize> {
    let reference_energy: f32 = preamble.iter().map(|x| x * x).sum();
    let last = samples.len().saturating_sub(preamble.len());
    let mut best = (f32::NEG_INFINITY, None);
    let mut search_until = None;
    for start in 0..=last {
        let window = &samples[start..start + preamble.len()];
        let energy: f32 = window.iter().map(|x| x * x).sum();
        if energy < 1e-8 {
            continue;
        }
        let dot: f32 = window.iter().zip(preamble).map(|(a, b)| a * b).sum();
        let correlation = dot / (energy * reference_energy).sqrt();
        if correlation > best.0 {
            best = (correlation, Some(start));
        }
        if correlation >= SYNC_THRESHOLD && search_until.is_none() {
            search_until = Some((start + CP_LEN * 2).min(last));
        }
        if search_until.is_some_and(|end| start >= end) {
            break;
        }
    }
    if std::env::var_os("SOUNDPASS_DSP_DEBUG").is_some() && best.0 > 0.1 {
        eprintln!("OFDM best preamble correlation: {:.3}", best.0);
    }
    (best.0 >= SYNC_THRESHOLD).then_some(best.1).flatten()
}

fn level(value: u8) -> f32 {
    f32::from(value) * 2.0 - 3.0
}

fn nearest_level(value: f32) -> u8 {
    (((value + 3.0) * 0.5).round() as i32).clamp(0, 3) as u8
}

fn bits(bytes: &[u8], offset: usize, count: usize) -> u8 {
    let mut value = 0;
    for bit in 0..count {
        let index = offset + bit;
        let set = bytes
            .get(index / 8)
            .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0);
        value = (value << 1) | u8::from(set);
    }
    value
}

fn put_bits(bytes: &mut [u8], offset: usize, count: usize, value: u8) {
    for bit in 0..count {
        let index = offset + bit;
        if index / 8 >= bytes.len() {
            return;
        }
        let source = (value >> (count - 1 - bit)) & 1;
        bytes[index / 8] |= source << (7 - index % 8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_pcm_round_trip() -> Result<()> {
        let bytes: Vec<u8> = (0..105).map(|value| value as u8).collect();
        let audio = Encoder::new().encode(&bytes)?;
        let mut decoder = Decoder::new(bytes.len())?;
        let frames = decoder.push(&audio);
        assert_eq!(frames, vec![bytes]);
        Ok(())
    }
}
