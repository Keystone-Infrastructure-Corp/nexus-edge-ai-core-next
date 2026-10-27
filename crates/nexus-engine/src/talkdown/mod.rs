//! Talk-down: play audio through an IP speaker assigned to a camera (ADR-091).
//! `sip` is the only driver: a direct-IP SIP call streaming G.711 RTP.
//! [`session`] runs the calls the admin endpoints in `crate::speakers_admin`
//! start, one per speaker; [`clip`] loads a canned clip for one, and `live`
//! (built with `gstreamer-webrtc`) receives an operator's microphone for one.

pub mod clip;
pub mod g711;
#[cfg(feature = "gstreamer-webrtc")]
pub mod live;
pub mod session;
pub mod sip;

/// A polite two-note chime (E5 then C5, each fading out) at about -10 dBFS,
/// 8 kHz mono, `ms` long.
pub fn tone_pcm(ms: u32) -> Vec<i16> {
    let n = ms as usize * 8;
    let half = n / 2;
    (0..n)
        .map(|i| {
            let (hz, j, len) = if i < half {
                (659.25, i, half)
            } else {
                (523.25, i - half, n - half)
            };
            let t = j as f32 / 8000.0;
            // 10 ms ramps at both ends of each note so the edges do not click.
            let ramp = (j.min(len - 1 - j) as f32 / 80.0).min(1.0);
            let level = 0.3 * f32::from(i16::MAX) * ramp * (-3.0 * t).exp();
            (level * (std::f32::consts::TAU * hz * t).sin()) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_is_quiet_8khz_and_starts_and_ends_at_zero() {
        let pcm = tone_pcm(2000);
        assert_eq!(pcm.len(), 16_000);
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((6_000..=10_000).contains(&peak), "peak {peak}");
        assert_eq!((pcm[0], pcm[7_999], pcm[8_000], pcm[15_999]), (0, 0, 0, 0));
    }
}
