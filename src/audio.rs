#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result, ensure};
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};
    use wasapi::{Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

    use crate::{Receiver, SAMPLE_RATE};

    fn selected_device(direction: Direction, selector: Option<&str>) -> Result<Device> {
        let enumerator = DeviceEnumerator::new()?;
        match selector {
            None => Ok(enumerator.get_default_device(direction)?),
            Some(s) => {
                let collection = enumerator.get_device_collection(direction)?;
                if let Ok(index) = s.parse::<u32>() {
                    Ok(collection.get_device_at_index(index)?)
                } else {
                    Ok(collection.get_device_with_name(s)?)
                }
            }
        }
    }

    pub fn list_devices() -> Result<()> {
        wasapi::initialize_mta().ok()?;
        let enumerator = DeviceEnumerator::new()?;
        for (label, direction) in [
            ("output / loopback", Direction::Render),
            ("input", Direction::Capture),
        ] {
            println!("{label} devices:");
            let collection = enumerator.get_device_collection(direction)?;
            for i in 0..collection.get_nbr_devices()? {
                let device = collection.get_device_at_index(i)?;
                println!("  {i}: {}", device.get_friendlyname()?);
            }
        }
        Ok(())
    }

    pub struct Player {
        client: wasapi::AudioClient,
        render: wasapi::AudioRenderClient,
        event: wasapi::Handle,
        queue: VecDeque<u8>,
    }

    impl Player {
        pub fn new(selector: Option<&str>) -> Result<Self> {
            wasapi::initialize_mta().ok()?;
            let device = selected_device(Direction::Render, selector)?;
            eprintln!("playing on {}", device.get_friendlyname()?);
            let mut client = device.get_iaudioclient()?;
            let format = WaveFormat::new(32, 32, SampleType::Float, SAMPLE_RATE as usize, 2, None);
            let (period, _) = client.get_device_period()?;
            client.initialize_client(
                &format,
                Direction::Render,
                StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: period,
                },
                None,
            )?;
            let event = client.set_get_eventhandle()?;
            let render = client.get_audiorenderclient()?;
            client.start_stream()?;
            Ok(Self {
                client,
                render,
                event,
                queue: VecDeque::new(),
            })
        }

        pub fn write(&mut self, samples: &[f32]) -> Result<()> {
            let mut pos = 0usize;
            while pos < samples.len() {
                let free = self.client.get_available_space_in_frames()? as usize;
                if free == 0 {
                    self.event.wait_for_event(2000)?;
                    continue;
                }
                let frames = free.min(samples.len() - pos);
                self.queue.clear();
                for &value in &samples[pos..pos + frames] {
                    let bytes = value.to_le_bytes();
                    self.queue.extend(bytes);
                    self.queue.extend(bytes);
                }
                self.render
                    .write_to_device_from_deque(frames, &mut self.queue, None)?;
                pos += frames;
            }
            Ok(())
        }

        pub fn finish(self) -> Result<()> {
            std::thread::sleep(Duration::from_millis(550));
            self.client.stop_stream()?;
            Ok(())
        }
    }

    pub fn play(samples: &[f32], selector: Option<&str>) -> Result<()> {
        let mut player = Player::new(selector)?;
        player.write(samples)?;
        player.finish()
    }

    pub fn capture(
        receiver: &mut Receiver,
        loopback: bool,
        selector: Option<&str>,
        max_seconds: u64,
        save_capture: Option<&std::path::Path>,
    ) -> Result<Option<(String, Vec<u8>)>> {
        wasapi::initialize_mta().ok()?;
        let direction = if loopback {
            Direction::Render
        } else {
            Direction::Capture
        };
        let device = selected_device(direction, selector)?;
        eprintln!(
            "recording {} from {}",
            if loopback { "loopback" } else { "input" },
            device.get_friendlyname()?
        );
        let mut client = device.get_iaudioclient()?;
        let format = WaveFormat::new(32, 32, SampleType::Float, SAMPLE_RATE as usize, 2, None);
        let (period, _) = client.get_device_period()?;
        client.initialize_client(
            &format,
            Direction::Capture,
            StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: period,
            },
            None,
        )?;
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        client.start_stream()?;
        let start = Instant::now();
        let mut last_report = Instant::now();
        let mut queue = VecDeque::new();
        let mut mono = Vec::with_capacity(4096);
        let mut wav = save_capture
            .map(|path| {
                hound::WavWriter::create(
                    path,
                    hound::WavSpec {
                        channels: 1,
                        sample_rate: SAMPLE_RATE,
                        bits_per_sample: 32,
                        sample_format: hound::SampleFormat::Float,
                    },
                )
            })
            .transpose()?;
        loop {
            capture.read_from_device_to_deque(&mut queue)?;
            while queue.len() >= 8 {
                let mut left = [0u8; 4];
                let mut right = [0u8; 4];
                for b in &mut left {
                    *b = queue.pop_front().context("capture underflow")?;
                }
                for b in &mut right {
                    *b = queue.pop_front().context("capture underflow")?;
                }
                let sample = f32::midpoint(f32::from_le_bytes(left), f32::from_le_bytes(right));
                if let Some(writer) = wav.as_mut() {
                    writer.write_sample(sample)?;
                }
                mono.push(sample);
                if mono.len() >= 1024 {
                    if let Some(done) = receiver.push_samples(&mono)? {
                        client.stop_stream()?;
                        if let Some(writer) = wav {
                            writer.finalize()?;
                        }
                        return Ok(Some(done));
                    }
                    mono.clear();
                }
            }
            if last_report.elapsed() >= Duration::from_secs(5) {
                eprintln!(
                    "heard {} modem frames; accepted {} packets; saved {} parts",
                    receiver.heard, receiver.accepted, receiver.parts_saved
                );
                last_report = Instant::now();
            }
            if max_seconds > 0 && start.elapsed() >= Duration::from_secs(max_seconds) {
                break;
            }
            match event.wait_for_event(2000) {
                Ok(()) | Err(wasapi::WasapiError::EventTimeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
        client.stop_stream()?;
        if let Some(writer) = wav {
            writer.finalize()?;
        }
        ensure!(queue.len() < 8, "unexpected capture tail");
        Ok(None)
    }
}

#[cfg(windows)]
pub use windows::{Player, capture, list_devices, play};

#[cfg(not(windows))]
pub fn list_devices() -> anyhow::Result<()> {
    anyhow::bail!("live audio requires Windows")
}
#[cfg(not(windows))]
pub fn play(_: &[f32], _: Option<&str>) -> anyhow::Result<()> {
    anyhow::bail!("live audio requires Windows")
}
#[cfg(not(windows))]
pub struct Player;
#[cfg(not(windows))]
impl Player {
    pub fn new(_: Option<&str>) -> anyhow::Result<Self> {
        anyhow::bail!("live audio requires Windows")
    }
    pub fn write(&mut self, _: &[f32]) -> anyhow::Result<()> {
        anyhow::bail!("live audio requires Windows")
    }
    pub fn finish(self) -> anyhow::Result<()> {
        anyhow::bail!("live audio requires Windows")
    }
}
#[cfg(not(windows))]
pub fn capture(
    _: &mut crate::Receiver,
    _: bool,
    _: Option<&str>,
    _: u64,
    _: Option<&std::path::Path>,
) -> anyhow::Result<Option<(String, Vec<u8>)>> {
    anyhow::bail!("live audio requires Windows")
}
