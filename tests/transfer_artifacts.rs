use anyhow::Result;
use soundpass_rs::{Profile, Receiver, Transmission, save_file};
use std::fs;
use std::process::Command;

#[test]
fn sender_writes_compressed_container_and_exact_128k_parts_beside_source() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("original.bin");
    let mut state = 0x72ad_1be5u32;
    let data: Vec<u8> = (0..140_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    fs::write(&source, data)?;

    let tx = Transmission::prepare(&source, 0)?;
    let container = fs::read(tx.container_path.as_ref().unwrap())?;
    let parts_dir = tx.parts_dir.as_ref().unwrap();
    let first = fs::read(parts_dir.join("part-0000.bin"))?;
    let second = fs::read(parts_dir.join("part-0001.bin"))?;
    assert_eq!(tx.part_count, 2);
    assert_eq!(first.len(), 128 * 1024);
    assert_eq!([first, second].concat(), container);
    let log = fs::read_to_string(tx.log_path.as_ref().unwrap())?;
    assert!(log.contains("\"stage\":\"compressed\""));
    assert!(log.contains("\"stage\":\"part_saved\""));
    let stages: Vec<_> = log
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["stage"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        &stages[..3],
        ["compressed", "container_saved", "part_saved"]
    );
    assert_eq!(stages.last().unwrap(), "frames_prepared");
    Ok(())
}

#[test]
fn receiver_persists_parts_then_merges_before_decompressing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("original.bin");
    let output = dir.path().join("received");
    let mut state = 0x1ced_45a7u32;
    let data: Vec<u8> = (0..140_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    fs::write(&source, &data)?;
    let tx = Transmission::prepare(&source, 0)?;
    assert_eq!(tx.part_count, 2);

    let mut rx = Receiver::with_output(Profile::Fast, &output)?;
    let mut saw_first = false;
    let mut completed = None;
    for frame in &tx.frames {
        completed = rx.push_frame(frame)?;
        if rx.parts_saved == 1 && !saw_first {
            saw_first = true;
            assert!(!output.join("original.bin").exists());
            assert_eq!(
                fs::read_dir(&output)?
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|x| x == "spc"))
                    .count(),
                0
            );
        }
        if completed.is_some() {
            break;
        }
    }
    assert!(saw_first);
    let (name, bytes) = completed.expect("receiver did not finish");
    let saved = save_file(&output, &name, &bytes, false)?;
    assert_eq!(fs::read(saved)?, data);
    assert_eq!(rx.parts_saved, 2);
    assert_eq!(
        fs::read_dir(&output)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "spc"))
            .count(),
        1
    );
    Ok(())
}

#[test]
fn cli_round_trip_records_prepare_receive_merge_and_output() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("hello-测试.txt");
    let wav = dir.path().join("transfer.wav");
    let output = dir.path().join("received");
    fs::write(&source, b"SoundPass CLI workflow")?;
    let sent = Command::new(env!("CARGO_BIN_EXE_soundpass-send"))
        .arg(&source)
        .args(["--no-play", "--passes", "1", "--repair-percent", "0"])
        .arg("--wav")
        .arg(&wav)
        .output()?;
    assert!(
        sent.status.success(),
        "sender: {}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let containers: Vec<_> = fs::read_dir(dir.path())?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "spc"))
        .collect();
    assert_eq!(containers.len(), 1);
    let send_log = fs::read_to_string(containers[0].path().with_extension("spc.jsonl"))?;
    assert!(send_log.contains("\"stage\":\"audio_completed\""));

    let received = Command::new(env!("CARGO_BIN_EXE_soundpass-recv"))
        .arg("--input")
        .arg(&wav)
        .arg("--out")
        .arg(&output)
        .output()?;
    assert!(
        received.status.success(),
        "receiver: {}",
        String::from_utf8_lossy(&received.stderr)
    );
    assert_eq!(
        fs::read(output.join("hello-测试.txt"))?,
        b"SoundPass CLI workflow"
    );
    let logs: Vec<_> = fs::read_dir(&output)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .collect();
    assert_eq!(logs.len(), 1);
    let receive_log = fs::read_to_string(logs[0].path())?;
    for stage in ["part_received", "merged", "decompressed", "file_saved"] {
        assert!(
            receive_log.contains(&format!("\"stage\":\"{stage}\"")),
            "missing {stage}"
        );
    }
    Ok(())
}

#[test]
fn receiver_resumes_from_a_verified_part_saved_before_restart() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("resume.bin");
    let output = dir.path().join("received");
    let mut state = 0xcafe_1234u32;
    let data: Vec<u8> = (0..140_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    fs::write(&source, &data)?;
    let tx = Transmission::prepare(&source, 0)?;
    let mut first = Receiver::with_output(Profile::Fast, &output)?;
    for frame in &tx.frames {
        first.push_frame(frame)?;
        if first.parts_saved == 1 {
            break;
        }
    }
    assert_eq!(first.parts_saved, 1);
    drop(first);

    let mut resumed = Receiver::with_output(Profile::Fast, &output)?;
    let mut result = None;
    for frame in tx
        .frames
        .iter()
        .filter(|f| u16::from_le_bytes([f[15], f[16]]) == 1)
    {
        if let Some(done) = resumed.push_frame(frame)? {
            result = Some(done);
            break;
        }
    }
    let (name, recovered) = result.expect("saved part was not reused");
    assert_eq!(name, "resume.bin");
    assert_eq!(recovered, data);
    assert_eq!(resumed.parts_saved, 2);
    Ok(())
}

#[test]
fn receiver_can_finish_again_from_parts_already_on_disk() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("repeat.txt");
    let output = dir.path().join("received");
    fs::write(&source, b"repeat this transfer")?;
    let tx = Transmission::prepare(&source, 0)?;
    let mut first = Receiver::with_output(Profile::Fast, &output)?;
    assert!(
        tx.frames
            .iter()
            .any(|frame| first.push_frame(frame).unwrap().is_some())
    );
    let mut second = Receiver::with_output(Profile::Fast, &output)?;
    let repeated = second.push_frame(&tx.frames[0])?;
    let (_, bytes) = repeated.expect("receiver did not reuse completed parts");
    assert_eq!(bytes, b"repeat this transfer");
    Ok(())
}
