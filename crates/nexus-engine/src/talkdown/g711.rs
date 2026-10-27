//! G.711 encoders (ITU-T G.711, bit-for-bit with the classic Sun `g711.c`).

/// The two G.711 codecs a talk-down speaker can be offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// µ-law, static RTP payload type 0.
    Pcmu,
    /// A-law, static RTP payload type 8.
    Pcma,
}

impl Codec {
    pub fn from_payload_type(pt: u8) -> Option<Self> {
        match pt {
            0 => Some(Self::Pcmu),
            8 => Some(Self::Pcma),
            _ => None,
        }
    }

    pub fn payload_type(self) -> u8 {
        match self {
            Self::Pcmu => 0,
            Self::Pcma => 8,
        }
    }

    pub fn encode(self, sample: i16) -> u8 {
        match self {
            Self::Pcmu => ulaw(sample),
            Self::Pcma => alaw(sample),
        }
    }
}

/// Encode one 16-bit linear sample as µ-law.
pub fn ulaw(sample: i16) -> u8 {
    let v = i32::from(sample) >> 2;
    let (mag, mask) = if v < 0 { (-v, 0x7f) } else { (v, 0xff) };
    let mag = mag.min(8159) + 33;
    // Segment = position of the top bit above bit 5 (Sun's `seg_uend` table).
    let seg = (32 - mag.leading_zeros() as i32 - 6).max(0);
    if seg >= 8 {
        return 0x7f ^ mask;
    }
    (((seg << 4) | ((mag >> (seg + 1)) & 0xf)) as u8) ^ mask
}

/// Encode one 16-bit linear sample as A-law.
pub fn alaw(sample: i16) -> u8 {
    let v = i32::from(sample) >> 3;
    let (mag, mask) = if v < 0 { (-v - 1, 0x55) } else { (v, 0xd5) };
    // Segment = position of the top bit above bit 4 (Sun's `seg_aend` table);
    // `mag` is at most 4095, so it never exceeds 7.
    let seg = (32 - mag.leading_zeros() as i32 - 5).max(0);
    let mantissa = (if seg < 2 { mag >> 1 } else { mag >> seg }) & 0xf;
    (((seg << 4) | mantissa) as u8) ^ mask
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLES: [i16; 13] = [
        0, -1, 1, 100, -100, 1000, -1000, 4000, -4000, 12345, -12345, 32767, -32768,
    ];

    // Expected codes and hashes come from Python's `audioop.lin2ulaw` /
    // `lin2alaw` (the Sun reference implementation).
    #[test]
    fn ulaw_known_values() {
        let want = [
            0xff, 0x7e, 0xff, 0xf2, 0x72, 0xce, 0x4e, 0xaf, 0x2f, 0x97, 0x17, 0x80, 0x00,
        ];
        assert_eq!(SAMPLES.map(ulaw), want);
        assert_eq!(fnv_all(ulaw), 0xd5ee_23f7_d0ff_ae91);
    }

    #[test]
    fn alaw_known_values() {
        let want = [
            0xd5, 0x55, 0xd5, 0xd3, 0x53, 0xfa, 0x7a, 0x9a, 0x1a, 0xbd, 0x3d, 0xaa, 0x2a,
        ];
        assert_eq!(SAMPLES.map(alaw), want);
        assert_eq!(fnv_all(alaw), 0xf030_666e_b780_cf25);
    }

    /// FNV-1a over the encoding of every i16, so all 65536 inputs are pinned.
    fn fnv_all(encode: fn(i16) -> u8) -> u64 {
        (i16::MIN..=i16::MAX).fold(0xcbf2_9ce4_8422_2325, |h, s| {
            (h ^ u64::from(encode(s))).wrapping_mul(0x100_0000_01b3)
        })
    }
}
