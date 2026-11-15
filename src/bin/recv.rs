use anyhow::{Result, bail, ensure};
use clap::Parser;
use soundpass_rs::{Profile, Receiver, SAMPLE_RATE, audio, record, save_file};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Record live audio or decode a WAV and recover a transmitted file")]
struct Args {
    #[arg(long)]
    input: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    out: PathBuf,
    #[arg(long)]
    overwrite: bool,
    #[arg(long)]
    devices: bool,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    loopback: bool,
    #[arg(long)]
    device: Option<String>,
    #[arg(long, value_enum, default_value = "loopback")]
    profile: Profile,
    #[arg(long, default_value_t = 0)]
    max_seconds: u64,
    /// Save the captured mono 48 kHz samples as a float WAV for diagnostics.
    #[arg(long)]
    save_capture: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.devices {
        return audio::list_devices();
    }
    let mut receiver = Receiver::with_output(args.profile, &args.out)?;
    let recovered = if let Some(path) = args.input {
        decode_wav(&path, &mut receiver)?
    } else {
        audio::capture(
            &mut receiver,
            args.loopback,
            args.device.as_deref(),
            args.max_seconds,
            args.save_capture.as_deref(),
        )?
    };
    let Some((name, bytes)) = recovered else {
        bail!(
            "no complete file received (heard {} frames, accepted {} packets)",
            receiver.heard,
            receiver.accepted
        );
    };
    let path = save_file(&args.out, &name, &bytes, args.overwrite)?;
    if let Some(log) = &receiver.completed_log {
        record(
            log,
            serde_json::json!({"stage":"file_saved", "path":path.display().to_string(), "bytes":bytes.len(), "hash":blake3::hash(&bytes).to_hex().to_string()}),
        )?;
    }
    println!(
        "saved {} ({} bytes, hash verified)",
        path.display(),
        bytes.len()
    );
    Ok(())
}

fn decode_wav(
    path: &std::path::Path,
    receiver: &mut Receiver,
) -> Result<Option<(String, Vec<u8>)>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    ensure!(
        spec.sample_rate == SAMPLE_RATE,
        "WAV sample rate must be 48000 Hz"
    );
    ensure!(
        spec.channels > 0 && spec.channels <= 8,
        "unsupported WAV channel count"
    );
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => feed_samples(
            reader
                .samples::<i16>()
                .map(|s| s.map(|v| v as f32 / 32768.0)),
            spec.channels as usize,
            receiver,
        ),
        (hound::SampleFormat::Float, 32) => {
            feed_samples(reader.samples::<f32>(), spec.channels as usize, receiver)
        }
        _ => bail!("WAV must be 16-bit PCM or 32-bit float"),
    }
}

fn feed_samples(
    samples: impl Iterator<Item = std::result::Result<f32, hound::Error>>,
    channels: usize,
    receiver: &mut Receiver,
) -> Result<Option<(String, Vec<u8>)>> {
    let mut block = Vec::with_capacity(1024);
    let mut sum = 0.0;
    let mut count = 0;
    for sample in samples {
        sum += sample?;
        count += 1;
        if count == channels {
            block.push(sum / channels as f32);
            sum = 0.0;
            count = 0;
            if block.len() == 1024 {
                if let Some(done) = receiver.push_samples(&block)? {
                    return Ok(Some(done));
                }
                block.clear();
            }
        }
    }
    if !block.is_empty() {
        return receiver.push_samples(&block);
    }
    Ok(None)
}
