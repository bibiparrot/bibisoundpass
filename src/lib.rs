use anyhow::{Context, Result, bail, ensure};
use ggwave::{Decoder as WaveDecoder, Encoder as WaveEncoder, Parameters, Protocol};
use raptorq::{
    Decoder as FountainDecoder, Encoder as FountainEncoder, EncodingPacket,
    ObjectTransmissionInformation,
};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

pub mod audio;
mod ofdm;

pub const SAMPLE_RATE: u32 = 48_000;
pub const MAX_COMPRESSED: usize = 1_048_576;
pub const MAX_ORIGINAL: usize = 64 * 1024 * 1024;
pub const PART_SIZE: usize = 128 * 1024;
const SYMBOL_SIZE: u16 = 80;
const MAGIC: &[u8; 4] = b"SP01";
const FRAME_MAGIC: &[u8; 2] = b"SP";
const HEADER_LEN: usize = 2 + 1 + 8 + 4 + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Profile {
    Loopback,
    Air,
    Fast,
    Robust,
}

impl Profile {
    #[must_use]
    pub fn protocol(self) -> Option<Protocol> {
        match self {
            Self::Loopback => None,
            Self::Air | Self::Fast => Some(Protocol::AudibleFastest),
            Self::Robust => Some(Protocol::AudibleNormal),
        }
    }
}

fn pack(input: &Path) -> Result<Vec<u8>> {
    let name = input
        .file_name()
        .context("input has no filename")?
        .to_str()
        .context("filename is not valid Unicode")?;
    let name_bytes = name.as_bytes();
    ensure!(name_bytes.len() <= u16::MAX as usize, "filename too long");
    let file = fs::read(input).with_context(|| format!("read {}", input.display()))?;
    ensure!(
        file.len() <= MAX_ORIGINAL,
        "file exceeds 64 MiB original-size limit"
    );
    let mut plain = Vec::with_capacity(46 + name_bytes.len() + file.len());
    plain.extend_from_slice(MAGIC);
    plain.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    plain.extend_from_slice(&(file.len() as u64).to_le_bytes());
    plain.extend_from_slice(blake3::hash(&file).as_bytes());
    plain.extend_from_slice(name_bytes);
    plain.extend_from_slice(&file);

    let mut candidates = vec![(0u8, plain.clone())];
    candidates.push((1, zstd::bulk::compress(&plain, 19)?));
    let mut brotli_data = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut brotli_data, 4096, 11, 22);
        writer.write_all(&plain)?;
    }
    candidates.push((2, brotli_data));
    let mut lzma_data = Vec::new();
    lzma_rs::lzma_compress(&mut Cursor::new(&plain), &mut lzma_data)?;
    candidates.push((3, lzma_data));
    let (codec, smallest) = candidates
        .into_iter()
        .min_by_key(|(_, bytes)| bytes.len())
        .unwrap();
    let mut result = Vec::with_capacity(9 + smallest.len());
    result.push(codec);
    result.extend_from_slice(&(plain.len() as u64).to_le_bytes());
    result.extend_from_slice(&smallest);
    ensure!(
        result.len() <= MAX_COMPRESSED,
        "compressed envelope exceeds 1 MiB"
    );
    Ok(result)
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other(
                "decompressed data exceeds declared size",
            ));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn unpack(envelope: &[u8]) -> Result<(String, Vec<u8>)> {
    ensure!(
        envelope.len() >= 9 && envelope.len() <= MAX_COMPRESSED,
        "invalid envelope length"
    );
    let len = u64::from_le_bytes(envelope[1..9].try_into()?);
    ensure!(
        len <= (MAX_ORIGINAL + 46 + u16::MAX as usize) as u64,
        "declared size too large"
    );
    let limit = len as usize;
    let mut writer = LimitedWriter {
        bytes: Vec::new(),
        limit,
    };
    match envelope[0] {
        0 => writer.write_all(&envelope[9..])?,
        1 => {
            let mut decoder = zstd::stream::read::Decoder::new(&envelope[9..])?;
            std::io::copy(&mut decoder, &mut writer)?;
        }
        2 => {
            let mut decoder = brotli::Decompressor::new(&envelope[9..], 4096);
            std::io::copy(&mut decoder, &mut writer)?;
        }
        3 => lzma_rs::lzma_decompress(&mut Cursor::new(&envelope[9..]), &mut writer)?,
        _ => bail!("unknown compression codec"),
    }
    let plain = writer.bytes;
    ensure!(
        plain.len() == limit && plain.len() >= 46,
        "invalid decompressed length"
    );
    ensure!(&plain[..4] == MAGIC, "invalid envelope magic");
    let name_len = u16::from_le_bytes(plain[4..6].try_into()?) as usize;
    let file_len = u64::from_le_bytes(plain[6..14].try_into()?);
    ensure!(file_len <= MAX_ORIGINAL as u64, "original file too large");
    ensure!(
        46 + name_len + file_len as usize == plain.len(),
        "invalid file length"
    );
    let name = String::from_utf8(plain[46..46 + name_len].to_vec())?;
    let data = plain[46 + name_len..].to_vec();
    ensure!(
        blake3::hash(&data).as_bytes() == &plain[14..46],
        "file hash mismatch"
    );
    Ok((safe_name(&name), data))
}

fn safe_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let sanitized = sanitized.trim_end_matches([' ', '.']);
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        return "received-file".into();
    }
    let stem = sanitized
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        format!("_{sanitized}")
    } else {
        sanitized.into()
    }
}

pub fn save_file(out_dir: &Path, name: &str, data: &[u8], overwrite: bool) -> Result<PathBuf> {
    fs::create_dir_all(out_dir)?;
    let safe = safe_name(name);
    let path = out_dir.join(&safe);
    if overwrite {
        fs::write(&path, data)?;
        return Ok(path);
    }
    for i in 0..10_000 {
        let candidate = if i == 0 {
            path.clone()
        } else {
            let p = Path::new(&safe);
            let stem = p.file_stem().unwrap_or_default().to_string_lossy();
            let ext = p
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            out_dir.join(format!("{stem} ({i}){ext}"))
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut f) => {
                f.write_all(data)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    bail!("unable to find an unused output filename")
}

pub fn record(path: &Path, mut event: serde_json::Value) -> Result<()> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    event["unix_ms"] = serde_json::json!(millis);
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    writeln!(file, "{event}")?;
    Ok(())
}

pub struct Transmission {
    pub frames: Vec<Vec<u8>>,
    pub envelope_len: usize,
    pub part_count: usize,
    pub container_path: Option<PathBuf>,
    pub parts_dir: Option<PathBuf>,
    pub log_path: Option<PathBuf>,
}

impl Transmission {
    pub fn from_file(path: &Path, repair_percent: u32) -> Result<Self> {
        let envelope = pack(path)?;
        Self::from_envelope(envelope, repair_percent)
    }

    pub fn prepare(path: &Path, repair_percent: u32) -> Result<Self> {
        let envelope = pack(path)?;
        let hash = blake3::hash(&envelope);
        let id = &hash.to_hex()[..16];
        let name = path
            .file_name()
            .context("input has no filename")?
            .to_str()
            .context("filename is not valid Unicode")?;
        let base = format!("{name}.{id}.spc");
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let container_path = parent.join(&base);
        let parts_dir = parent.join(format!("{base}.parts"));
        let log_path = parent.join(format!("{base}.jsonl"));
        fs::create_dir_all(&parts_dir)?;
        record(
            &log_path,
            serde_json::json!({"stage":"compressed", "codec":envelope[0], "bytes":envelope.len(), "source":path.display().to_string()}),
        )?;
        fs::write(&container_path, &envelope)?;
        record(
            &log_path,
            serde_json::json!({"stage":"container_saved", "path":container_path.display().to_string()}),
        )?;
        for (i, part) in envelope.chunks(PART_SIZE).enumerate() {
            let part_path = parts_dir.join(format!("part-{i:04}.bin"));
            fs::write(&part_path, part)?;
            record(
                &log_path,
                serde_json::json!({"stage":"part_saved", "index":i, "bytes":part.len(), "hash":blake3::hash(part).to_hex().to_string(), "path":part_path.display().to_string()}),
            )?;
        }
        let mut tx = Self::from_envelope(envelope, repair_percent)?;
        record(
            &log_path,
            serde_json::json!({"stage":"frames_prepared", "frames":tx.frames.len(), "parts":tx.part_count, "repair_percent":repair_percent}),
        )?;
        tx.container_path = Some(container_path);
        tx.parts_dir = Some(parts_dir);
        tx.log_path = Some(log_path);
        Ok(tx)
    }

    fn from_envelope(envelope: Vec<u8>, repair_percent: u32) -> Result<Self> {
        ensure!(repair_percent <= 200, "repair percentage must be 0..200");
        let hash = blake3::hash(&envelope);
        let session = &hash.as_bytes()[..8];
        let mut frames = Vec::new();
        for (part_index, part) in envelope.chunks(PART_SIZE).enumerate() {
            let encoder = FountainEncoder::with_defaults(part, SYMBOL_SIZE);
            let source_count = part.len().div_ceil(SYMBOL_SIZE as usize);
            let repair = (source_count as u32 * repair_percent).div_ceil(100);
            for packet in encoder.get_encoded_packets(repair) {
                let mut frame = Vec::with_capacity(140);
                frame.extend_from_slice(FRAME_MAGIC);
                frame.push(3);
                frame.extend_from_slice(session);
                frame.extend_from_slice(&(envelope.len() as u32).to_le_bytes());
                frame.extend_from_slice(&(part_index as u16).to_le_bytes());
                frame.extend_from_slice(&packet.serialize());
                let crc = crc32fast::hash(&frame);
                frame.extend_from_slice(&crc.to_le_bytes());
                ensure!(
                    frame.len() <= ggwave::MAX_LENGTH_VARIABLE,
                    "modem packet too long"
                );
                frames.push(frame);
            }
        }
        Ok(Self {
            frames,
            envelope_len: envelope.len(),
            part_count: envelope.len().div_ceil(PART_SIZE),
            container_path: None,
            parts_dir: None,
            log_path: None,
        })
    }

    pub fn stream_audio(
        &self,
        profile: Profile,
        passes: u32,
        mut sink: impl FnMut(&[f32]) -> Result<()>,
    ) -> Result<u64> {
        ensure!(passes > 0 && passes <= 100, "passes must be 1..100");
        let wave_encoder = profile
            .protocol()
            .map(|_| WaveEncoder::new(Parameters::default()))
            .transpose()?;
        let ofdm_encoder = (profile == Profile::Loopback).then(ofdm::Encoder::new);
        let leading = vec![
            0.0;
            if profile == Profile::Loopback {
                SAMPLE_RATE as usize / 10
            } else {
                SAMPLE_RATE as usize / 2
            }
        ];
        let gap = vec![
            0.0;
            if profile == Profile::Loopback {
                0
            } else {
                SAMPLE_RATE as usize / 10
            }
        ];
        if let Some(log) = &self.log_path {
            record(
                log,
                serde_json::json!({"stage":"audio_started", "profile":format!("{profile:?}").to_ascii_lowercase(), "passes":passes, "frames_per_pass":self.frames.len()}),
            )?;
        }
        sink(&leading)?;
        let mut count = leading.len() as u64;
        for pass in 0..passes {
            if let Some(log) = &self.log_path {
                record(
                    log,
                    serde_json::json!({"stage":"pass_started", "pass":pass + 1}),
                )?;
            }
            for (frame_index, frame) in self.frames.iter().enumerate() {
                let chunk = if let Some(encoder) = &ofdm_encoder {
                    encoder.encode(frame)?
                } else {
                    wave_encoder
                        .as_ref()
                        .unwrap()
                        .encode(frame, profile.protocol().unwrap(), 25)?
                };
                sink(&chunk)?;
                sink(&gap)?;
                count += (chunk.len() + gap.len()) as u64;
                if let Some(log) = &self.log_path {
                    record(
                        log,
                        serde_json::json!({"stage":"frame_encoded", "pass":pass + 1, "index":frame_index, "part":u16::from_le_bytes([frame[15], frame[16]])}),
                    )?;
                }
            }
            if let Some(log) = &self.log_path {
                record(
                    log,
                    serde_json::json!({"stage":"pass_completed", "pass":pass + 1}),
                )?;
            }
        }
        sink(&leading)?;
        if let Some(log) = &self.log_path {
            record(
                log,
                serde_json::json!({"stage":"audio_completed", "samples":count + leading.len() as u64}),
            )?;
        }
        Ok(count + leading.len() as u64)
    }
}

struct PartDecoder {
    decoder: FountainDecoder,
}

struct Session {
    total_len: usize,
    parts: Vec<Option<Vec<u8>>>,
    decoders: HashMap<u16, PartDecoder>,
}

pub struct Receiver {
    wave: Option<WaveDecoder>,
    ofdm: Option<ofdm::Decoder>,
    sessions: HashMap<[u8; 8], Session>,
    output_dir: Option<PathBuf>,
    pub heard: usize,
    pub accepted: usize,
    pub parts_saved: usize,
    pub completed_log: Option<PathBuf>,
}

impl Receiver {
    pub fn new(profile: Profile) -> Result<Self> {
        Ok(Self {
            wave: profile
                .protocol()
                .map(|protocol| WaveDecoder::with_protocols(Parameters::default(), &[protocol]))
                .transpose()?,
            ofdm: if profile == Profile::Loopback {
                Some(ofdm::Decoder::new(
                    HEADER_LEN + 4 + SYMBOL_SIZE as usize + 4,
                )?)
            } else {
                None
            },
            sessions: HashMap::new(),
            output_dir: None,
            heard: 0,
            accepted: 0,
            parts_saved: 0,
            completed_log: None,
        })
    }

    pub fn with_output(profile: Profile, output: &Path) -> Result<Self> {
        fs::create_dir_all(output)?;
        let mut receiver = Self::new(profile)?;
        receiver.output_dir = Some(output.to_path_buf());
        Ok(receiver)
    }

    pub fn push_samples(&mut self, samples: &[f32]) -> Result<Option<(String, Vec<u8>)>> {
        if let Some(decoder) = &mut self.ofdm {
            let frames = decoder.push(samples);
            for frame in frames {
                if let Some(done) = self.push_frame(&frame)? {
                    return Ok(Some(done));
                }
            }
            return Ok(None);
        }
        for block in samples.chunks(1024) {
            if let Some(frame) = self.wave.as_mut().unwrap().push(block) {
                if let Some(done) = self.push_frame(&frame)? {
                    return Ok(Some(done));
                }
            }
        }
        Ok(None)
    }

    pub fn push_frame(&mut self, frame: &[u8]) -> Result<Option<(String, Vec<u8>)>> {
        self.heard += 1;
        if frame.len() != HEADER_LEN + 4 + SYMBOL_SIZE as usize + 4
            || &frame[..2] != FRAME_MAGIC
            || frame[2] != 3
        {
            if std::env::var_os("SOUNDPASS_DSP_DEBUG").is_some() {
                eprintln!(
                    "OFDM decoded invalid frame; prefix={:02x?}",
                    &frame[..16.min(frame.len())]
                );
            }
            return Ok(None);
        }
        let crc_at = frame.len() - 4;
        if crc32fast::hash(&frame[..crc_at]) != u32::from_le_bytes(frame[crc_at..].try_into()?) {
            if std::env::var_os("SOUNDPASS_DSP_DEBUG").is_some() {
                eprintln!(
                    "OFDM decoded frame failed CRC; prefix={:02x?}",
                    &frame[..16.min(frame.len())]
                );
            }
            return Ok(None);
        }
        let session_id: [u8; 8] = frame[3..11].try_into()?;
        let total_len = u32::from_le_bytes(frame[11..15].try_into()?) as usize;
        let part_index = u16::from_le_bytes(frame[15..17].try_into()?);
        if total_len == 0 || total_len > MAX_COMPRESSED {
            return Ok(None);
        }
        let part_count = total_len.div_ceil(PART_SIZE);
        if part_index as usize >= part_count {
            return Ok(None);
        }
        let part_len = (total_len - part_index as usize * PART_SIZE).min(PART_SIZE);
        let config = ObjectTransmissionInformation::with_defaults(part_len as u64, SYMBOL_SIZE);
        let packet = EncodingPacket::deserialize(&frame[HEADER_LEN..crc_at]);
        if packet.payload_id().source_block_number() >= config.source_blocks() {
            return Ok(None);
        }
        let id = hex_id(&session_id);
        let log = self
            .output_dir
            .as_ref()
            .map(|out| out.join(format!("soundpass-{id}.jsonl")));
        if !self.sessions.contains_key(&session_id) {
            let parts = if let Some(out) = &self.output_dir {
                load_saved_parts(out, &id, total_len, log.as_deref())?
            } else {
                vec![None; part_count]
            };
            self.parts_saved += parts.iter().filter(|part| part.is_some()).count();
            self.sessions.insert(
                session_id,
                Session {
                    total_len,
                    parts,
                    decoders: HashMap::new(),
                },
            );
        }
        if self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.parts.iter().all(Option::is_some))
        {
            return self.complete_session(session_id);
        }
        let session = self.sessions.get_mut(&session_id).unwrap();
        if session.total_len != total_len || session.parts[part_index as usize].is_some() {
            return Ok(None);
        }
        let part = session
            .decoders
            .entry(part_index)
            .or_insert_with(|| PartDecoder {
                decoder: FountainDecoder::new(config),
            });
        self.accepted += 1;
        if let Some(path) = &log {
            record(
                path,
                serde_json::json!({"stage":"packet_received", "part":part_index, "symbol":packet.payload_id().encoding_symbol_id()}),
            )?;
        }
        if let Some(bytes) = part.decoder.decode(packet) {
            if let Some(out) = &self.output_dir {
                let dir = out.join(format!("soundpass-{id}.parts"));
                fs::create_dir_all(&dir)?;
                let path = dir.join(format!("part-{part_index:04}.bin"));
                fs::write(&path, &bytes)?;
                fs::write(
                    dir.join(format!("part-{part_index:04}.blake3")),
                    blake3::hash(&bytes).to_hex().as_str(),
                )?;
                if let Some(log) = &log {
                    record(
                        log,
                        serde_json::json!({"stage":"part_received", "index":part_index, "bytes":bytes.len(), "hash":blake3::hash(&bytes).to_hex().to_string(), "path":path.display().to_string()}),
                    )?;
                }
            }
            session.parts[part_index as usize] = Some(bytes);
            session.decoders.remove(&part_index);
            self.parts_saved += 1;
            return self.complete_session(session_id);
        }
        Ok(None)
    }

    fn complete_session(&mut self, session_id: [u8; 8]) -> Result<Option<(String, Vec<u8>)>> {
        let session = self
            .sessions
            .get(&session_id)
            .context("missing transfer session")?;
        if !session.parts.iter().all(Option::is_some) {
            return Ok(None);
        }
        let envelope: Vec<u8> = session
            .parts
            .iter()
            .flat_map(|part| part.as_ref().unwrap().iter().copied())
            .collect();
        ensure!(
            envelope.len() == session.total_len
                && blake3::hash(&envelope).as_bytes()[..8] == session_id,
            "merged container hash mismatch"
        );
        let id = hex_id(&session_id);
        let log = self
            .output_dir
            .as_ref()
            .map(|out| out.join(format!("soundpass-{id}.jsonl")));
        if let Some(out) = &self.output_dir {
            let path = out.join(format!("soundpass-{id}.spc"));
            fs::write(&path, &envelope)?;
            if let Some(log) = &log {
                record(
                    log,
                    serde_json::json!({"stage":"merged", "bytes":envelope.len(), "path":path.display().to_string()}),
                )?;
            }
        }
        let file = unpack(&envelope)?;
        if let Some(log) = &log {
            record(
                log,
                serde_json::json!({"stage":"decompressed", "filename":file.0, "bytes":file.1.len()}),
            )?;
            self.completed_log = Some(log.clone());
        }
        Ok(Some(file))
    }
}

fn hex_id(id: &[u8; 8]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn load_saved_parts(
    out: &Path,
    id: &str,
    total_len: usize,
    log: Option<&Path>,
) -> Result<Vec<Option<Vec<u8>>>> {
    let dir = out.join(format!("soundpass-{id}.parts"));
    let mut parts = vec![None; total_len.div_ceil(PART_SIZE)];
    for (index, slot) in parts.iter_mut().enumerate() {
        let path = dir.join(format!("part-{index:04}.bin"));
        let hash_path = dir.join(format!("part-{index:04}.blake3"));
        if !path.exists() || !hash_path.exists() {
            continue;
        }
        let bytes = fs::read(&path)?;
        let hash = fs::read_to_string(&hash_path)?;
        let expected_len = (total_len - index * PART_SIZE).min(PART_SIZE);
        if bytes.len() != expected_len || blake3::hash(&bytes).to_hex().as_str() != hash.trim() {
            continue;
        }
        if let Some(log) = log {
            record(
                log,
                serde_json::json!({"stage":"part_resumed", "index":index, "path":path.display().to_string()}),
            )?;
        }
        *slot = Some(bytes);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recover_after_one_lost_packet() -> Result<()> {
        let path = std::env::temp_dir().join(format!("soundpass-{}-测试.bin", std::process::id()));
        let source = b"repeatable content".repeat(100);
        fs::write(&path, &source)?;
        let tx = Transmission::from_file(&path, 100)?;
        fs::remove_file(&path)?;
        let mut rx = Receiver::new(Profile::Fast)?;
        let mut recovered = None;
        for frame in tx.frames.iter().skip(1) {
            if let Some(file) = rx.push_frame(frame)? {
                recovered = Some(file);
                break;
            }
        }
        let (name, bytes) = recovered.context("repair packets did not recover lost frame")?;
        ensure!(
            name.contains("测试") && bytes == source,
            "recovered content differs"
        );
        Ok(())
    }

    #[test]
    fn invalid_frame_and_filename_cannot_escape() -> Result<()> {
        let mut rx = Receiver::new(Profile::Robust)?;
        ensure!(rx.push_frame(b"garbage")?.is_none(), "accepted bad frame");
        ensure!(
            safe_name("../bad\\name.txt") == ".._bad_name.txt",
            "unsafe filename"
        );
        ensure!(safe_name(".env") == ".env", "changed a valid filename");
        Ok(())
    }

    #[test]
    fn recover_with_twenty_percent_packet_loss() -> Result<()> {
        let path = std::env::temp_dir().join(format!("soundpass-loss-{}.bin", std::process::id()));
        let mut state = 0x1234_5678u32;
        let data: Vec<u8> = (0..5000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(&path, &data)?;
        let tx = Transmission::from_file(&path, 40)?;
        fs::remove_file(&path)?;
        let mut rx = Receiver::new(Profile::Fast)?;
        let mut result = None;
        for (i, frame) in tx.frames.iter().enumerate() {
            if i % 5 == 0 {
                continue;
            }
            if let Some(file) = rx.push_frame(frame)? {
                result = Some(file);
                break;
            }
        }
        let (_, recovered) = result.context("failed to recover after 20% packet loss")?;
        ensure!(recovered == data, "recovered bytes differ");
        Ok(())
    }
}
