// SPDX-License-Identifier: GPL-2.0-or-later

//! Revdi framebuffer source for the Chimera daemon.
//!
//! Handle ownership and the C ABI boundary live in `librevdi`'s safe Rust client. Chimera only
//! deals in borrowed coherent frames.

pub use evdi::safe::{Cursor, DamageRect, Device as RevdiCard, DeviceEvent, Error, Frame, Mode};

/// Connect a Revdi output using the dock sink's EDID.
pub fn connect_monitor(card: &mut RevdiCard, edid: &[u8]) -> Result<(), Error> {
    const MAX_PIXEL_AREA: u32 = 3840 * 2160;
    card.connect(edid, MAX_PIXEL_AREA, MAX_PIXEL_AREA * 60)
}

/// Convert a coherent XRGB8888 framebuffer to the packed RGB raster used by Vino's codec.
pub fn to_rgb888(frame: &Frame<'_>) -> Vec<u8> {
    let mut out = vec![0u8; frame.width * frame.height * 3];
    for y in 0..frame.height {
        for x in 0..frame.width {
            let source = y * frame.stride + x * 4;
            let target = (y * frame.width + x) * 3;
            let Some(pixel) = frame.pixels.get(source..source + 4) else {
                continue;
            };
            let pixel = u32::from_le_bytes(pixel.try_into().unwrap());
            out[target] = ((pixel >> 16) & 0xff) as u8;
            out[target + 1] = ((pixel >> 8) & 0xff) as u8;
            out[target + 2] = (pixel & 0xff) as u8;
        }
    }
    out
}

/// Validate the live trial's format before reading pixels or feeding the encoder.
pub fn validate_trial_mode(mode: Mode) -> Result<(), String> {
    if mode.width != 1920
        || mode.height != 1080
        || mode.refresh_hz != 60
        || mode.bits_per_pixel != 32
        || ![u32::from_le_bytes(*b"XR24"), u32::from_le_bytes(*b"AR24")]
            .contains(&mode.pixel_format)
    {
        return Err(format!(
            "trial requires 1920x1080@60, XRGB/ARGB8888; got {mode:?}"
        ));
    }
    Ok(())
}

/// Copy validated little-endian XRGB/ARGB into reusable RGB storage, padding the last strip black.
pub fn copy_trial_rgb(frame: &Frame<'_>, out: &mut Vec<u8>) -> Result<(), String> {
    if frame.width != 1920
        || frame.height != 1080
        || frame.stride < 1920 * 4
        || frame
            .stride
            .checked_mul(frame.height)
            .is_none_or(|n| n > frame.pixels.len())
    {
        return Err("invalid live framebuffer dimensions, stride or length".into());
    }
    out.resize(1920 * 1088 * 3, 0);
    for (source, target) in frame
        .pixels
        .chunks_exact(frame.stride)
        .take(1080)
        .zip(out.as_chunks_mut::<{ 1920 * 3 }>().0.iter_mut())
    {
        for (pixel, rgb) in source[..1920 * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .zip(target.as_chunks_mut::<3>().0.iter_mut())
        {
            rgb.copy_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    out[1920 * 1080 * 3..].fill(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trial_rejects_bad_format_stride_and_truncation() {
        let mut mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            bits_per_pixel: 32,
            pixel_format: u32::from_le_bytes(*b"XR24"),
        };
        assert!(validate_trial_mode(mode).is_ok());
        mode.pixel_format = u32::from_le_bytes(*b"XR30");
        assert!(validate_trial_mode(mode).is_err());
        let pixels = vec![0; 1920 * 1080 * 4];
        let mut frame = Frame {
            pixels: &pixels,
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            damage: vec![],
        };
        let mut rgb = Vec::new();
        assert!(copy_trial_rgb(&frame, &mut rgb).is_ok());
        frame.stride = 1920 * 4 + 4;
        assert!(copy_trial_rgb(&frame, &mut rgb).is_err());
        frame.stride = 1920 * 4 - 1;
        assert!(copy_trial_rgb(&frame, &mut rgb).is_err());
    }

    #[test]
    fn trial_conversion_honours_stride_and_clears_padding() {
        let stride = 1920 * 4 + 16;
        let mut pixels = vec![0; stride * 1080];
        pixels[..4].copy_from_slice(&[3, 2, 1, 255]);
        pixels[stride..stride + 4].copy_from_slice(&[6, 5, 4, 255]);
        let frame = Frame {
            pixels: &pixels,
            width: 1920,
            height: 1080,
            stride,
            damage: vec![],
        };
        let mut rgb = vec![99; 1920 * 1088 * 3];
        copy_trial_rgb(&frame, &mut rgb).unwrap();
        assert_eq!(&rgb[..3], &[1, 2, 3]);
        assert_eq!(&rgb[1920 * 3..1920 * 3 + 3], &[4, 5, 6]);
        assert!(rgb[1920 * 1080 * 3..].iter().all(|&p| p == 0));
    }

    #[test]
    fn converts_xrgb8888_with_stride() {
        let pixels = [
            0x33, 0x22, 0x11, 0xff, 0x66, 0x55, 0x44, 0xff, 0xaa, 0xaa, 0xaa, 0xaa,
        ];
        let frame = Frame {
            pixels: &pixels,
            width: 2,
            height: 1,
            stride: 12,
            damage: Vec::new(),
        };
        assert_eq!(to_rgb888(&frame), [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    }
}
