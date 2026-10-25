//! Talk-down: play audio through an IP speaker assigned to a camera (ADR-090).
//! `sip` is the only driver: a direct-IP SIP call streaming G.711 RTP.
// The admin talk-down endpoints wire this module up next.
#![allow(dead_code)]

pub mod g711;
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
    use super::sip::{options_ping, place_call, SipTarget};
    use super::*;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn tone_is_quiet_8khz_and_starts_and_ends_at_zero() {
        let pcm = tone_pcm(2000);
        assert_eq!(pcm.len(), 16_000);
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((6_000..=10_000).contains(&peak), "peak {peak}");
        assert_eq!((pcm[0], pcm[7_999], pcm[8_000], pcm[15_999]), (0, 0, 0, 0));
    }

    /// Plays the chime on a real speaker:
    /// `NEXUS_TALKDOWN_HW=192.168.1.66 cargo test -p nexus-engine talkdown_hw_tone -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a SIP speaker on the LAN: set NEXUS_TALKDOWN_HW=<ip>"]
    async fn talkdown_hw_tone() {
        let host = std::env::var("NEXUS_TALKDOWN_HW").expect("set NEXUS_TALKDOWN_HW");
        let ua = options_ping(&host, 5060, Duration::from_secs(3)).await;
        println!("OPTIONS -> final response, User-Agent {ua:?}");
        assert!(ua.is_some(), "speaker did not answer OPTIONS");

        let target = SipTarget {
            host,
            port: 5060,
            username: None,
            password: None,
        };
        let mut call = place_call(&target, Duration::from_secs(10))
            .await
            .expect("INVITE answered 2xx");
        println!(
            "INVITE -> 200 OK, codec {:?} (PT {})\n{call:?}",
            call.codec(),
            call.codec().payload_type()
        );
        call.play_pcm(&tone_pcm(2000), &CancellationToken::new())
            .await
            .expect("RTP sent");
        call.hangup().await.expect("BYE answered 2xx");
        println!("BYE -> 200 OK");
    }
}
