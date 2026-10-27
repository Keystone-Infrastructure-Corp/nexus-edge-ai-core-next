//! Canned talk-down clips: fetched by read SAS URL (hard rule 7), checked
//! against the caller's SHA-256, and decoded from 8 kHz mono 16-bit WAV. No
//! cache: every play fetches. The URL is a bearer credential, so it is never
//! logged and never put in an error.

use std::time::Duration;

use sha2::{Digest, Sha256};

/// Reading stops past this many bytes.
const MAX_BYTES: usize = 1 << 20;
// 5 s fetch + the 9 s call bound in session.rs stays inside the cloud's 15 s rpc timeout.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Fetch the clip at `url` and return its samples, or why it was refused.
/// `allow_http` exists for the tests' plain-http clip server only.
pub(super) async fn fetch(url: &str, sha256: &str, allow_http: bool) -> Result<Vec<i16>, String> {
    let url = url::Url::parse(url).map_err(|e| format!("clip url is invalid: {e}"))?;
    if !(url.scheme() == "https" || allow_http && url.scheme() == "http") {
        return Err("clip url must be https".into());
    }
    let failed = |e: reqwest::Error| format!("clip fetch failed: {}", e.without_url());
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        // A redirect could leave https.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(failed)?;
    let mut resp = client.get(url).send().await.map_err(failed)?;
    if !resp.status().is_success() {
        return Err(format!(
            "clip fetch answered HTTP {}",
            resp.status().as_u16()
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(failed)? {
        if body.len() + chunk.len() > MAX_BYTES {
            return Err("clip is larger than 1 MiB".into());
        }
        body.extend_from_slice(&chunk);
    }
    if !hex::encode(Sha256::digest(&body)).eq_ignore_ascii_case(sha256) {
        return Err("clip does not match its sha256".into());
    }
    parse_wav(&body).map_err(String::from)
}

/// The samples of a RIFF/WAVE file in PCM, mono, 8000 Hz, 16-bit. Chunks
/// other than `fmt ` and `data` are skipped.
fn parse_wav(file: &[u8]) -> Result<Vec<i16>, &'static str> {
    if file.len() < 12 || &file[..4] != b"RIFF" || &file[8..12] != b"WAVE" {
        return Err("clip is not a RIFF/WAVE file");
    }
    let mut rest = &file[12..];
    let mut fmt_ok = false;
    while rest.len() >= 8 {
        let (head, tail) = rest.split_at(8);
        let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let body = tail.get(..len).ok_or("clip has a truncated chunk")?;
        match &head[..4] {
            b"fmt " => {
                let field = |at: usize| {
                    body.get(at..at + 2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                };
                let rate = body
                    .get(4..8)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                // format tag, channels, sample rate, bits per sample
                if (field(0), field(2), rate, field(14)) != (Some(1), Some(1), Some(8000), Some(16))
                {
                    return Err("clip must be PCM, mono, 8000 Hz, 16-bit");
                }
                fmt_ok = true;
            }
            b"data" if fmt_ok => {
                let (samples, _) = body.as_chunks::<2>();
                return Ok(samples.iter().map(|&b| i16::from_le_bytes(b)).collect());
            }
            b"data" => return Err("clip has data before its format"),
            _ => {}
        }
        // Chunks are padded to an even length.
        rest = tail.get(len + len % 2..).unwrap_or_default();
    }
    Err("clip has no data chunk")
}
