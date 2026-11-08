//! Text-to-speech for talk-down (SPEC-086, D4): the `nexus-tts` helper
//! process renders text to 8 kHz PCM. It runs apart from the engine because
//! sherpa-onnx links its own onnxruntime, and a crash or a runaway render
//! must not reach the pipeline. The helper gets a fixed argv, no shell, a
//! cleared environment, a voice id only from the discovered table, the text
//! on stdin, an output cap, a timeout and a low priority. Text never goes
//! into a log or a tracing span.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use super::library::normalize;

/// Typed speech (`{"mode":"tts"}`) length.
// shortcut: placeholder from the T10a spike; re-measure p95 on the slowest profile before release.
pub const ADHOC_MAX_CHARS: usize = 120;
/// A library item's text, the cloud schema's cap.
pub const LIBRARY_MAX_CHARS: usize = 500;
/// Typed speech must render inside the cloud's 15 s RPC beside the 8 s answer wait.
pub const ADHOC_TIMEOUT: Duration = Duration::from_secs(5);
// shortcut: placeholder from the T10a spike; re-measure 500 characters on the slowest profile.
pub const LIBRARY_TIMEOUT: Duration = Duration::from_secs(60);
/// A preview renders then uploads inside the cloud's 15 s RPC.
// shortcut: placeholder, not measured; tie it to the spike's 500-character p95.
pub const PREVIEW_TIMEOUT: Duration = Duration::from_secs(10);
/// 60 s of 8 kHz s16le.
const MAX_RAW_BYTES: usize = 960_000;
const NICE_BIN: &str = "/usr/bin/nice";
// shortcut: placeholder niceness; measure render time under full inference and tune.
const NICE: &str = "10";
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether startup found the helper and a voice: the heartbeat's
/// `talkdown_tts` cap. Written once, by `main` only, so tests never race it.
pub static AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A voice the model pack ships and the helper can load.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Voice {
    pub id: String,
    pub label: String,
    pub language: String,
}

/// The helper and its voices. One render runs at a time, across typed
/// speech, library renders and previews.
#[derive(Debug)]
pub struct Tts {
    bin: PathBuf,
    model_dir: PathBuf,
    voices: Vec<Voice>,
    one: Mutex<()>,
}

/// Trim, map control characters (newlines included) to spaces, and refuse
/// empty text or text over `max_chars`.
pub fn validate_text(text: &str, max_chars: usize) -> Result<String, String> {
    let mapped: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let text = mapped.trim();
    if text.is_empty() {
        return Err("text is empty".into());
    }
    if text.chars().count() > max_chars {
        return Err(format!("text is over {max_chars} characters"));
    }
    Ok(text.to_string())
}

/// A voice id becomes a helper argument and a directory name.
fn plain_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Tts {
    pub fn new(bin: PathBuf, model_dir: PathBuf, voices: Vec<Voice>) -> Self {
        Self {
            bin,
            model_dir,
            voices,
            one: Mutex::new(()),
        }
    }

    /// `<bin_dir>/nexus-tts` with the voices of `<models_dir>/models-manifest.json`
    /// whose files are all present, plus the shared non-voice `tts` entries
    /// (phoneme data). `None`, logged once at info, without the helper, `nice` or a voice.
    pub fn discover(bin_dir: &Path, models_dir: &Path) -> Option<Self> {
        Self::discover_with(bin_dir, models_dir, Path::new(NICE_BIN))
    }

    fn discover_with(bin_dir: &Path, models_dir: &Path, nice: &Path) -> Option<Self> {
        let bin = bin_dir.join("nexus-tts");
        if !bin.is_file() || !nice.is_file() {
            tracing::info!("text-to-speech is off: no nexus-tts helper or no nice");
            return None;
        }
        let manifest: serde_json::Value = std::fs::read(models_dir.join("models-manifest.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut voices = Vec::new();
        let mut shared = true;
        let entries = manifest["models"].as_array().into_iter().flatten();
        for entry in entries.filter(|e| e["task"] == "tts") {
            let present = entry["artifacts"].as_array().is_some_and(|a| {
                !a.is_empty()
                    && a.iter().all(|a| {
                        a["path"]
                            .as_str()
                            .is_some_and(|p| models_dir.join(p).is_file())
                    })
            });
            match serde_json::from_value::<Voice>(entry["voice"].clone()) {
                Ok(v) if present && plain_id(&v.id) => voices.push(v),
                Ok(_) => {}
                Err(_) => shared &= present,
            }
        }
        if !shared || voices.is_empty() {
            tracing::info!("text-to-speech is off: the model pack has no complete voice");
            return None;
        }
        Some(Self::new(bin, models_dir.join("tts"), voices))
    }

    pub fn voices(&self) -> &[Voice] {
        &self.voices
    }

    /// The voice `id` names, or the first voice when it names none.
    pub fn voice(&self, id: Option<&str>) -> Option<&Voice> {
        match id {
            Some(id) => self.voices.iter().find(|v| v.id == id),
            None => self.voices.first(),
        }
    }

    /// Render already validated `text` in `voice_id`, normalised. An unknown
    /// voice is refused before anything spawns; `timeout` includes the wait
    /// for a render already running.
    pub async fn speak(
        &self,
        voice_id: &str,
        text: &str,
        timeout: Duration,
    ) -> Result<Vec<i16>, String> {
        let voice = self
            .voice(Some(voice_id))
            .ok_or_else(|| format!("unknown voice {voice_id:?}"))?;
        let mut pcm = tokio::time::timeout(timeout, self.render(&voice.id, text))
            .await
            .map_err(|_| "text-to-speech timed out".to_string())??;
        normalize(&mut pcm);
        Ok(pcm)
    }

    async fn render(&self, voice_id: &str, text: &str) -> Result<Vec<i16>, String> {
        let _one = self.one.lock().await;
        let mut child = tokio::process::Command::new(NICE_BIN)
            .args(["-n", NICE])
            .arg(&self.bin)
            .arg("--model-dir")
            .arg(&self.model_dir)
            .arg("--voice")
            .arg(voice_id)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("text-to-speech did not start: {e}"))?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err("text-to-speech has no pipes".into());
        };
        // Write and read together, so a helper that answers before it has
        // read everything cannot stall on a full pipe.
        let write = async move {
            stdin.write_all(text.as_bytes()).await?;
            stdin.write_all(b"\n").await
            // `stdin` drops here: the helper sees end of input.
        };
        let mut raw = Vec::new();
        let mut capped = stdout.take(MAX_RAW_BYTES as u64 + 1);
        let read = capped.read_to_end(&mut raw);
        let (written, read) = tokio::join!(write, read);
        read.map_err(|e| format!("text-to-speech output unreadable: {e}"))?;
        if raw.len() > MAX_RAW_BYTES {
            // `child` drops here and is killed.
            return Err("text-to-speech output is over 60 s".into());
        }
        let status = child
            .wait()
            .await
            .map_err(|e| format!("text-to-speech did not finish: {e}"))?;
        if !status.success() {
            return Err(format!("text-to-speech failed ({status})"));
        }
        written.map_err(|e| format!("text-to-speech input not written: {e}"))?;
        let pcm: Vec<i16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        if pcm.is_empty() {
            return Err("text-to-speech rendered no audio".into());
        }
        Ok(pcm)
    }
}

/// `pcm` as a RIFF/WAVE file: PCM, mono, 8000 Hz, 16-bit.
pub fn wav(pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    for field in [16u32, 1 | 1 << 16, 8000, 16_000, 2 | 16 << 16] {
        out.extend_from_slice(&field.to_le_bytes());
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
    out
}

/// PUT `body` (a WAV) to the write SAS `url`. The URL is a bearer
/// credential: never logged and never in an error. `allow_http` exists for
/// the tests' plain-http stub only.
pub async fn upload(url: &str, body: Vec<u8>, allow_http: bool) -> Result<(), String> {
    let url = url::Url::parse(url).map_err(|_| "upload_url is not a URL".to_string())?;
    if !(url.scheme() == "https" || allow_http && url.scheme() == "http") {
        return Err("upload_url must be https".into());
    }
    // A blob host is a name; an IP literal could aim the upload at the LAN.
    if matches!(url.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_))) {
        return Err("upload_url must name a host, not an IP address".into());
    }
    let failed = |e: reqwest::Error| format!("preview upload failed: {}", e.without_url());
    let client = reqwest::Client::builder()
        .timeout(UPLOAD_TIMEOUT)
        // A redirect could leave https or reach the LAN.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(failed)?;
    let resp = client
        .put(url)
        .header("x-ms-blob-type", "BlockBlob")
        .header(reqwest::header::CONTENT_TYPE, "audio/wav")
        .body(body)
        .send()
        .await
        .map_err(failed)?;
    if !resp.status().is_success() {
        return Err(format!(
            "preview upload answered HTTP {}",
            resp.status().as_u16()
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A [`Tts`] over `fake-tts.sh`, whose voice id picks its behaviour.
    pub(crate) fn fake(voices: &[&str]) -> Tts {
        let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-tts.sh");
        let voices = voices
            .iter()
            .map(|id| Voice {
                id: (*id).into(),
                label: (*id).into(),
                language: "en-US".into(),
            })
            .collect();
        Tts::new(bin, PathBuf::from("/nonexistent"), voices)
    }

    #[test]
    fn validate_text_trims_maps_control_characters_and_enforces_the_limit() {
        assert_eq!(
            validate_text("  Please\nleave\tnow\r\n ", 120).unwrap(),
            "Please leave now"
        );
        assert_eq!(validate_text("a\u{7}b", 120).unwrap(), "a b");
        assert!(validate_text("", 120).is_err());
        assert!(validate_text(" \n\t ", 120).is_err());
        assert!(validate_text(&"x".repeat(121), 120).is_err());
        assert_eq!(
            validate_text(&"é".repeat(120), 120)
                .unwrap()
                .chars()
                .count(),
            120
        );
    }

    #[tokio::test]
    async fn speak_returns_the_helpers_pcm_normalised() {
        let tts = fake(&["ok"]);
        let pcm = tts
            .speak("ok", "Please leave the area", ADHOC_TIMEOUT)
            .await
            .unwrap();
        // 21 characters and the newline, 800 samples each.
        assert_eq!(pcm.len(), 22 * 800);
        assert!(pcm.iter().all(|&s| s == pcm[0] && s != 0));
        // A constant brought to -20 dBFS RMS.
        assert!((i32::from(pcm[0]) - 3277).abs() <= 1, "{}", pcm[0]);
    }

    #[tokio::test]
    async fn speak_refuses_an_unknown_voice_before_spawning() {
        // A helper that does not exist: only a spawn could fail differently.
        let mut tts = fake(&["ok"]);
        tts.bin = PathBuf::from("/nonexistent/nexus-tts");
        let err = tts.speak("--speed", "hi", ADHOC_TIMEOUT).await.unwrap_err();
        assert!(err.starts_with("unknown voice"), "{err}");
        let err = tts.speak("ok", "hi", ADHOC_TIMEOUT).await.unwrap_err();
        assert!(!err.starts_with("unknown voice"), "{err}");
    }

    #[tokio::test]
    async fn speak_kills_a_helper_at_the_timeout() {
        let tts = fake(&["sleep"]);
        let start = std::time::Instant::now();
        let err = tts
            .speak("sleep", "hi", Duration::from_millis(300))
            .await
            .unwrap_err();
        assert_eq!(err, "text-to-speech timed out");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn speak_refuses_output_over_the_cap_and_a_failed_exit() {
        let tts = fake(&["big", "fail"]);
        let err = tts.speak("big", "hi", ADHOC_TIMEOUT).await.unwrap_err();
        assert_eq!(err, "text-to-speech output is over 60 s");
        let err = tts.speak("fail", "hi", ADHOC_TIMEOUT).await.unwrap_err();
        assert!(err.starts_with("text-to-speech failed"), "{err}");
        assert!(!err.contains("hi"), "no text in errors: {err}");
    }

    #[tokio::test]
    async fn renders_run_one_at_a_time() {
        let tts = fake(&["slow"]);
        let start = std::time::Instant::now();
        let (a, b) = tokio::join!(
            tts.speak("slow", "a", ADHOC_TIMEOUT),
            tts.speak("slow", "b", ADHOC_TIMEOUT)
        );
        a.unwrap();
        b.unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(600),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn discover_keeps_only_voices_whose_files_are_present() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, models) = (dir.path().join("bin"), dir.path().join("models"));
        assert!(Tts::discover(&bin, &models).is_none(), "no helper");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("nexus-tts"), "").unwrap();
        assert!(Tts::discover(&bin, &models).is_none(), "no manifest");

        let entry = |id: &str, voice: serde_json::Value| {
            serde_json::json!({ "id": format!("tts-{id}"), "task": "tts", "voice": voice,
                "artifacts": [{ "path": format!("tts/{id}/model.onnx") },
                              { "path": format!("tts/{id}/tokens.txt") }] })
        };
        let voice = |id: &str| serde_json::json!({ "id": id, "label": id, "language": "en-US" });
        let manifest = serde_json::json!({ "models": [
            { "id": "yolo", "task": "detect", "artifacts": [{ "path": "yolo.onnx" }] },
            entry("en-US-female", voice("en-US-female")),
            entry("en-US-male", voice("en-US-male")),
            entry("--speed", voice("--speed")),
            { "id": "tts-espeak-ng-data", "task": "tts",
              "artifacts": [{ "path": "tts/espeak-ng-data/phontab" }] },
        ]});
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("models-manifest.json"), manifest.to_string()).unwrap();
        let touch = |p: &str| {
            let p = models.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        };
        for p in [
            "tts/en-US-female/model.onnx",
            "tts/en-US-female/tokens.txt",
            "tts/en-US-male/model.onnx",
            "tts/--speed/model.onnx",
            "tts/--speed/tokens.txt",
        ] {
            touch(p);
        }
        assert!(
            Tts::discover(&bin, &models).is_none(),
            "no phoneme data, no voice works"
        );
        touch("tts/espeak-ng-data/phontab");
        let tts = Tts::discover(&bin, &models).unwrap();
        let ids: Vec<&str> = tts.voices().iter().map(|v| v.id.as_str()).collect();
        assert_eq!(
            ids,
            ["en-US-female"],
            "male lacks tokens; a flag-like id is refused"
        );
        assert_eq!(tts.model_dir, models.join("tts"));
        assert_eq!(tts.voice(None).unwrap().id, "en-US-female");
        assert!(tts.voice(Some("es-ES-male")).is_none());
        let no_nice = dir.path().join("no-nice");
        assert!(
            Tts::discover_with(&bin, &models, &no_nice).is_none(),
            "no nice, every render would fail"
        );
    }

    /// One real render through the built helper and a voice pack:
    /// `NEXUS_TTS_BIN=target/debug/nexus-tts NEXUS_TTS_MODEL_DIR=<pack>/tts
    /// cargo test -p nexus-engine real_render -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs nexus-tts built with --features sherpa and a voice pack"]
    async fn real_render() {
        let var = |k| PathBuf::from(std::env::var(k).unwrap_or_else(|_| panic!("{k}")));
        let voice = Voice {
            id: "en-US-female".into(),
            label: "English (US), female".into(),
            language: "en-US".into(),
        };
        let tts = Tts::new(
            var("NEXUS_TTS_BIN"),
            var("NEXUS_TTS_MODEL_DIR"),
            vec![voice],
        );
        let start = std::time::Instant::now();
        let pcm = tts
            .speak("en-US-female", "Please leave the area", LIBRARY_TIMEOUT)
            .await
            .unwrap();
        println!("{} samples in {:?}", pcm.len(), start.elapsed());
        assert!(pcm.len() > 8000, "at least a second of speech");
        assert!(pcm.iter().any(|&s| s.unsigned_abs() > 1000), "not silent");
    }

    #[test]
    fn wav_is_a_pcm_mono_8khz_16_bit_file() {
        let file = wav(&[1, -2, 3]);
        assert_eq!(file.len(), 44 + 6);
        assert_eq!(&file[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(file[4..8].try_into().unwrap()), 42);
        assert_eq!(&file[8..16], b"WAVEfmt ");
        assert_eq!(&file[20..24], &[1, 0, 1, 0]);
        assert_eq!(u32::from_le_bytes(file[24..28].try_into().unwrap()), 8000);
        assert_eq!(u32::from_le_bytes(file[28..32].try_into().unwrap()), 16_000);
        assert_eq!(&file[32..36], &[2, 0, 16, 0]);
        assert_eq!(&file[36..40], b"data");
        assert_eq!(&file[44..], &[1, 0, 0xfe, 0xff, 3, 0]);
    }

    #[tokio::test]
    async fn upload_puts_the_wav_and_refuses_ip_hosts_http_and_redirects() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/ok.wav"))
            .and(header("x-ms-blob-type", "BlockBlob"))
            .and(header("content-type", "audio/wav"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(path("/moved.wav"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/ok.wav", server.uri())),
            )
            .mount(&server)
            .await;
        let base = server.uri().replace("127.0.0.1", "localhost");
        let url = |p: &str| format!("{base}{p}?sv=1&sig=SAS-SIGNATURE");
        let body = wav(&[1, 2, 3]);

        upload(&url("/ok.wav"), body.clone(), true).await.unwrap();
        let got = server.received_requests().await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, body);

        let ip = format!("{}/ok.wav?sig=SAS-SIGNATURE", server.uri());
        for (url, allow_http) in [
            (url("/moved.wav"), true),
            (ip, true),
            (url("/ok.wav"), false),
            ("https://[::1]/x?sig=SAS-SIGNATURE".into(), false),
            ("not a url SAS-SIGNATURE".into(), false),
        ] {
            let err = upload(&url, body.clone(), allow_http).await.unwrap_err();
            assert!(
                !err.contains("SAS-SIGNATURE") && !err.contains("localhost"),
                "{err}"
            );
        }
        // The redirect was not followed: still one PUT to /ok.wav.
        let got = server.received_requests().await.unwrap();
        assert_eq!(got.iter().filter(|r| r.url.path() == "/ok.wav").count(), 1);
    }
}
