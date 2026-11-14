use anyhow::{Result, ensure};
use clap::Parser;
use soundpass_rs::{Profile, SAMPLE_RATE, Transmission, audio, record};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Compress a file and transmit it through Windows audio")]
struct Args {
    file: Option<PathBuf>,
    #[arg(long)]
    wav: Option<PathBuf>,
    #[arg(long)]
    no_play: bool,
    #[arg(long)]
    devices: bool,
    #[arg(long)]
    device: Option<String>,
    #[arg(long, value_enum, default_value = "loopback")]
    profile: Profile,
    #[arg(long, default_value_t = 40)]
    repair_percent: u32,
    #[arg(long, default_value_t = 2)]
    passes: u32,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.devices {
        return audio::list_devices();
    }
    let file = args
        .file
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("provide a file"))?;
    ensure!(
        !args.no_play || args.wav.is_some(),
        "--no-play requires --wav"
    );
    let tx = Transmission::prepare(file, args.repair_percent)?;
    eprintln!(
        "compressed envelope: {} bytes; {} parts; {} modem frames",
        tx.envelope_len,
        tx.part_count,
        tx.frames.len()
    );
    if let Some(path) = &tx.container_path {
        eprintln!("container: {}", path.display());
    }
    if let Some(path) = &tx.parts_dir {
        eprintln!("parts: {}", path.display());
    }
    if let Some(path) = &tx.log_path {
        eprintln!("log: {}", path.display());
    }
    let mut wav = if let Some(path) = &args.wav {
        Some(hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: SAMPLE_RATE,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?)
    } else {
        None
    };
    let mut player = if args.no_play {
        None
    } else {
        Some(audio::Player::new(args.device.as_deref())?)
    };
    let count = tx.stream_audio(args.profile, args.passes, |chunk| {
        if let Some(w) = wav.as_mut() {
            for &sample in chunk {
                w.write_sample((sample.clamp(-1.0, 1.0) * 32767.0) as i16)?;
            }
        }
        if let Some(p) = player.as_mut() {
            p.write(chunk)?;
        }
        Ok(())
    })?;
    if let Some(w) = wav {
        w.finalize()?;
        if let (Some(log), Some(path)) = (&tx.log_path, &args.wav) {
            record(
                log,
                serde_json::json!({"stage":"wav_saved", "path":path.display().to_string()}),
            )?;
        }
    }
    if let Some(p) = player {
        p.finish()?;
        if let Some(log) = &tx.log_path {
            record(log, serde_json::json!({"stage":"playback_finished"}))?;
        }
    }
    eprintln!(
        "audio duration: {:.1} seconds; compressed payload rate: {:.1} kbps",
        count as f64 / SAMPLE_RATE as f64,
        tx.envelope_len as f64 * 8.0 * f64::from(SAMPLE_RATE) / count as f64 / 1000.0
    );
    if let Some(path) = &args.wav {
        println!("WAV: {}", path.display());
    }
    Ok(())
}
