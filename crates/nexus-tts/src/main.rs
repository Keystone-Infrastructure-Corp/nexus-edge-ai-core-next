//! `nexus-tts` — the engine's text-to-speech helper process.
//!
//! Contract: `nexus-tts --model-dir <dir> --voice <id> [--speed <f>]`.
//! Reads UTF-8 text on stdin, writes raw s16le 8 kHz mono PCM on stdout,
//! exits 0 on success. Any failure exits non-zero with a reason on stderr
//! (the engine ignores stderr). The text never comes through argv.
//!
//! Layout under `--model-dir` (the model pack's `tts/` directory):
//! `<voice>/model.onnx`, `<voice>/tokens.txt` and a shared
//! `espeak-ng-data/` used to turn text into phonemes.
//!
//! It runs as its own process because sherpa-onnx links its own
//! onnxruntime, which must not share the engine's address space.

mod resample;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

/// Voice ids this helper accepts. Each names a directory under
/// `--model-dir`; the model pack's manifest carries the label and
/// language the engine shows.
const VOICES: &[&str] = &["en-US-female", "en-US-male", "es-ES-male"];

/// Stdin cap. The engine's own text limit is far lower; this only stops
/// a runaway writer from growing the helper without bound.
const MAX_INPUT_BYTES: u64 = 16 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "nexus-tts",
    version,
    about = "Render stdin text to 8 kHz s16le on stdout"
)]
struct Args {
    /// The model pack's `tts/` directory.
    #[arg(long)]
    model_dir: PathBuf,
    /// One of the built-in voice ids.
    #[arg(long)]
    voice: String,
    /// Speaking rate; 1.0 is the voice's natural pace.
    #[arg(long, default_value_t = 1.0)]
    speed: f32,
}

/// Files one render needs, checked before any model loads.
#[derive(Debug)]
struct VoicePaths {
    model: PathBuf,
    tokens: PathBuf,
    espeak_data: PathBuf,
}

fn resolve(model_dir: &Path, voice: &str, speed: f32) -> Result<VoicePaths, String> {
    if !VOICES.contains(&voice) {
        return Err(format!("unknown voice '{voice}'"));
    }
    if !(0.5..=2.0).contains(&speed) {
        return Err(format!("speed {speed} is outside 0.5..=2.0"));
    }
    let dir = model_dir.join(voice);
    let paths = VoicePaths {
        model: dir.join("model.onnx"),
        tokens: dir.join("tokens.txt"),
        espeak_data: model_dir.join("espeak-ng-data"),
    };
    for file in [&paths.model, &paths.tokens] {
        if !file.is_file() {
            return Err(format!("voice file missing: {}", file.display()));
        }
    }
    if !paths.espeak_data.is_dir() {
        return Err(format!(
            "phoneme data missing: {}",
            paths.espeak_data.display()
        ));
    }
    Ok(paths)
}

fn read_text() -> Result<String, String> {
    let mut buf = String::new();
    std::io::stdin()
        .take(MAX_INPUT_BYTES + 1)
        .read_to_string(&mut buf)
        .map_err(|e| format!("stdin is not UTF-8 text: {e}"))?;
    if buf.len() as u64 > MAX_INPUT_BYTES {
        return Err(format!("input is over {MAX_INPUT_BYTES} bytes"));
    }
    let text = buf.trim();
    if text.is_empty() {
        return Err("no text on stdin".into());
    }
    Ok(text.to_owned())
}

/// Text → float PCM at the voice's own sample rate.
#[cfg(feature = "sherpa")]
fn synthesize(paths: &VoicePaths, text: &str, speed: f32) -> Result<(Vec<f32>, u32), String> {
    use piper_phonemize_sys as pp;
    use sherpa_onnx::{
        GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsModelConfig,
        OfflineTtsVitsModelConfig,
    };
    use std::ffi::CString;

    let utf8 = |p: &Path| {
        p.to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("path is not UTF-8: {}", p.display()))
    };
    let config = OfflineTtsConfig {
        model: OfflineTtsModelConfig {
            vits: OfflineTtsVitsModelConfig {
                model: Some(utf8(&paths.model)?),
                tokens: Some(utf8(&paths.tokens)?),
                ..Default::default()
            },
            // shortcut: fixed at 2 threads, measured only on a dev Mac; retune with the per-profile p95 run.
            num_threads: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    let tts = OfflineTts::create(&config).ok_or("the voice model did not load")?;
    // The espeak-ng voice the model was trained with, from its metadata.
    let lang = tts.lang();
    if lang.is_empty() {
        return Err("the voice model names no phonemizer language".into());
    }

    let data = CString::new(utf8(&paths.espeak_data)?).map_err(|e| e.to_string())?;
    let c_text = CString::new(text).map_err(|_| "text contains a NUL byte".to_string())?;
    let c_lang = CString::new(lang).map_err(|e| e.to_string())?;
    // SAFETY: the C API copies what it needs from these NUL-terminated
    // strings; the result handle is read only within its bounds and freed once.
    let sentences = unsafe {
        if pp::PiperPhonemizeInitialize(data.as_ptr()) < 0 {
            return Err("phoneme data did not load".into());
        }
        let result = pp::PiperPhonemizeText(c_text.as_ptr(), c_lang.as_ptr());
        if result.is_null() {
            return Err("text could not be phonemised".into());
        }
        let mut sentences = Vec::new();
        for i in 0..pp::PiperPhonemizeResultGetNumSentences(result) {
            let n = pp::PiperPhonemizeResultGetNumPhonemes(result, i);
            let p = pp::PiperPhonemizeResultGetPhonemes(result, i);
            if n > 0 && !p.is_null() {
                sentences.push(std::slice::from_raw_parts(p, n as usize).to_vec());
            }
        }
        pp::PiperPhonemizeDestroyResult(result.cast_mut());
        sentences
    };
    if sentences.is_empty() {
        return Err("the text has nothing to say".into());
    }

    let gen = GenerationConfig {
        speed,
        phoneme_codepoints: Some(sentences),
        ..Default::default()
    };
    let audio = tts
        .generate_with_config("", &gen, None::<fn(&[f32], f32) -> bool>)
        .ok_or("synthesis failed")?;
    let rate =
        u32::try_from(audio.sample_rate()).map_err(|_| "the voice reported a bad sample rate")?;
    Ok((audio.samples().to_vec(), rate))
}

#[cfg(not(feature = "sherpa"))]
fn synthesize(_: &VoicePaths, _: &str, _: f32) -> Result<(Vec<f32>, u32), String> {
    Err("this nexus-tts was built without the `sherpa` feature".into())
}

fn run(args: &Args) -> Result<(), String> {
    let paths = resolve(&args.model_dir, &args.voice, args.speed)?;
    let text = read_text()?;
    let (pcm, rate) = synthesize(&paths, &text, args.speed)?;
    let bytes: Vec<u8> = resample::resample_to_8k(&pcm, rate)
        .into_iter()
        .flat_map(i16::to_le_bytes)
        .collect();
    let mut out = std::io::stdout().lock();
    out.write_all(&bytes)
        .and_then(|()| out.flush())
        .map_err(|e| format!("stdout: {e}"))
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "nexus-tts: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice_dir(root: &Path, voice: &str) {
        let dir = root.join(voice);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model.onnx"), b"x").unwrap();
        std::fs::write(dir.join("tokens.txt"), b"x").unwrap();
        std::fs::create_dir_all(root.join("espeak-ng-data")).unwrap();
    }

    #[test]
    fn refuses_an_unknown_voice_before_touching_the_disk() {
        let err = resolve(Path::new("/nonexistent"), "../../etc", 1.0).unwrap_err();
        assert!(err.contains("unknown voice"), "{err}");
    }

    #[test]
    fn refuses_a_missing_model_dir() {
        let err = resolve(Path::new("/nonexistent/tts"), "en-US-female", 1.0).unwrap_err();
        assert!(err.contains("voice file missing"), "{err}");
    }

    #[test]
    fn refuses_missing_phoneme_data_and_bad_speed() {
        let tmp = tempfile::tempdir().unwrap();
        voice_dir(tmp.path(), "en-US-male");
        std::fs::remove_dir(tmp.path().join("espeak-ng-data")).unwrap();
        let err = resolve(tmp.path(), "en-US-male", 1.0).unwrap_err();
        assert!(err.contains("phoneme data missing"), "{err}");
        let err = resolve(tmp.path(), "en-US-male", 9.0).unwrap_err();
        assert!(err.contains("speed"), "{err}");
    }

    #[test]
    fn resolves_every_listed_voice_when_its_files_exist() {
        let tmp = tempfile::tempdir().unwrap();
        for voice in VOICES {
            voice_dir(tmp.path(), voice);
            let paths = resolve(tmp.path(), voice, 1.0).unwrap();
            assert_eq!(paths.model, tmp.path().join(voice).join("model.onnx"));
        }
    }

    #[test]
    fn the_cli_takes_the_contract_flags() {
        let args = Args::try_parse_from([
            "nexus-tts",
            "--model-dir",
            "/m",
            "--voice",
            "es-ES-male",
            "--speed",
            "1.2",
        ])
        .unwrap();
        assert_eq!(
            (args.model_dir, args.voice.as_str(), args.speed),
            (PathBuf::from("/m"), "es-ES-male", 1.2)
        );
        assert!(Args::try_parse_from(["nexus-tts", "--voice", "en-US-male"]).is_err());
    }
}
