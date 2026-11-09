//! Float PCM at the model's rate down to the speaker path's 8 kHz s16.

/// Resample `pcm` (full scale ±1.0) from `from_hz` to 8 kHz with a
/// Hann-windowed sinc low-pass (16 taps a side, 3.6 kHz cutoff), so
/// nothing above the 4 kHz Nyquist folds back into the band.
pub fn resample_to_8k(pcm: &[f32], from_hz: u32) -> Vec<i16> {
    use std::f64::consts::PI;
    const TAPS: isize = 16;
    let step = f64::from(from_hz) / 8000.0;
    let fc = 3600.0 / f64::from(from_hz);
    let n_out = (pcm.len() as f64 / step) as usize;
    (0..n_out)
        .map(|n| {
            let t = n as f64 * step;
            let c = t.floor() as isize;
            let mut acc = 0.0;
            for k in (c - TAPS + 1)..=(c + TAPS) {
                let Some(&x) = usize::try_from(k).ok().and_then(|k| pcm.get(k)) else {
                    continue;
                };
                let d = t - k as f64;
                let arg = 2.0 * fc * d;
                let h = if arg == 0.0 {
                    2.0 * fc
                } else {
                    2.0 * fc * (PI * arg).sin() / (PI * arg)
                };
                acc += f64::from(x) * h * 0.5 * (1.0 + (PI * d / TAPS as f64).cos());
            }
            (acc * 32767.0).round().clamp(-32768.0, 32767.0) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The voices ship at 22.05 kHz (medium) and 16 kHz (x_low).
    const RATES: [u32; 2] = [22_050, 16_000];

    fn sine(freq: f64, from_hz: u32, secs: f64) -> Vec<f32> {
        let n = (f64::from(from_hz) * secs) as usize;
        (0..n)
            .map(|i| {
                (0.5 * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(from_hz)).sin())
                    as f32
            })
            .collect()
    }

    /// Gain in dB of a 0.5-amplitude sine after resampling, measured away
    /// from the edges where the filter runs out of input.
    fn gain_db(freq: f64, from_hz: u32) -> f64 {
        let out = resample_to_8k(&sine(freq, from_hz, 1.0), from_hz);
        let mid = &out[64..out.len() - 64];
        let rms =
            (mid.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / mid.len() as f64).sqrt();
        let expected = 0.5 * 32767.0 / std::f64::consts::SQRT_2;
        20.0 * (rms / expected).log10()
    }

    #[test]
    fn keeps_1khz_within_1db() {
        for rate in RATES {
            let g = gain_db(1000.0, rate);
            assert!(g.abs() <= 1.0, "{rate} Hz: 1 kHz gain {g:.2} dB");
        }
    }

    #[test]
    fn attenuates_5khz_by_30db() {
        for rate in RATES {
            let g = gain_db(5000.0, rate);
            assert!(g <= -30.0, "{rate} Hz: 5 kHz gain {g:.2} dB");
        }
    }

    #[test]
    fn band_edge_3900hz_is_cut_below_the_passband() {
        // Above the 3.6 kHz cutoff, so it must already be well down; it
        // sits just under Nyquist, so it must not come out louder either.
        for rate in RATES {
            let g = gain_db(3900.0, rate);
            assert!(g <= -6.0, "{rate} Hz: 3.9 kHz gain {g:.2} dB");
        }
    }

    #[test]
    fn output_length_follows_the_rate_ratio() {
        assert_eq!(resample_to_8k(&vec![0.0; 22_050], 22_050).len(), 8000);
        assert_eq!(resample_to_8k(&vec![0.0; 16_000], 16_000).len(), 8000);
        assert_eq!(resample_to_8k(&vec![0.0; 11_025], 22_050).len(), 4000);
        assert!(resample_to_8k(&[], 22_050).is_empty());
    }

    #[test]
    fn clamps_instead_of_wrapping() {
        let out = resample_to_8k(&vec![4.0; 1600], 16_000);
        assert!(out[100..700].iter().all(|&s| s == i16::MAX));
    }
}
