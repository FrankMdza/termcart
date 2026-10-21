//! Core pixel formats to tightly packed RGBA8.

use super::ffi;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb1555,
    Xrgb8888,
    Rgb565,
}

impl PixelFormat {
    pub fn from_raw(raw: std::ffi::c_int) -> Option<PixelFormat> {
        match raw {
            ffi::PIXEL_FORMAT_0RGB1555 => Some(PixelFormat::Rgb1555),
            ffi::PIXEL_FORMAT_XRGB8888 => Some(PixelFormat::Xrgb8888),
            ffi::PIXEL_FORMAT_RGB565 => Some(PixelFormat::Rgb565),
            _ => None,
        }
    }

    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Xrgb8888 => 4,
            PixelFormat::Rgb1555 | PixelFormat::Rgb565 => 2,
        }
    }
}

/// Converts `height` rows of `width` pixels, `pitch` bytes apart in `src`, into `dst`.
/// `dst` must be `width * height * 4` bytes and `src` must hold every row.
pub fn to_rgba(
    format: PixelFormat,
    src: &[u8],
    width: usize,
    height: usize,
    pitch: usize,
    dst: &mut [u8],
) {
    let row_bytes = width * format.bytes_per_pixel();
    for (y, out) in dst.chunks_exact_mut(width * 4).take(height).enumerate() {
        let row = &src[y * pitch..][..row_bytes];
        let out = out.as_chunks_mut::<4>().0;
        // One match per row so each inner loop is a straight line LLVM can vectorize.
        match format {
            PixelFormat::Xrgb8888 => {
                for (o, p) in out.iter_mut().zip(row.as_chunks::<4>().0) {
                    let p = u32::from_ne_bytes(*p);
                    *o = [(p >> 16) as u8, (p >> 8) as u8, p as u8, 0xFF];
                }
            }
            PixelFormat::Rgb565 => {
                for (o, p) in out.iter_mut().zip(row.as_chunks::<2>().0) {
                    let p = u16::from_ne_bytes(*p);
                    let (r, g, b) = ((p >> 11) & 0x1F, (p >> 5) & 0x3F, p & 0x1F);
                    *o = [expand5(r), (g << 2 | g >> 4) as u8, expand5(b), 0xFF];
                }
            }
            PixelFormat::Rgb1555 => {
                for (o, p) in out.iter_mut().zip(row.as_chunks::<2>().0) {
                    let p = u16::from_ne_bytes(*p);
                    let (r, g, b) = ((p >> 10) & 0x1F, (p >> 5) & 0x1F, p & 0x1F);
                    *o = [expand5(r), expand5(g), expand5(b), 0xFF];
                }
            }
        }
    }
}

/// 5-bit channel to 8 bits, so 0x1F maps to 0xFF.
fn expand5(c: u16) -> u8 {
    (c << 3 | c >> 2) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(format: PixelFormat, pixel: u32, rgba: [u8; 4]) {
        // Two rows with a padded pitch, the second row holds the pixel.
        let bpp = format.bytes_per_pixel();
        let pitch = bpp + 3;
        let mut src = vec![0u8; pitch + bpp];
        let bytes = match format {
            PixelFormat::Xrgb8888 => pixel.to_ne_bytes().to_vec(),
            _ => (pixel as u16).to_ne_bytes().to_vec(),
        };
        src[pitch..].copy_from_slice(&bytes);
        let mut dst = [0u8; 8];
        to_rgba(format, &src, 1, 2, pitch, &mut dst);
        assert_eq!(
            dst,
            [0, 0, 0, 0xFF, rgba[0], rgba[1], rgba[2], rgba[3]],
            "{format:?} {pixel:#x}"
        );
    }

    #[test]
    fn converts_each_format() {
        check(
            PixelFormat::Xrgb8888,
            0xAA_12_34_56,
            [0x12, 0x34, 0x56, 0xFF],
        );
        check(PixelFormat::Rgb565, 0xF800, [0xFF, 0, 0, 0xFF]);
        check(PixelFormat::Rgb565, 0x07E0, [0, 0xFF, 0, 0xFF]);
        check(PixelFormat::Rgb565, 0x001F, [0, 0, 0xFF, 0xFF]);
        check(PixelFormat::Rgb565, 0x8410, [0x84, 0x82, 0x84, 0xFF]);
        check(PixelFormat::Rgb1555, 0x7C00, [0xFF, 0, 0, 0xFF]);
        check(PixelFormat::Rgb1555, 0x03E0, [0, 0xFF, 0, 0xFF]);
        check(PixelFormat::Rgb1555, 0x001F, [0, 0, 0xFF, 0xFF]);
    }
}
