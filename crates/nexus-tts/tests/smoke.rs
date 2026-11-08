//! Renders real speech through the built helper. Needs the voices
//! (`tts.tar` from the model pack, unpacked so `<pack>/tts/` exists) and a
//! build with the synthesiser:
//!
//! `NEXUS_TEST_PACK=/path/to/pack cargo test -p nexus-tts --features sherpa -- --ignored`

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn tts_dir() -> PathBuf {
    std::env::var("NEXUS_TEST_PACK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models"))
        .join("tts")
}

#[test]
#[ignore = "needs the downloaded voices and --features sherpa"]
fn renders_please_leave_the_area_in_every_voice() {
    for voice in ["en-US-female", "en-US-male", "es-ES-male"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nexus-tts"))
            .arg("--model-dir")
            .arg(tts_dir())
            .args(["--voice", voice])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"Please leave the area\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{voice}: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let pcm: Vec<i16> = out
            .stdout
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        // About a second of speech at 8 kHz, and not silence.
        let secs = pcm.len() as f64 / 8000.0;
        assert!((0.5..5.0).contains(&secs), "{voice}: {secs:.2} s");
        let rms =
            (pcm.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt();
        assert!(rms > 300.0, "{voice}: rms {rms:.0}");
    }
}

#[test]
fn refuses_an_unknown_voice_without_reading_stdin() {
    let out = Command::new(env!("CARGO_BIN_EXE_nexus-tts"))
        .args(["--model-dir", "/nonexistent", "--voice", "xx"])
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown voice"));
}
