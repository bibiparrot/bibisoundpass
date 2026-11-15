#![cfg(target_os = "linux")]

use std::process::Command;

#[test]
fn linux_device_listing_uses_a_linux_audio_backend() {
    for executable in [
        env!("CARGO_BIN_EXE_soundpass-send"),
        env!("CARGO_BIN_EXE_soundpass-recv"),
    ] {
        let output = Command::new(executable)
            .arg("--devices")
            .output()
            .expect("run SoundPass device listing");
        let diagnostics = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !diagnostics.contains("live audio requires Windows"),
            "Linux binary still contains the Windows-only audio stub: {diagnostics}"
        );
    }
}
