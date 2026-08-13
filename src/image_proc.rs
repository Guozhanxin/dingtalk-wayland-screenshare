use resize::Pixel::RGBA8;
use resize::Type::Triangle;
use rgb::FromSlice;

/// Byte layout of a packed, four-byte RGB pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgbx,
    Bgrx,
    Rgba,
    Bgra,
}

impl PixelFormat {
    fn is_bgr(self) -> bool {
        matches!(self, Self::Bgrx | Self::Bgra)
    }
}

/// Resizes a tightly packed 32-bit pixel buffer and converts its channel order.
///
/// The fourth byte (alpha or padding) is preserved. PipeWire row padding must be
/// removed before calling this function; `src` and `dst` are both tightly packed.
pub fn resize_and_convert(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_format: PixelFormat,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    dst_format: PixelFormat,
) -> Result<(), &'static str> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Err("Dimensions cannot be zero");
    }

    let src_len = (src_w as usize)
        .checked_mul(src_h as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("Overflow calculating source buffer size")?;

    let dst_len = (dst_w as usize)
        .checked_mul(dst_h as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("Overflow calculating destination buffer size")?;

    if src.len() != src_len {
        return Err("Source buffer size does not match dimensions");
    }
    if dst.len() != dst_len {
        return Err("Destination buffer size does not match dimensions");
    }

    if src_w == dst_w && src_h == dst_h {
        dst.copy_from_slice(src);
    } else {
        // The resizer treats all four bytes independently, so it is safe to use
        // RGBA8 for BGRx/BGRA data and fix the channel order afterwards.
        let src_rgba = src.as_rgba();
        let dst_rgba = dst.as_rgba_mut();

        let mut resizer = resize::new(
            src_w as usize,
            src_h as usize,
            dst_w as usize,
            dst_h as usize,
            RGBA8,
            Triangle,
        )
        .map_err(|_| "Failed to initialize resizer")?;

        resizer
            .resize(src_rgba, dst_rgba)
            .map_err(|_| "Resizing failed")?;
    }

    // BGRx/BGRA and RGBx/RGBA differ only in the red/blue byte positions.
    // Do not swap when PipeWire and XImage already use the same order.
    if src_format.is_bgr() != dst_format.is_bgr() {
        for pixel in dst.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgr_to_bgr_does_not_swap_channels() {
        let src = vec![255, 10, 20, 0, 30, 40, 50, 255];
        let mut dst = vec![0u8; src.len()];

        resize_and_convert(
            &src,
            2,
            1,
            PixelFormat::Bgrx,
            &mut dst,
            2,
            1,
            PixelFormat::Bgrx,
        )
        .unwrap();

        assert_eq!(dst, src);
    }

    #[test]
    fn rgb_to_bgr_swaps_red_and_blue() {
        let src = vec![255, 0, 0, 255, 0, 0, 255, 10];
        let mut dst = vec![0u8; src.len()];

        resize_and_convert(
            &src,
            2,
            1,
            PixelFormat::Rgba,
            &mut dst,
            2,
            1,
            PixelFormat::Bgrx,
        )
        .unwrap();

        assert_eq!(dst, vec![0, 0, 255, 255, 255, 0, 0, 10]);
    }

    #[test]
    fn downscale_and_convert_channel_order() {
        let src = vec![255, 0, 0, 255].repeat(16);
        let mut dst = vec![0u8; 4];

        resize_and_convert(
            &src,
            4,
            4,
            PixelFormat::Rgbx,
            &mut dst,
            1,
            1,
            PixelFormat::Bgra,
        )
        .unwrap();

        assert_eq!(dst, vec![0, 0, 255, 255]);
    }

    #[test]
    fn validates_dimensions_and_buffer_sizes() {
        let src = vec![0u8; 16];
        let mut dst = vec![0u8; 16];

        assert!(
            resize_and_convert(
                &src,
                0,
                2,
                PixelFormat::Bgrx,
                &mut dst,
                2,
                2,
                PixelFormat::Bgrx,
            )
            .is_err()
        );
        assert!(
            resize_and_convert(
                &src[..12],
                2,
                2,
                PixelFormat::Bgrx,
                &mut dst,
                2,
                2,
                PixelFormat::Bgrx,
            )
            .is_err()
        );
        assert!(
            resize_and_convert(
                &src,
                2,
                2,
                PixelFormat::Bgrx,
                &mut dst[..12],
                2,
                2,
                PixelFormat::Bgrx,
            )
            .is_err()
        );
    }
}
