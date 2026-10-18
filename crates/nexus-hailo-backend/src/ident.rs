//! HailoRT identity-string handling, outside the `linked` gate so it is
//! compiled and tested under `cargo test` on every target.

/// Convert a HailoRT fixed-size identity field to a `String`.
///
/// HailoRT reports the padded field length, so the bytes after the real
/// value are NULs. Read at most `len` bytes (bounded by `buf`) and stop at
/// the first NUL; a field with no NUL uses the whole bounded slice.
pub(crate) fn fixed_field_to_string(buf: &[u8], len: usize) -> String {
    let bounded = &buf[..len.min(buf.len())];
    let value = bounded.split(|&b| b == 0).next().unwrap_or(bounded);
    String::from_utf8_lossy(value).into_owned()
}

#[cfg(test)]
mod tests {
    use super::fixed_field_to_string;

    #[test]
    fn nul_padded_field_is_cut_at_first_nul() {
        let mut buf = [0u8; 32];
        buf[..12].copy_from_slice(b"HM218B1C2LAE");
        assert_eq!(fixed_field_to_string(&buf, 16), "HM218B1C2LAE");
    }

    #[test]
    fn full_length_field_without_nul_is_kept_whole() {
        assert_eq!(fixed_field_to_string(b"HM218B1C2LAE", 12), "HM218B1C2LAE");
    }

    #[test]
    fn zero_length_field_is_empty() {
        assert_eq!(fixed_field_to_string(b"HM218B1C2LAE", 0), "");
    }

    #[test]
    fn len_beyond_buffer_is_bounded_by_buffer() {
        assert_eq!(fixed_field_to_string(b"HM21", 99), "HM21");
    }
}
