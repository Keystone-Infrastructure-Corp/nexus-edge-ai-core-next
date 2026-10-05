//! Parameter-set re-injection for cameras that send SPS/PPS only in
//! the SDP (#358).
//!
//! The pre-roll ingester runs `h26Xparse config-interval=0`, so it
//! passes the camera's byte-stream through unchanged (see the
//! `preroll_ingester` module doc for why `-1` breaks recording). For a
//! camera that never repeats its parameter sets in-band, the only
//! SPS/PPS a session ever carries is the copy `rtph26Xdepay` injects
//! from `sprop-parameter-sets` at session start, so every IDR after
//! the first GOP arrives bare and every clip that starts there is
//! undecodable.
//!
//! [`ParamSetCache`] remembers the most recent VPS/SPS/PPS seen on the
//! session (which includes the depayloader's sprop copy) and prepends
//! them to an IDR access unit that carries none. An access unit that
//! already carries any parameter set is never touched, so cameras that
//! send them per keyframe produce byte-identical output.

/// Per-session cache of the most recent parameter-set NAL units, stored
/// without their start codes.
#[derive(Debug, Default)]
pub struct ParamSetCache {
    hevc: bool,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NalKind {
    Aud,
    Vps,
    Sps,
    Pps,
    Idr,
    Other,
}

impl ParamSetCache {
    /// `hevc` selects H.265 NAL header parsing; otherwise H.264.
    pub fn new(hevc: bool) -> Self {
        Self {
            hevc,
            ..Self::default()
        }
    }

    /// Feed one Annex-B access unit. Returns `None` when the AU must
    /// pass through unchanged, or `Some(bytes)` with the cached
    /// parameter sets inserted ahead of the AU's first non-AUD NAL.
    pub fn process(&mut self, au: &[u8]) -> Option<Vec<u8>> {
        let nals = split_nals(au);
        let mut has_params = false;
        let mut has_idr = false;
        for &(_, start, end) in &nals {
            let kind = self.kind(au[start]);
            let slot = match kind {
                NalKind::Vps => &mut self.vps,
                NalKind::Sps => &mut self.sps,
                NalKind::Pps => &mut self.pps,
                NalKind::Idr => {
                    has_idr = true;
                    continue;
                }
                _ => continue,
            };
            has_params = true;
            *slot = Some(au[start..end].to_vec());
        }
        if has_params || !has_idr {
            return None;
        }
        let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else {
            return None;
        };
        let vps = if self.hevc {
            Some(self.vps.as_ref()?)
        } else {
            None
        };
        // Parameter sets follow an access-unit delimiter if there is one.
        let insert_at = match nals.first() {
            Some(&(_, start, _)) if self.kind(au[start]) == NalKind::Aud => {
                nals.get(1).map(|&(sc, _, _)| sc).unwrap_or(au.len())
            }
            _ => 0,
        };
        let mut out = Vec::with_capacity(au.len() + 64);
        out.extend_from_slice(&au[..insert_at]);
        for ps in vps.into_iter().chain([sps, pps]) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(ps);
        }
        out.extend_from_slice(&au[insert_at..]);
        Some(out)
    }

    fn kind(&self, header: u8) -> NalKind {
        if self.hevc {
            match (header >> 1) & 0x3f {
                35 => NalKind::Aud,
                32 => NalKind::Vps,
                33 => NalKind::Sps,
                34 => NalKind::Pps,
                // IRAP pictures (BLA, IDR, CRA) need the parameter sets.
                16..=21 => NalKind::Idr,
                _ => NalKind::Other,
            }
        } else {
            match header & 0x1f {
                9 => NalKind::Aud,
                7 => NalKind::Sps,
                8 => NalKind::Pps,
                5 => NalKind::Idr,
                _ => NalKind::Other,
            }
        }
    }
}

/// Split an Annex-B buffer into `(start_code_offset, payload_start,
/// payload_end)` triples. Empty NAL units are skipped.
fn split_nals(data: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let sc = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((sc, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &(sc, start)) in starts.iter().enumerate() {
        let end = starts
            .get(n + 1)
            .map(|&(next, _)| next)
            .unwrap_or(data.len());
        if start < end {
            out.push((sc, start, end));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC: [u8; 4] = [0, 0, 0, 1];

    fn au(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| SC.iter().chain(n.iter()))
            .copied()
            .collect()
    }

    // H.264 headers: SPS 0x67, PPS 0x68, IDR 0x65, non-IDR 0x41, AUD 0x09.
    const SPS: &[u8] = &[0x67, 0x42, 0x00, 0x1f];
    const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00];
    const P: &[u8] = &[0x41, 0x9a, 0x02];
    const AUD: &[u8] = &[0x09, 0xf0];

    #[test]
    fn au_that_carries_params_passes_through_unchanged() {
        let mut cache = ParamSetCache::new(false);
        assert_eq!(cache.process(&au(&[SPS, PPS, IDR])), None);
        // A camera that sends them with every IDR is never touched.
        assert_eq!(cache.process(&au(&[P])), None);
        assert_eq!(cache.process(&au(&[AUD, SPS, PPS, IDR])), None);
    }

    #[test]
    fn bare_idr_gets_the_cached_params_prepended() {
        let mut cache = ParamSetCache::new(false);
        // Session start: the depayloader's sprop copy.
        assert_eq!(cache.process(&au(&[SPS, PPS, IDR])), None);
        assert_eq!(cache.process(&au(&[P])), None);
        // Second GOP: the camera sends the IDR bare.
        assert_eq!(cache.process(&au(&[IDR])), Some(au(&[SPS, PPS, IDR])));
    }

    #[test]
    fn params_go_after_an_access_unit_delimiter() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS, PPS, IDR]));
        assert_eq!(
            cache.process(&au(&[AUD, IDR])),
            Some(au(&[AUD, SPS, PPS, IDR]))
        );
    }

    #[test]
    fn non_idr_is_unchanged() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS, PPS, IDR]));
        assert_eq!(cache.process(&au(&[P])), None);
    }

    #[test]
    fn bare_idr_with_nothing_cached_is_unchanged() {
        let mut cache = ParamSetCache::new(false);
        assert_eq!(cache.process(&au(&[IDR])), None);
    }

    #[test]
    fn three_byte_start_codes_are_recognised() {
        let mut cache = ParamSetCache::new(false);
        let mut first = vec![0, 0, 1];
        first.extend_from_slice(SPS);
        first.extend_from_slice(&[0, 0, 1]);
        first.extend_from_slice(PPS);
        first.extend_from_slice(&[0, 0, 1]);
        first.extend_from_slice(IDR);
        assert_eq!(cache.process(&first), None);
        let mut bare = vec![0, 0, 1];
        bare.extend_from_slice(IDR);
        let mut want = au(&[SPS, PPS]);
        want.extend_from_slice(&bare);
        assert_eq!(cache.process(&bare), Some(want));
    }

    #[test]
    fn latest_params_win() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS, PPS, IDR]));
        let sps2: &[u8] = &[0x67, 0x64, 0x00, 0x28];
        cache.process(&au(&[sps2, PPS, IDR]));
        assert_eq!(cache.process(&au(&[IDR])), Some(au(&[sps2, PPS, IDR])));
    }

    // H.265 headers (two bytes): type << 1 in the first byte.
    const HVPS: &[u8] = &[0x40, 0x01, 0x0c];
    const HSPS: &[u8] = &[0x42, 0x01, 0x01];
    const HPPS: &[u8] = &[0x44, 0x01, 0xc1];
    const HIDR: &[u8] = &[0x26, 0x01, 0xaf]; // IDR_W_RADL (19)
    const HTRAIL: &[u8] = &[0x02, 0x01, 0xd0]; // TRAIL_R (1)
    const HAUD: &[u8] = &[0x46, 0x01, 0x50];

    #[test]
    fn h265_bare_idr_gets_vps_sps_pps() {
        let mut cache = ParamSetCache::new(true);
        assert_eq!(cache.process(&au(&[HVPS, HSPS, HPPS, HIDR])), None);
        assert_eq!(cache.process(&au(&[HTRAIL])), None);
        assert_eq!(
            cache.process(&au(&[HIDR])),
            Some(au(&[HVPS, HSPS, HPPS, HIDR]))
        );
        assert_eq!(
            cache.process(&au(&[HAUD, HIDR])),
            Some(au(&[HAUD, HVPS, HSPS, HPPS, HIDR]))
        );
        assert_eq!(cache.process(&au(&[HVPS, HSPS, HPPS, HIDR])), None);
    }

    #[test]
    fn h265_without_a_cached_vps_is_unchanged() {
        let mut cache = ParamSetCache::new(true);
        cache.process(&au(&[HSPS, HPPS, HIDR]));
        assert_eq!(cache.process(&au(&[HIDR])), None);
    }
}
