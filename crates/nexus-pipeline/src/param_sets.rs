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
//! already carries any parameter set (even just one of VPS/SPS/PPS)
//! counts as "has params" and is never touched, so cameras that send
//! them per keyframe produce byte-identical output.
//!
//! Only the latest VPS, the latest SPS and the latest PPS are kept --
//! one slot each, not one per parameter-set id. A stream that uses
//! several SPS/PPS ids gets back only the most recent of each.
//!
//! It also reports when an access unit changes the SPS (or VPS)
//! mid-session ([`ParamSetCache::sps_changed`]), so the ingester can
//! start a new pre-roll epoch there: mp4mux cannot take a new SPS in
//! the middle of a clip (#376). That comparison is per parameter-set
//! id, unlike the single re-injection slots above, and is against the
//! parameter sets of the current epoch only: each epoch's clip gets a
//! fresh muxer, so returning to an SPS an earlier epoch carried is a
//! change too ([`ParamSetCache::begin_epoch`]).

/// Per-session cache of the most recent parameter-set NAL units, stored
/// without their start codes.
#[derive(Debug, Default)]
pub struct ParamSetCache {
    hevc: bool,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    sps_changed: bool,
    /// SPS seen this epoch, by `seq_parameter_set_id`, for change
    /// detection only.
    sps_by_id: Vec<(u32, Vec<u8>)>,
    /// H.265 VPS seen this epoch, by `vps_video_parameter_set_id`, for
    /// change detection only.
    vps_by_id: Vec<(u32, Vec<u8>)>,
    /// The SPS and VPS, by id, of the access unit last passed to
    /// [`Self::process`]: all a muxer starting at that AU has been given.
    au_sps_by_id: Vec<(u32, Vec<u8>)>,
    au_vps_by_id: Vec<(u32, Vec<u8>)>,
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
        self.sps_changed = false;
        self.au_sps_by_id.clear();
        self.au_vps_by_id.clear();
        // An epoch with none of a kind yet takes every id of that kind in
        // this AU as its first, not just the first NAL's.
        let first_vps = self.vps_by_id.is_empty();
        let first_sps = self.sps_by_id.is_empty();
        for &(_, start, end) in &nals {
            let nal = &au[start..end];
            let kind = self.kind(nal[0]);
            let (slot, first) = match kind {
                NalKind::Vps => (&mut self.vps, first_vps),
                NalKind::Sps => (&mut self.sps, first_sps),
                NalKind::Pps => (&mut self.pps, false),
                NalKind::Idr => {
                    has_idr = true;
                    continue;
                }
                _ => continue,
            };
            *slot = Some(nal.to_vec());
            has_params = true;
            self.sps_changed |= self.note(kind, nal, first);
        }
        if self.sps_changed {
            // This AU heads the new epoch.
            self.begin_epoch();
        }
        if has_params || !has_idr {
            return None;
        }
        let (Some(sps), Some(pps)) = (self.sps.clone(), self.pps.clone()) else {
            return None;
        };
        let vps = if self.hevc {
            Some(self.vps.clone()?)
        } else {
            None
        };
        // The muxer is given the injected VPS/SPS as if this AU carried
        // them; they are already this epoch's or its first, never a change.
        if let Some(vps) = &vps {
            self.note(NalKind::Vps, vps, true);
        }
        self.note(NalKind::Sps, &sps, true);
        // Parameter sets follow an access-unit delimiter if there is one.
        let insert_at = match nals.first() {
            Some(&(_, start, _)) if self.kind(au[start]) == NalKind::Aud => {
                nals.get(1).map(|&(sc, _, _)| sc).unwrap_or(au.len())
            }
            _ => 0,
        };
        let mut out = Vec::with_capacity(au.len() + 64);
        out.extend_from_slice(&au[..insert_at]);
        for ps in vps.iter().chain([&sps, &pps]) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(ps);
        }
        out.extend_from_slice(&au[insert_at..]);
        Some(out)
    }

    /// Whether the access unit last passed to [`Self::process`] carried
    /// an SPS (or, for H.265, a VPS or SPS) that changes what the current
    /// epoch's muxer was given. Trailing zero bytes are not content. The
    /// SPS of an epoch's first AU carrying any (whatever their ids, and
    /// including injected ones) are not a change, and neither is a PPS: a
    /// stream alternating PPS ids would otherwise read as a change every
    /// GOP. SPS (and VPS) are compared per id, as h264parse / h265parse
    /// store them, against the current epoch's only: a new id or new
    /// bytes for a known id is a change -- including an id an earlier
    /// epoch carried -- and a stream repeating two ids every keyframe is
    /// not. A change begins a new epoch at that access unit.
    pub fn sps_changed(&self) -> bool {
        self.sps_changed
    }

    /// Begin a new epoch at the access unit last passed to
    /// [`Self::process`]: a fresh muxer starts there, so from now on SPS
    /// and VPS are compared against that AU's alone (including any it was
    /// given by injection). The ingester calls this whenever an epoch
    /// begins; it is a no-op after an SPS change, and matters for the
    /// ring's PTS step back. The re-injection slots are untouched.
    pub fn begin_epoch(&mut self) {
        self.sps_by_id.clone_from(&self.au_sps_by_id);
        self.vps_by_id.clone_from(&self.au_vps_by_id);
    }

    /// Record a VPS/SPS under its id in the current epoch's and this
    /// AU's maps; `true` when it changes what the epoch's muxer was given.
    /// `first`: the epoch had none of this kind when the AU began.
    fn note(&mut self, kind: NalKind, nal: &[u8], first: bool) -> bool {
        let (id, seen, in_au) = match kind {
            NalKind::Vps => (
                h265_vps_id(nal),
                &mut self.vps_by_id,
                &mut self.au_vps_by_id,
            ),
            NalKind::Sps if self.hevc => (
                h265_sps_id(nal),
                &mut self.sps_by_id,
                &mut self.au_sps_by_id,
            ),
            NalKind::Sps => (
                h264_sps_id(nal),
                &mut self.sps_by_id,
                &mut self.au_sps_by_id,
            ),
            _ => return false,
        };
        let id = id.unwrap_or(0);
        note_by_id(in_au, id, nal, true);
        note_by_id(seen, id, nal, first)
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

fn trim_trailing_zeros(nal: &[u8]) -> &[u8] {
    let end = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &nal[..end]
}

/// Record a VPS/SPS under its id; `true` when it changes what the muxer
/// was given (new bytes for a known id, or a new id), unless it is part
/// of the epoch's `first` of its kind.
fn note_by_id(seen: &mut Vec<(u32, Vec<u8>)>, id: u32, nal: &[u8], first: bool) -> bool {
    let bytes = trim_trailing_zeros(nal);
    match seen.iter_mut().find(|(i, _)| *i == id) {
        Some((_, prev)) if prev.as_slice() == bytes => false,
        Some((_, prev)) => {
            *prev = bytes.to_vec();
            !first
        }
        None => {
            seen.push((id, bytes.to_vec()));
            !first
        }
    }
}

/// Big-endian bit reader over an RBSP; every read fails at the end.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn bit(&mut self) -> Option<u32> {
        let b = self.data.get(self.pos / 8)?;
        let v = (b >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(v))
    }

    fn bits(&mut self, n: usize) -> Option<u32> {
        (0..n).try_fold(0, |v, _| Some((v << 1) | self.bit()?))
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.data.len() * 8).then_some(())
    }

    /// ue(v) with at most `max_zeros` leading zero bits.
    fn ue(&mut self, max_zeros: usize) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > max_zeros {
                return None;
            }
        }
        Some((1 << zeros) - 1 + self.bits(zeros)?)
    }
}

/// H.264 `seq_parameter_set_id`: the ue(v) after the NAL header,
/// profile_idc, the constraint flags and level_idc. No
/// emulation-prevention byte can come before it, since profile_idc and
/// level_idc are never zero.
fn h264_sps_id(nal: &[u8]) -> Option<u32> {
    let mut r = BitReader {
        data: nal.get(4..)?,
        pos: 0,
    };
    r.ue(5) // ids are 0..=31
}

/// H.265 `vps_video_parameter_set_id`: the first 4 bits after the
/// two-byte NAL header, which no emulation-prevention byte can precede.
fn h265_vps_id(nal: &[u8]) -> Option<u32> {
    Some(u32::from(nal.get(2)? >> 4))
}

/// H.265 `sps_seq_parameter_set_id` (H.265 §7.3.2.2): the ue(v) after
/// the sps_video_parameter_set_id / max_sub_layers / nesting byte and
/// `profile_tier_level(1, max_sub_layers_minus1)`. That structure
/// routinely holds emulation-prevention bytes, so the RBSP is unescaped
/// first.
fn h265_sps_id(nal: &[u8]) -> Option<u32> {
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal.get(2..)? {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = BitReader {
        data: &rbsp,
        pos: 4, // sps_video_parameter_set_id
    };
    let m = r.bits(3)? as usize; // sps_max_sub_layers_minus1
    r.skip(1 + 96)?; // nesting flag, general profile fields and level
    let flags = (0..m)
        .map(|_| Some((r.bit()? == 1, r.bit()? == 1)))
        .collect::<Option<Vec<_>>>()?;
    if m > 0 {
        r.skip(2 * (8 - m))?; // reserved_zero_2bits
    }
    for (profile, level) in flags {
        r.skip(if profile { 88 } else { 0 } + if level { 8 } else { 0 })?;
    }
    r.ue(4) // ids are 0..=15
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

    #[test]
    fn the_first_sps_of_a_session_is_not_a_change() {
        let mut cache = ParamSetCache::new(false);
        assert_eq!(cache.process(&au(&[SPS, PPS, IDR])), None);
        assert!(!cache.sps_changed());
    }

    #[test]
    fn a_differing_sps_is_a_change_reported_on_that_au_only() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS, PPS, IDR]));
        cache.process(&au(&[P]));
        // Main -> High profile.
        let high: &[u8] = &[0x67, 0x64, 0x00, 0x1f];
        assert_eq!(
            cache.process(&au(&[high, PPS, IDR])),
            None,
            "still passthrough"
        );
        assert!(cache.sps_changed());
        cache.process(&au(&[P]));
        assert!(!cache.sps_changed(), "the change belongs to one AU");
        cache.process(&au(&[high, PPS, IDR]));
        assert!(!cache.sps_changed(), "the new SPS repeated is not a change");
    }

    #[test]
    fn a_frame_rate_change_in_the_sps_is_a_change() {
        // Same profile and level; the VUI timing bytes differ (15 -> 20 fps).
        let fps15: &[u8] = &[0x67, 0x4d, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x1e, 0x00];
        let fps20: &[u8] = &[0x67, 0x4d, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x28, 0x00];
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[fps15, PPS, IDR]));
        assert!(!cache.sps_changed());
        cache.process(&au(&[AUD, fps20, PPS, IDR]));
        assert!(cache.sps_changed());
    }

    // Main-profile SPS NALs whose seq_parameter_set_id (the ue(v) after
    // level_idc) is 0 (`1...`) and 1 (`010...`).
    const SPS_ID0: &[u8] = &[0x67, 0x4d, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80];
    const SPS_ID1: &[u8] = &[0x67, 0x4d, 0x00, 0x1f, 0x4d, 0x33, 0x01, 0x40];

    #[test]
    fn two_sps_ids_repeated_every_keyframe_are_not_a_change() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS_ID0, SPS_ID1, PPS, IDR]));
        for _ in 0..3 {
            cache.process(&au(&[SPS_ID0, SPS_ID1, PPS, IDR]));
            assert!(
                !cache.sps_changed(),
                "a stable second SPS id must not rotate the clip every GOP"
            );
        }
    }

    #[test]
    fn a_new_sps_id_or_new_bytes_for_a_known_id_is_a_change() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS_ID0, PPS, IDR]));
        cache.process(&au(&[SPS_ID0, SPS_ID1, PPS, IDR]));
        assert!(cache.sps_changed(), "a new SPS id re-caps the muxer");
        let id1_changed: &[u8] = &[0x67, 0x4d, 0x00, 0x1f, 0x4d, 0x33, 0x01, 0x48];
        cache.process(&au(&[SPS_ID0, id1_changed, PPS, IDR]));
        assert!(cache.sps_changed());
    }

    #[test]
    fn returning_to_an_sps_id_of_an_earlier_epoch_is_a_change() {
        // PR #379 review: A -> B rotates to a muxer that only knows B, so
        // the return to A must rotate again.
        let mut cache = ParamSetCache::new(false);
        let mut flags = Vec::new();
        for sps in [SPS_ID0, SPS_ID1, SPS_ID0] {
            cache.process(&au(&[sps, PPS, IDR]));
            flags.push(cache.sps_changed());
        }
        assert_eq!(flags, [false, true, true]);
        cache.process(&au(&[SPS_ID0, PPS, IDR]));
        assert!(!cache.sps_changed(), "A repeated within its epoch");
    }

    #[test]
    fn a_new_sps_repeated_after_a_change_is_not_a_change() {
        let mut cache = ParamSetCache::new(false);
        let mut flags = Vec::new();
        for sps in [SPS_ID0, SPS_ID1, SPS_ID1] {
            cache.process(&au(&[sps, PPS, IDR]));
            flags.push(cache.sps_changed());
        }
        assert_eq!(flags, [false, true, false]);
    }

    #[test]
    fn every_id_of_the_au_heading_an_epoch_is_known_to_it() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS_ID0, PPS, IDR]));
        cache.process(&au(&[SPS_ID0, SPS_ID1, PPS, IDR]));
        assert!(cache.sps_changed());
        for _ in 0..3 {
            cache.process(&au(&[SPS_ID0, SPS_ID1, PPS, IDR]));
            assert!(
                !cache.sps_changed(),
                "a stable two-id stream after a change"
            );
        }
    }

    #[test]
    fn after_begin_epoch_an_id_is_not_a_change_as_the_epochs_first() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS_ID0, PPS, IDR]));
        cache.process(&au(&[SPS_ID1, PPS, IDR]));
        // The ring began an epoch at an AU carrying no SPS.
        cache.process(&au(&[P]));
        cache.begin_epoch();
        cache.process(&au(&[SPS_ID0, PPS, IDR]));
        assert!(!cache.sps_changed(), "the epoch's first SPS");
        cache.process(&au(&[SPS_ID1, PPS, IDR]));
        assert!(cache.sps_changed(), "a second id in the same epoch");
    }

    #[test]
    fn begin_epoch_keeps_the_sps_of_the_au_heading_the_epoch() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS_ID0, PPS, IDR]));
        // PTS step back at a keyframe repeating A: the new muxer gets A.
        cache.process(&au(&[SPS_ID0, PPS, IDR]));
        cache.begin_epoch();
        cache.process(&au(&[SPS_ID1, PPS, IDR]));
        assert!(cache.sps_changed(), "B after the head's A");
    }

    #[test]
    fn begin_epoch_leaves_reinjection_alone() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS, PPS, IDR]));
        cache.process(&au(&[P]));
        cache.begin_epoch();
        assert_eq!(cache.process(&au(&[IDR])), Some(au(&[SPS, PPS, IDR])));
    }

    #[test]
    fn parameter_sets_injected_into_a_bare_idr_count_toward_the_epoch() {
        // #376 review: the muxer is given the injected SPS, so an SPS
        // after it is compared against it.
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS_ID0, PPS, IDR]));
        cache.process(&au(&[P]));
        assert!(cache.process(&au(&[IDR])).is_some(), "injected");
        assert!(!cache.sps_changed(), "injecting is never a change");
        // PTS step back at the bare IDR: its new muxer got SPS id 0.
        cache.begin_epoch();
        cache.process(&au(&[SPS_ID1, PPS, IDR]));
        assert!(cache.sps_changed(), "id 1 after the injected id 0");
    }

    #[test]
    fn an_sps_injected_after_an_epoch_begun_at_a_delta_is_that_epochs_first() {
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS_ID0, PPS, IDR]));
        cache.process(&au(&[P]));
        cache.begin_epoch();
        assert!(cache.process(&au(&[IDR])).is_some(), "injected");
        cache.process(&au(&[SPS_ID1, PPS, IDR]));
        assert!(cache.sps_changed(), "id 1 after the injected id 0");
    }

    #[test]
    fn h265_vps_injected_into_a_bare_idr_counts_toward_the_epoch() {
        let mut cache = ParamSetCache::new(true);
        cache.process(&au(&[&h265_vps(0, 1), HSPS, HPPS, HIDR]));
        assert!(cache.process(&au(&[HIDR])).is_some(), "injected");
        cache.begin_epoch();
        cache.process(&au(&[&h265_vps(1, 1), HSPS, HPPS, HIDR]));
        assert!(cache.sps_changed(), "VPS id 1 after the injected id 0");
    }

    #[test]
    fn every_id_of_an_epochs_first_au_is_its_first() {
        // #376 review: "first of the epoch" is per access unit, not per NAL.
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS_ID0, SPS_ID1, PPS, IDR]));
        assert!(!cache.sps_changed(), "both ids head the session");
        cache.process(&au(&[P]));
        cache.begin_epoch();
        cache.process(&au(&[SPS_ID0, SPS_ID1, PPS, IDR]));
        assert!(!cache.sps_changed(), "both ids head the epoch");
        // seq_parameter_set_id 2 (`011...`).
        let sps_id2: &[u8] = &[0x67, 0x4d, 0x00, 0x1f, 0x6d, 0x33, 0x01, 0x40];
        assert_eq!(h264_sps_id(sps_id2), Some(2));
        cache.process(&au(&[SPS_ID0, SPS_ID1, sps_id2, PPS, IDR]));
        assert!(cache.sps_changed(), "a new id after the epoch's first AU");
    }

    #[test]
    fn an_identical_sps_with_or_without_trailing_zeros_is_not_a_change() {
        let padded: &[u8] = &[0x67, 0x42, 0x00, 0x1f, 0x00, 0x00];
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS, PPS, IDR]));
        cache.process(&au(&[padded, PPS, IDR]));
        assert!(
            !cache.sps_changed(),
            "trailing zero bytes are not SPS content"
        );
        cache.process(&au(&[SPS, PPS, IDR]));
        assert!(!cache.sps_changed());
        // At the very end of the AU too, where no start code follows.
        cache.process(&au(&[PPS, IDR, padded]));
        assert!(!cache.sps_changed());
    }

    #[test]
    fn a_pps_only_change_is_not_a_change() {
        let pps2: &[u8] = &[0x68, 0xee, 0x3c, 0x80];
        let mut cache = ParamSetCache::new(false);
        cache.process(&au(&[SPS, PPS, IDR]));
        cache.process(&au(&[SPS, pps2, IDR]));
        assert!(!cache.sps_changed());
        cache.process(&au(&[pps2, IDR]));
        assert!(!cache.sps_changed());
    }

    #[test]
    fn h265_vps_or_sps_change_is_a_change() {
        let hsps2: &[u8] = &[0x42, 0x01, 0x02];
        let hvps2: &[u8] = &[0x40, 0x01, 0x0d];
        let hpps2: &[u8] = &[0x44, 0x01, 0xc2];
        let mut cache = ParamSetCache::new(true);
        cache.process(&au(&[HVPS, HSPS, HPPS, HIDR]));
        assert!(!cache.sps_changed(), "first VPS/SPS of the session");
        cache.process(&au(&[HVPS, hsps2, HPPS, HIDR]));
        assert!(cache.sps_changed(), "SPS changed");
        cache.process(&au(&[hvps2, hsps2, HPPS, HIDR]));
        assert!(cache.sps_changed(), "VPS changed");
        cache.process(&au(&[hvps2, hsps2, hpps2, HIDR]));
        assert!(!cache.sps_changed(), "PPS only");
    }

    // --- H.265 per-id comparison (#376) ---

    /// Insert emulation-prevention bytes, as an encoder does: a 0x03
    /// after every 0x00 0x00 that is followed by a byte <= 3.
    fn escape(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut zeros = 0;
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    /// A syntactically valid H.265 SPS NAL: all-zero profile fields (so
    /// emulation-prevention bytes appear, as in real streams),
    /// `sub` = (profile_present, level_present) per sub-layer, ending in
    /// a non-zero `extra` byte so two SPS of one id can differ.
    fn h265_sps(sub: &[(bool, bool)], id: u32, extra: u8) -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();
        let mut put = |v: u32, n: u32| {
            for i in (0..n).rev() {
                bits.push(i < 32 && (v >> i) & 1 == 1);
            }
        };
        let m = sub.len() as u32;
        put(0, 4); // sps_video_parameter_set_id
        put(m, 3); // sps_max_sub_layers_minus1
        put(1, 1); // sps_temporal_id_nesting_flag
        put(0, 88); // general profile fields
        put(93, 8); // general_level_idc
        for &(p, l) in sub {
            put(p.into(), 1);
            put(l.into(), 1);
        }
        if m > 0 {
            put(0, 2 * (8 - m));
        }
        for &(p, l) in sub {
            if p {
                put(0, 88);
            }
            if l {
                put(90, 8);
            }
        }
        let len = 32 - (id + 1).leading_zeros(); // bits in id + 1
        put(0, len - 1);
        put(id + 1, len); // ue(v)
        put(1, 1); // stop bit
        let mut rbsp: Vec<u8> = bits
            .chunks(8)
            .map(|c| (0..8).fold(0u8, |a, i| (a << 1) | u8::from(c.get(i) == Some(&true))))
            .collect();
        rbsp.push(extra);
        let mut nal = vec![0x42, 0x01];
        nal.extend(escape(&rbsp));
        nal
    }

    fn h265_vps(id: u8, extra: u8) -> Vec<u8> {
        vec![0x40, 0x01, (id << 4) | 0x0f, extra]
    }

    #[test]
    fn h265_sps_id_survives_emulation_prevention_in_profile_tier_level() {
        // x265 Main SPS; the 0x03 bytes sit inside profile_tier_level.
        // `a0` starts `1`: sps_seq_parameter_set_id 0.
        let x265: &[u8] = &[
            0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x03, 0x00, 0x5d, 0xa0, 0x02, 0x80, 0x80, 0x2d, 0x16,
        ];
        assert_eq!(h265_sps_id(x265), Some(0));
        // The same SPS with `40` (`010`): id 1. Read raw, the 0x03 bytes
        // would shift every later field and this could not come out as 1.
        let mut id1 = x265.to_vec();
        id1[18] = 0x40;
        assert_eq!(h265_sps_id(&id1), Some(1));
    }

    #[test]
    fn h265_sps_id_reads_ids_and_sub_layer_fields() {
        assert!(h265_sps(&[], 3, 0x11).windows(3).any(|w| w == [0, 0, 3]));
        for id in [0, 1, 2, 7, 15] {
            assert_eq!(h265_sps_id(&h265_sps(&[], id, 0x11)), Some(id));
        }
        // sps_max_sub_layers_minus1 > 0: the per-sub-layer flags, the
        // alignment bits and the optional sub-layer fields all come first.
        let layers: &[&[(bool, bool)]] = &[
            &[(false, false)],
            &[(true, false)],
            &[(false, true)],
            &[(true, true), (false, true)],
            &[(true, false), (true, true), (false, false)],
        ];
        for sub in layers {
            assert_eq!(h265_sps_id(&h265_sps(sub, 5, 0x11)), Some(5), "{sub:?}");
        }
    }

    #[test]
    fn h265_sps_id_of_a_truncated_sps_is_none() {
        let full = h265_sps(&[(true, true)], 1, 0x11);
        for cut in 0..full.len() - 3 {
            // Never panics; a cut before the id has nothing to read.
            assert_eq!(h265_sps_id(&full[..cut]), None, "cut at {cut}");
        }
        assert_eq!(h265_sps_id(&[]), None);
    }

    #[test]
    fn h265_vps_id_is_the_first_four_rbsp_bits() {
        assert_eq!(h265_vps_id(&h265_vps(0, 1)), Some(0));
        assert_eq!(h265_vps_id(&h265_vps(1, 1)), Some(1));
        assert_eq!(h265_vps_id(&h265_vps(15, 1)), Some(15));
        assert_eq!(h265_vps_id(&[0x40, 0x01]), None);
    }

    #[test]
    fn two_h265_sps_ids_repeated_every_keyframe_are_not_a_change() {
        let (a, b) = (h265_sps(&[], 0, 0x11), h265_sps(&[], 1, 0x11));
        let mut cache = ParamSetCache::new(true);
        cache.process(&au(&[HVPS, &a, &b, HPPS, HIDR]));
        for _ in 0..3 {
            cache.process(&au(&[HVPS, &a, &b, HPPS, HIDR]));
            assert!(
                !cache.sps_changed(),
                "a stable second SPS id must not rotate the clip every GOP"
            );
        }
    }

    #[test]
    fn two_h265_vps_ids_repeated_every_keyframe_are_not_a_change() {
        let (a, b) = (h265_vps(0, 1), h265_vps(1, 1));
        let mut cache = ParamSetCache::new(true);
        cache.process(&au(&[&a, &b, HSPS, HPPS, HIDR]));
        for _ in 0..3 {
            cache.process(&au(&[&a, &b, HSPS, HPPS, HIDR]));
            assert!(!cache.sps_changed(), "a stable second VPS id");
        }
    }

    #[test]
    fn a_new_h265_sps_id_or_new_bytes_for_a_known_id_is_a_change() {
        let (a, b) = (h265_sps(&[], 0, 0x11), h265_sps(&[], 1, 0x11));
        let mut cache = ParamSetCache::new(true);
        cache.process(&au(&[HVPS, &a, HPPS, HIDR]));
        cache.process(&au(&[HVPS, &a, &b, HPPS, HIDR]));
        assert!(cache.sps_changed(), "a new SPS id re-caps the muxer");
        cache.process(&au(&[HVPS, &a, &b, HPPS, HIDR]));
        assert!(!cache.sps_changed());
        let b2 = h265_sps(&[], 1, 0x12);
        cache.process(&au(&[HVPS, &a, &b2, HPPS, HIDR]));
        assert!(cache.sps_changed(), "new bytes for a known SPS id");
    }

    #[test]
    fn returning_to_an_h265_sps_id_of_an_earlier_epoch_is_a_change() {
        let (a, b) = (h265_sps(&[], 0, 0x11), h265_sps(&[], 1, 0x11));
        let mut cache = ParamSetCache::new(true);
        let mut flags = Vec::new();
        for sps in [&a, &b, &a, &a] {
            cache.process(&au(&[HVPS, sps, HPPS, HIDR]));
            flags.push(cache.sps_changed());
        }
        assert_eq!(flags, [false, true, true, false]);
    }

    #[test]
    fn a_new_h265_vps_id_or_new_bytes_for_a_known_id_is_a_change() {
        let mut cache = ParamSetCache::new(true);
        cache.process(&au(&[&h265_vps(0, 1), HSPS, HPPS, HIDR]));
        cache.process(&au(&[&h265_vps(0, 1), &h265_vps(1, 1), HSPS, HPPS, HIDR]));
        assert!(cache.sps_changed(), "a new VPS id");
        cache.process(&au(&[&h265_vps(0, 1), &h265_vps(1, 1), HSPS, HPPS, HIDR]));
        assert!(!cache.sps_changed());
        cache.process(&au(&[&h265_vps(0, 1), &h265_vps(1, 2), HSPS, HPPS, HIDR]));
        assert!(cache.sps_changed(), "new bytes for a known VPS id");
    }

    #[test]
    fn h265_vps_and_sps_ids_are_tracked_independently() {
        // VPS id 1 and SPS id 1 appearing as the first of their kind is
        // not a change just because the other kind already has an id 1.
        let mut cache = ParamSetCache::new(true);
        cache.process(&au(&[&h265_vps(1, 1), &h265_sps(&[], 1, 0x11), HPPS, HIDR]));
        assert!(!cache.sps_changed());
        cache.process(&au(&[&h265_vps(1, 1), &h265_sps(&[], 1, 0x11), HPPS, HIDR]));
        assert!(!cache.sps_changed());
    }

    #[test]
    fn bare_h265_idr_gets_the_latest_sps_of_the_last_id_seen() {
        // Re-injection keeps its single latest-of-each slot.
        let (a, b) = (h265_sps(&[], 0, 0x11), h265_sps(&[], 1, 0x11));
        let mut cache = ParamSetCache::new(true);
        cache.process(&au(&[HVPS, &a, &b, HPPS, HIDR]));
        assert_eq!(
            cache.process(&au(&[HIDR])),
            Some(au(&[HVPS, &b, HPPS, HIDR]))
        );
    }
}
