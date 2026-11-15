# SoundPass Rust

Two command-line executables transfer one file through the sound card on Windows and Linux. The sender packs the original filename and bytes into one lossless container, chooses the smallest of stored, zstd, Brotli, and LZMA, writes that compressed container beside the source, splits it into 128 KiB parts, encodes those parts as audio, and plays them. The receiver captures the chosen output device through WASAPI loopback on Windows or a PulseAudio/PipeWire monitor source on Linux, reconstructs and saves each part, merges the compressed container after all parts arrive, verifies the container hash, decompresses it, verifies the file hash, and writes the original filename. Microphone or line-in capture and offline WAV decoding are also supported.

## Build and use

From this directory:

```powershell
cargo build --release --bins
```

Linux builds require the PulseAudio client development library at compile time (`libpulse-dev` on Debian/Ubuntu or `pulseaudio-libs-devel` on Fedora). At runtime, use PulseAudio or the `pipewire-pulse` compatibility service; most current desktop distributions already provide one of them.

Start the receiver first, then the sender in another terminal:

```powershell
.\target\release\soundpass-recv.exe --out .\received
.\target\release\soundpass-send.exe .\my-file.txt
```

Both programs use the default playback device and the high-rate `loopback` OFDM profile. The receiver records the **output device** via shared-mode WASAPI loopback. Use `--devices` to list output and input devices, and `--device 0` or `--device "exact device name"` to select one. For speaker-to-microphone, line-in, or an uncertain channel, select the same ggwave profile on both sides:

On Linux, the same commands use the default PulseAudio/PipeWire sink and automatically capture its monitor source. `--devices` prints each sink's index, internal name, display name, and monitor source. Pass its index, internal name, or exact display name to `--device`. Start the receiver before the sender:

```bash
soundpass-recv --out ./received
soundpass-send ./my-file.txt
```

```powershell
# Faster ggwave air profile
.\target\release\soundpass-recv.exe --loopback false --profile air --out .\received
.\target\release\soundpass-send.exe .\my-file.txt --profile air

# Slowest and most robust ggwave profile
.\target\release\soundpass-recv.exe --loopback false --profile robust --out .\received
.\target\release\soundpass-send.exe .\my-file.txt --profile robust
```

`fast` remains as a backward-compatible name for the same ggwave protocol as `air`. Run `--help` for all options.

To make and decode a WAV without audio hardware:

```powershell
.\target\release\soundpass-send.exe .\my-file.txt --wav .\transfer.wav --no-play
.\target\release\soundpass-recv.exe --input .\transfer.wav --out .\received
```

The sender includes 40% RaptorQ repair packets **per part** by default and repeats the carousel twice. `--repair-percent` and `--passes` adjust those values. The receiver can join after playback starts if enough of the remaining carousel arrives. It can also restart and reuse saved parts whose BLAKE3 sidecar hashes still match. Files are written with an unused numeric suffix unless `--overwrite` is given. A successful receive has a verified BLAKE3 file hash. The receiver exits after one file; `--max-seconds` bounds live listening.

## Files and step log

For `my-file.txt`, the sender writes these files **in the same directory as the source** before audio encoding. `<id>` is the first 16 hex digits of the compressed container's BLAKE3 hash.

```text
my-file.txt
my-file.txt.<id>.spc                  compressed container
my-file.txt.<id>.spc.parts/
  part-0000.bin                       exactly 128 KiB, except the last part
  part-0001.bin
my-file.txt.<id>.spc.jsonl            sender step log
```

The receiver writes these files under `--out` as parts complete:

```text
received/
  soundpass-<id>.parts/
    part-0000.bin                     reconstructed compressed part
    part-0000.blake3                  full part hash for restart validation
    part-0001.bin
    part-0001.blake3
  soundpass-<id>.spc                  merged compressed container
  soundpass-<id>.jsonl                receiver step log
  my-file.txt                         decompressed, hash-verified original
```

Each JSONL line has a Unix timestamp in milliseconds (`unix_ms`) and a `stage`. The sender records `compressed`, `container_saved`, each `part_saved`, `frames_prepared`, each pass and encoded frame, `audio_completed`, and WAV or playback completion. The receiver records each accepted `packet_received`, each `part_received` or `part_resumed`, `merged`, `decompressed`, and `file_saved`, including paths, sizes, and hashes where applicable. An incomplete receive leaves verified parts on disk; it does not create the merged container or original file.

## Modem profiles

| Profile | Modem | Intended link | Measured or expected rate |
| --- | --- | --- | --- |
| `loopback` (default) | Pure Rust OFDM, 16-QAM + RS(137,105) | WASAPI loopback, virtual cable, clean line cable | **25.36 kbps measured end to end** for a 32 KiB incompressible file, including start/end silence |
| `air` / `fast` | ggwave Audible Fastest | Speaker to microphone and less predictable links | Slow; depends on hardware and room |
| `robust` | ggwave Audible Normal | Poor acoustic links and short data | Slowest, with the most time diversity |

The `loopback` modem uses a 256-point FFT at 48 kHz, a 32-sample cyclic prefix, 112 real-signal subcarriers, 16-QAM data, and a known BPSK OFDM preamble. The receiver uses the preamble for frame synchronization and per-carrier complex equalization. A 32-byte Reed-Solomon inner code corrects up to 16 erroneous bytes in each 105-byte packet before its CRC is checked. Each frame is independently synchronized, so a frame that is still damaged or missing is rejected and replaced by the outer RaptorQ layer when enough repair packets arrive. Its steady fountain payload capacity is about 26 kbps before start/end silence and container overhead.

This high-rate profile is designed for clean loopback and cable channels. It has not been tuned for speaker-to-microphone use. Use `air` or `robust` for that path.

## Design and research

The pipeline is `filename + file bytes → smallest lossless compression → 128 KiB parts → RaptorQ symbols per part → packet CRC → selected modem → live audio output and capture → verified parts → merged container → hash-checked file`. Every modem frame carries its session ID, part index, compressed total length, part hash, and fountain configuration, so the receiver needs no initial announcement. The OFDM profile uses packet CRC plus RaptorQ. The ggwave profiles additionally use ggwave's built-in Reed-Solomon correction inside each audio message.

[ggwave's Rust port](https://docs.rs/ggwave/latest/ggwave/) supplies the two acoustic fallbacks with FSK audio, Reed-Solomon correction, and a streaming decoder. [RaptorQ](https://docs.rs/raptorq/latest/raptorq/) supplies the one-way packet erasure layer for every profile. The high-rate OFDM implementation uses [rustfft](https://docs.rs/rustfft/latest/rustfft/) and is contained in `src/ofdm.rs`.

The [`wasapi` crate](https://docs.rs/wasapi/latest/wasapi/) exposes Windows playback, capture, and loopback directly. [Microsoft's loopback documentation](https://learn.microsoft.com/windows/win32/coreaudio/loopback-recording) requires **shared mode** for loopback; exclusive-mode capture cannot be combined with it. The implementation requests 48 kHz stereo float from WASAPI and downmixes to mono for the decoder.

On Linux, [`libpulse-binding`](https://docs.rs/libpulse-binding/latest/libpulse_binding/) and [`libpulse-simple-binding`](https://docs.rs/libpulse-simple-binding/latest/libpulse_simple_binding/) connect directly to PulseAudio or PipeWire's PulseAudio compatibility server. Every output sink exposes a monitor source, which the receiver selects automatically for same-computer loopback. Playback and capture use 48 kHz mono float samples.

## Limits and verification

- Maximum compressed envelope: 1 MiB. Maximum original file: 64 MiB.
- The OFDM profile meets the PRD's initial 20 kbps loopback target in the reproducible 32 KiB test. Its 25.36 kbps result is a measurement of generated, filtered, and 16-bit-quantized audio on this build, not a claim for every audio device. ggwave remains much slower. Ordinary RIFF WAV files also have a roughly 4 GiB size limit; live playback avoids storing all PCM in memory.
- The OFDM implementation has synchronization, a cyclic prefix, 16-QAM, Reed-Solomon inner correction, and per-carrier equalization. It does not yet have adaptive constellation selection, pilot-based continuous clock-drift tracking, resampling, calibration, a live input meter, or a full analog channel simulator.
- Successful transfers are bit exact. Packet repair cannot guarantee recovery from every loss pattern; use more passes or repair packets when needed.
- Tests cover 128 KiB splitting, part persistence, restart recovery, an offline 16-bit WAV CLI round trip, OFDM throughput, periodic 30 ms audio dropouts, and recovery after 20% of encoded packets were removed. The default OFDM profile completed live loopback round trips on Windows WASAPI and a Linux PulseAudio null sink with matching file hashes; captured float samples can be saved with `--save-capture capture.wav`. Speaker-to-microphone transfer and other devices have not been measured.
- Earlier WAV files and transmissions used a different packet format. Regenerate them with this version of the sender.

Run `cargo test --release` for all sender, receiver, packet loss, filename, and corruption checks. The integration tests use the same CLI executables as the commands above and keep their generated artifacts inside temporary directories.

## Release packages

Every `v*` tag runs the GitHub Actions release workflow and publishes SHA-256 checksums together with these packages:

- Linux x86_64 and aarch64: AppImage, RPM, and `.tar.gz`
- Windows x86_64: portable ZIP, Inno Setup EXE, and MSI
- macOS x86_64 and arm64: DMG, PKG, and ZIP

Windows and Linux support live playback and same-computer loopback capture. Windows uses WASAPI; Linux uses PulseAudio or PipeWire's PulseAudio compatibility service and the selected sink's monitor source. macOS currently supports the complete lossless container/part/modem pipeline through WAV files (`soundpass-send --wav ... --no-play` and `soundpass-recv --input ...`) but not live playback/capture. The installers are unsigned; Windows SmartScreen and macOS Gatekeeper may therefore ask for confirmation.

The Linux AppImage wraps both commands. Use its first argument to select one:

```bash
./soundpass-0.1.1-linux-x86_64.AppImage recv --out ./received
./soundpass-0.1.1-linux-x86_64.AppImage send ./my-file.txt
```

For reproducible releases, update the version in `Cargo.toml`, commit it, then create and push the matching tag, for example `v0.1.1`. The workflow also supports a manual run with an explicit version.
