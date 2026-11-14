use anyhow::Result;
use soundpass_rs::{Profile, Receiver, SAMPLE_RATE, Transmission};
use std::fs;

#[test]
fn loopback_ofdm_round_trip_exceeds_twenty_kbps() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("ofdm-speed.bin");
    let mut state = 0x91e1_0da5u32;
    let data: Vec<u8> = (0..32 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    fs::write(&source, &data)?;
    let tx = Transmission::from_file(&source, 0)?;
    let mut samples = Vec::new();
    tx.stream_audio(Profile::Loopback, 1, |chunk| {
        samples.extend_from_slice(chunk);
        Ok(())
    })?;

    let duration = samples.len() as f64 / f64::from(SAMPLE_RATE);
    let peak = samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    let payload_bps = data.len() as f64 * 8.0 / duration;
    eprintln!("OFDM duration={duration:.3}s payload={payload_bps:.0}bps peak={peak:.3}");
    assert!(
        payload_bps >= 20_000.0,
        "measured {payload_bps:.0} bps over {duration:.2}s"
    );

    for sample in &mut samples {
        *sample = f32::from((*sample * 32767.0) as i16) / 32768.0;
    }
    let original = samples.clone();
    for index in 1..samples.len() - 1 {
        samples[index] =
            original[index - 1] * 0.15 + original[index] * 0.70 + original[index + 1] * 0.15;
    }

    let mut rx = Receiver::new(Profile::Loopback)?;
    let mut recovered = None;
    for block in samples.chunks(1024) {
        if let Some(file) = rx.push_samples(block)? {
            recovered = Some(file);
            break;
        }
    }
    let (name, bytes) = recovered.expect("OFDM receiver did not recover the file");
    assert_eq!(name, "ofdm-speed.bin");
    assert_eq!(bytes, data);
    Ok(())
}

#[test]
fn loopback_ofdm_recovers_from_periodic_audio_dropouts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("dropouts.bin");
    let mut state = 0x33d0_41b7u32;
    let data: Vec<u8> = (0..5000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    fs::write(&source, &data)?;
    let tx = Transmission::from_file(&source, 50)?;
    let mut samples = Vec::new();
    tx.stream_audio(Profile::Loopback, 1, |chunk| {
        samples.extend_from_slice(chunk);
        Ok(())
    })?;

    let dropout = SAMPLE_RATE as usize * 30 / 1000;
    let interval = SAMPLE_RATE as usize * 152 / 1000;
    let mut start = SAMPLE_RATE as usize / 2 + interval;
    while start < samples.len().saturating_sub(SAMPLE_RATE as usize / 2) {
        let end = (start + dropout).min(samples.len());
        samples[start..end].fill(0.0);
        start += interval;
    }

    let mut rx = Receiver::new(Profile::Loopback)?;
    let mut recovered = None;
    for block in samples.chunks(733) {
        if let Some(file) = rx.push_samples(block)? {
            recovered = Some(file);
            break;
        }
    }
    let (_, bytes) = recovered.expect("OFDM/RaptorQ did not recover after periodic dropouts");
    assert_eq!(bytes, data);
    Ok(())
}
