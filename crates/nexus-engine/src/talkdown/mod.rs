//! Talk-down: play audio through an IP speaker assigned to a camera (ADR-091).
//! `sip` is the only driver: a direct-IP SIP call streaming G.711 RTP.
//! [`session`] runs the calls the admin endpoints in `crate::speakers_admin`
//! start, one per speaker; [`library`] holds the stored audio they play
//! ([`clip`] downloads it), [`responder`] plays a rule's audio when the rule
//! fires (ADR-095), and `live`
//! (built with `gstreamer-webrtc`) receives an operator's microphone for one.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use nexus_store::SpeakerOutput;

pub mod clip;
pub mod g711;
pub mod health;
pub mod library;
#[cfg(feature = "gstreamer-webrtc")]
pub mod live;
pub mod responder;
pub mod session;
pub mod sip;
pub mod tts;

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

/// 100 is unity, each step below is 0.4 dB (50 is -20 dB), 0 is silence.
pub fn level_gain(level: u8) -> f32 {
    match level.min(100) {
        0 => 0.0,
        v => 10f32.powf((f32::from(v) - 100.0) * 0.02),
    }
}

/// Soft limit above -1 dBFS so a boosted or loud frame compresses instead of wrapping.
pub fn limit(x: f32) -> i16 {
    const T: f32 = 0.891 * 32767.0;
    let a = x.abs();
    let y = if a <= T {
        a
    } else {
        T + (32767.0 - T) * ((a - T) / (32767.0 - T)).tanh()
    };
    y.copysign(x) as i16
}

/// A call's linear gain, shared with the session that can change it while the
/// call plays (f32 bits in an atomic: the play task reads it per frame).
#[derive(Debug, Clone)]
pub struct Gain(Arc<AtomicU32>);

impl Default for Gain {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl Gain {
    pub fn new(gain: f32) -> Self {
        Self(Arc::new(AtomicU32::new(gain.to_bits())))
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, gain: f32) {
        self.0.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// The speaker's level (unity when the speaker applies it itself) times the
    /// session's (a live slider or a rule's volume; 100 by default), +6 dB with
    /// `boost`.
    pub fn compose(speaker: &SpeakerOutput, session_volume: u8, boost: bool) -> f32 {
        level_gain(if speaker.hw_volume {
            100
        } else {
            speaker.volume
        }) * level_gain(session_volume)
            * if boost { 2.0 } else { 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_gain_is_unity_at_100_minus_20_db_at_50_and_silent_at_0() {
        assert_eq!(level_gain(100), 1.0);
        assert!((level_gain(50) - 0.1).abs() < 1e-6);
        assert_eq!(level_gain(0), 0.0);
        assert_eq!(level_gain(200), 1.0, "clamped");
        assert!((1..=100).all(|v| level_gain(v) > level_gain(v - 1)));
    }

    #[test]
    fn limit_is_identity_below_minus_1_dbfs_and_never_wraps() {
        for s in [0i32, 1, -1, 1000, -1000, 29_000, -29_000] {
            assert_eq!(i32::from(limit(s as f32)), s);
        }
        let mut last = i16::MIN;
        for x in (-200_000..=200_000).step_by(97) {
            let y = limit(x as f32);
            assert!(y >= last, "monotonic at {x}: {y} < {last}");
            last = y;
        }
        assert!(limit(1e9) > 0 && limit(-1e9) < 0, "no wrap at any input");
        assert!(limit(60_000.0) > 29_197 && limit(-60_000.0) < -29_197);
    }

    #[test]
    fn compose_multiplies_speaker_session_and_boost_and_skips_hw_volume() {
        let out = SpeakerOutput {
            volume: 50,
            boost: false,
            hw_volume: false,
            auto_max_per_hour: 6,
        };
        assert!((Gain::compose(&out, 100, false) - 0.1).abs() < 1e-6);
        assert!((Gain::compose(&out, 50, false) - 0.01).abs() < 1e-6);
        assert!((Gain::compose(&out, 100, true) - 0.2).abs() < 1e-6);
        let hw = SpeakerOutput {
            hw_volume: true,
            ..out
        };
        assert_eq!(Gain::compose(&hw, 100, false), 1.0);
        assert_eq!(Gain::compose(&hw, 100, true), 2.0);
        let g = Gain::default();
        let shared = g.clone();
        shared.set(0.25);
        assert_eq!(g.get(), 0.25);
    }

    #[test]
    fn tone_is_quiet_8khz_and_starts_and_ends_at_zero() {
        let pcm = tone_pcm(2000);
        assert_eq!(pcm.len(), 16_000);
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((6_000..=10_000).contains(&peak), "peak {peak}");
        assert_eq!((pcm[0], pcm[7_999], pcm[8_000], pcm[15_999]), (0, 0, 0, 0));
    }
}
