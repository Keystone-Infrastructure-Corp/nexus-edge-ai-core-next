//! The one JPEG encode of packed RGB24 pixels, shared by the alert
//! snapshot, the snapshot route and the live-view encoder.

use image::ImageEncoder as _;

/// JPEG-encode `pixels` as a packed `width`×`height` RGB24 image.
///
/// `image`'s `JpegEncoder` asserts that the buffer is exactly
/// `width * height * 3` bytes, and the release profile aborts on a
/// panic, so one wrongly sized frame would end the engine. The length
/// is checked here and a mismatch is an error.
pub fn encode_rgb24(
    pixels: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, String> {
    let expected = width as usize * height as usize * 3;
    if pixels.len() != expected {
        return Err(format!(
            "a {width}x{height} RGB24 frame is {expected} bytes, got {}",
            pixels.len()
        ));
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .write_image(pixels, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_buffer_encodes() {
        let jpeg = encode_rgb24(&[120u8; 642 * 361 * 3], 642, 361, 80).expect("encode");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }

    /// Short, GStreamer's padded row stride at 642 px, and the
    /// trailing partial pixel `Frame::rgb24` keeps.
    #[test]
    fn a_buffer_of_any_other_length_is_an_error() {
        for (len, w, h) in [
            (642 * 361 * 3 - 1, 642, 361),
            (1928 * 361, 642, 361),
            (13, 2, 2),
        ] {
            assert_eq!(
                encode_rgb24(&vec![0u8; len], w, h, 80),
                Err(format!(
                    "a {w}x{h} RGB24 frame is {} bytes, got {len}",
                    w * h * 3
                )),
            );
        }
    }
}
