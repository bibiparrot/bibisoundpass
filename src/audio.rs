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

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context as _, Result, bail, ensure};
    use libpulse_binding as pulse;
    use libpulse_simple_binding::Simple;
    use pulse::callbacks::ListResult;
    use pulse::context::{Context as PulseContext, FlagSet, State};
    use pulse::mainloop::standard::{IterateResult, Mainloop};
    use pulse::sample::{Format, Spec};
    use pulse::stream::Direction;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use crate::{Receiver, SAMPLE_RATE};

    #[derive(Clone, Debug)]
    struct DeviceInfo {
        index: u32,
        name: String,
        description: String,
        monitor_source_name: Option<String>,
    }

    #[derive(Default)]
    struct DeviceCatalog {
        default_sink_name: Option<String>,
        default_source_name: Option<String>,
        sinks: Vec<DeviceInfo>,
        sources: Vec<DeviceInfo>,
    }

    impl DeviceCatalog {
        fn select<'a>(
            devices: &'a [DeviceInfo],
            selector: Option<&str>,
            default_name: Option<&str>,
            kind: &str,
        ) -> Result<&'a DeviceInfo> {
            let selected = selector
                .or(default_name)
                .with_context(|| format!("PulseAudio did not report a default {kind} device"))?;
            let index = selected.parse::<u32>().ok();
            devices
                .iter()
                .find(|device| {
                    index == Some(device.index)
                        || device.name == selected
                        || device.description == selected
                })
                .with_context(|| format!("{kind} device not found: {selected}"))
        }

        fn playback_sink(&self, selector: Option<&str>) -> Result<String> {
            Ok(Self::select(
                &self.sinks,
                selector,
                self.default_sink_name.as_deref(),
                "output",
            )?
            .name
            .clone())
        }

        fn capture_source(&self, loopback: bool, selector: Option<&str>) -> Result<String> {
            if loopback {
                let sink = Self::select(
                    &self.sinks,
                    selector,
                    self.default_sink_name.as_deref(),
                    "output / loopback",
                )?;
                sink.monitor_source_name
                    .clone()
                    .with_context(|| format!("output device has no monitor source: {}", sink.name))
            } else {
                Ok(Self::select(
                    &self.sources,
                    selector,
                    self.default_source_name.as_deref(),
                    "input",
                )?
                .name
                .clone())
            }
        }
    }

    fn iterate(mainloop: &mut Mainloop) -> Result<()> {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => Ok(()),
            IterateResult::Quit(value) => bail!("PulseAudio main loop quit: {value:?}"),
            IterateResult::Err(error) => Err(error).context("PulseAudio main loop failed"),
        }
    }

    fn query_devices() -> Result<DeviceCatalog> {
        let mut mainloop = Mainloop::new().context("create PulseAudio main loop")?;
        let mut context =
            PulseContext::new(&mainloop, "SoundPass").context("create PulseAudio context")?;
        context
            .connect(None, FlagSet::NOFLAGS, None)
            .context("connect to PulseAudio/PipeWire audio service")?;
        loop {
            match context.get_state() {
                State::Ready => break,
                State::Failed | State::Terminated => {
                    return Err(context.errno()).context(
                        "connect to PulseAudio/PipeWire audio service; ensure pipewire-pulse or PulseAudio is running",
                    );
                }
                _ => iterate(&mut mainloop)?,
            }
        }

        let default_sink_name = Rc::new(RefCell::new(None));
        let default_source_name = Rc::new(RefCell::new(None));
        let server_done = Rc::new(Cell::new(false));
        {
            let sink = Rc::clone(&default_sink_name);
            let source = Rc::clone(&default_source_name);
            let done = Rc::clone(&server_done);
            let _operation = context.introspect().get_server_info(move |info| {
                *sink.borrow_mut() = info.default_sink_name.as_ref().map(ToString::to_string);
                *source.borrow_mut() = info.default_source_name.as_ref().map(ToString::to_string);
                done.set(true);
            });
            while !server_done.get() {
                iterate(&mut mainloop)?;
            }
        }

        let sinks = Rc::new(RefCell::new(Vec::new()));
        let sinks_done = Rc::new(Cell::new(false));
        let sinks_failed = Rc::new(Cell::new(false));
        {
            let values = Rc::clone(&sinks);
            let done = Rc::clone(&sinks_done);
            let failed = Rc::clone(&sinks_failed);
            let _operation = context
                .introspect()
                .get_sink_info_list(move |result| match result {
                    ListResult::Item(info) => {
                        if let Some(name) = &info.name {
                            values.borrow_mut().push(DeviceInfo {
                                index: info.index,
                                name: name.to_string(),
                                description: info
                                    .description
                                    .as_ref()
                                    .map_or_else(|| name.to_string(), ToString::to_string),
                                monitor_source_name: info
                                    .monitor_source_name
                                    .as_ref()
                                    .map(ToString::to_string),
                            });
                        }
                    }
                    ListResult::End => done.set(true),
                    ListResult::Error => {
                        failed.set(true);
                        done.set(true);
                    }
                });
            while !sinks_done.get() {
                iterate(&mut mainloop)?;
            }
        }
        if sinks_failed.get() {
            bail!("PulseAudio failed to enumerate output devices");
        }

        let sources = Rc::new(RefCell::new(Vec::new()));
        let sources_done = Rc::new(Cell::new(false));
        let sources_failed = Rc::new(Cell::new(false));
        {
            let values = Rc::clone(&sources);
            let done = Rc::clone(&sources_done);
            let failed = Rc::clone(&sources_failed);
            let _operation =
                context
                    .introspect()
                    .get_source_info_list(move |result| match result {
                        ListResult::Item(info) => {
                            if let Some(name) = &info.name {
                                values.borrow_mut().push(DeviceInfo {
                                    index: info.index,
                                    name: name.to_string(),
                                    description: info
                                        .description
                                        .as_ref()
                                        .map_or_else(|| name.to_string(), ToString::to_string),
                                    monitor_source_name: None,
                                });
                            }
                        }
                        ListResult::End => done.set(true),
                        ListResult::Error => {
                            failed.set(true);
                            done.set(true);
                        }
                    });
            while !sources_done.get() {
                iterate(&mut mainloop)?;
            }
        }
        if sources_failed.get() {
            bail!("PulseAudio failed to enumerate input devices");
        }

        context.disconnect();
        Ok(DeviceCatalog {
            default_sink_name: default_sink_name.borrow().clone(),
            default_source_name: default_source_name.borrow().clone(),
            sinks: Rc::try_unwrap(sinks)
                .map_err(|_| anyhow::anyhow!("PulseAudio output list is still in use"))?
                .into_inner(),
            sources: Rc::try_unwrap(sources)
                .map_err(|_| anyhow::anyhow!("PulseAudio input list is still in use"))?
                .into_inner(),
        })
    }

    pub fn list_devices() -> Result<()> {
        let catalog = query_devices()?;
        println!("output / loopback devices:");
        for device in &catalog.sinks {
            let default = if Some(device.name.as_str()) == catalog.default_sink_name.as_deref() {
                " (default)"
            } else {
                ""
            };
            println!(
                "  {}: {} [{}]{}; monitor: {}",
                device.index,
                device.description,
                device.name,
                default,
                device
                    .monitor_source_name
                    .as_deref()
                    .unwrap_or("unavailable")
            );
        }
        println!("input devices:");
        for device in &catalog.sources {
            let default = if Some(device.name.as_str()) == catalog.default_source_name.as_deref() {
                " (default)"
            } else {
                ""
            };
            println!(
                "  {}: {} [{}]{}",
                device.index, device.description, device.name, default
            );
        }
        Ok(())
    }

    fn sample_spec() -> Result<Spec> {
        let spec = Spec {
            format: Format::FLOAT32NE,
            channels: 1,
            rate: SAMPLE_RATE,
        };
        ensure!(spec.is_valid(), "invalid PulseAudio sample specification");
        Ok(spec)
    }

    pub struct Player {
        stream: Simple,
        bytes: Vec<u8>,
    }

    impl Player {
        pub fn new(selector: Option<&str>) -> Result<Self> {
            let catalog = query_devices()?;
            let sink = catalog.playback_sink(selector)?;
            let description = catalog
                .sinks
                .iter()
                .find(|device| device.name == sink)
                .map_or(sink.as_str(), |device| device.description.as_str());
            eprintln!("playing on {description} [{sink}]");
            let spec = sample_spec()?;
            let stream = Simple::new(
                None,
                "SoundPass",
                Direction::Playback,
                Some(&sink),
                "SoundPass file transmission",
                &spec,
                None,
                None,
            )
            .context("open PulseAudio/PipeWire playback stream")?;
            Ok(Self {
                stream,
                bytes: Vec::new(),
            })
        }

        pub fn write(&mut self, samples: &[f32]) -> Result<()> {
            if samples.is_empty() {
                return Ok(());
            }
            self.bytes.clear();
            self.bytes.reserve(size_of_val(samples));
            for sample in samples {
                self.bytes.extend_from_slice(&sample.to_ne_bytes());
            }
            self.stream
                .write(&self.bytes)
                .context("write to PulseAudio/PipeWire playback stream")
        }

        pub fn finish(self) -> Result<()> {
            self.stream
                .drain()
                .context("finish PulseAudio/PipeWire playback")
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
        let catalog = query_devices()?;
        let source = catalog.capture_source(loopback, selector)?;
        let description = if loopback {
            catalog
                .sinks
                .iter()
                .find(|device| device.monitor_source_name.as_deref() == Some(source.as_str()))
                .map_or(source.as_str(), |device| device.description.as_str())
        } else {
            catalog
                .sources
                .iter()
                .find(|device| device.name == source)
                .map_or(source.as_str(), |device| device.description.as_str())
        };
        eprintln!(
            "recording {} from {description} [{source}]",
            if loopback { "loopback" } else { "input" }
        );
        let spec = sample_spec()?;
        let stream = Simple::new(
            None,
            "SoundPass",
            Direction::Record,
            Some(&source),
            "SoundPass file receiver",
            &spec,
            None,
            None,
        )
        .context("open PulseAudio/PipeWire recording stream")?;
        let mut raw = vec![0u8; 1024 * size_of::<f32>()];
        let mut samples = Vec::with_capacity(1024);
        let start = Instant::now();
        let mut last_report = Instant::now();
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
            stream
                .read(&mut raw)
                .context("read from PulseAudio/PipeWire recording stream")?;
            samples.clear();
            for bytes in raw.chunks_exact(size_of::<f32>()) {
                let sample = f32::from_ne_bytes(bytes.try_into().context("invalid audio sample")?);
                if let Some(writer) = wav.as_mut() {
                    writer.write_sample(sample)?;
                }
                samples.push(sample);
            }
            if let Some(done) = receiver.push_samples(&samples)? {
                if let Some(writer) = wav {
                    writer.finalize()?;
                }
                return Ok(Some(done));
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
        }
        if let Some(writer) = wav {
            writer.finalize()?;
        }
        Ok(None)
    }

    #[cfg(test)]
    mod tests {
        use super::{DeviceCatalog, DeviceInfo};

        fn catalog() -> DeviceCatalog {
            DeviceCatalog {
                default_sink_name: Some("speaker".into()),
                default_source_name: Some("microphone".into()),
                sinks: vec![DeviceInfo {
                    index: 3,
                    name: "speaker".into(),
                    description: "Built-in Audio".into(),
                    monitor_source_name: Some("speaker.monitor".into()),
                }],
                sources: vec![DeviceInfo {
                    index: 7,
                    name: "microphone".into(),
                    description: "USB Microphone".into(),
                    monitor_source_name: None,
                }],
            }
        }

        #[test]
        fn default_loopback_uses_the_default_sinks_monitor_source() {
            let catalog = catalog();
            assert_eq!(
                catalog.capture_source(true, None).unwrap(),
                "speaker.monitor"
            );
        }

        #[test]
        fn selectors_accept_index_internal_name_and_description() {
            let catalog = catalog();
            assert_eq!(catalog.playback_sink(Some("3")).unwrap(), "speaker");
            assert_eq!(
                catalog.playback_sink(Some("Built-in Audio")).unwrap(),
                "speaker"
            );
            assert_eq!(
                catalog.capture_source(false, Some("microphone")).unwrap(),
                "microphone"
            );
            assert_eq!(
                catalog.capture_source(false, Some("7")).unwrap(),
                "microphone"
            );
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{Player, capture, list_devices, play};

#[cfg(not(any(windows, target_os = "linux")))]
pub fn list_devices() -> anyhow::Result<()> {
    anyhow::bail!("live audio is not implemented on this operating system")
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn play(_: &[f32], _: Option<&str>) -> anyhow::Result<()> {
    anyhow::bail!("live audio is not implemented on this operating system")
}
#[cfg(not(any(windows, target_os = "linux")))]
pub struct Player;
#[cfg(not(any(windows, target_os = "linux")))]
impl Player {
    pub fn new(_: Option<&str>) -> anyhow::Result<Self> {
        anyhow::bail!("live audio is not implemented on this operating system")
    }
    pub fn write(&mut self, _: &[f32]) -> anyhow::Result<()> {
        anyhow::bail!("live audio is not implemented on this operating system")
    }
    pub fn finish(self) -> anyhow::Result<()> {
        anyhow::bail!("live audio is not implemented on this operating system")
    }
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn capture(
    _: &mut crate::Receiver,
    _: bool,
    _: Option<&str>,
    _: u64,
    _: Option<&std::path::Path>,
) -> anyhow::Result<Option<(String, Vec<u8>)>> {
    anyhow::bail!("live audio is not implemented on this operating system")
}
