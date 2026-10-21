// SPDX-License-Identifier: GPL-2.0-or-later

//! Trial-local cursor composition. Keep the desktop image clean so cursor-only updates do not
//! require GRABPIX (which also completes EVDI's pending page flip).

use crate::revdi::Cursor;
use crate::surface::{CursorBitmap, CursorSnapshot};
use std::sync::Arc;

#[derive(Default)]
pub struct CursorPlane {
    image: Option<Cursor>,
    position: (i32, i32),
    changed: bool,
    snapshot_bitmap: Option<Arc<CursorBitmap>>,
    revision: u64,
}

impl CursorPlane {
    pub fn set_image(&mut self, image: Cursor) -> Result<(), String> {
        if image.enabled {
            let row = (image.width as usize).checked_mul(4);
            let length = (image.stride as usize).checked_mul(image.height as usize);
            if image.width == 0
                || image.height == 0
                || row.is_none_or(|row| row > image.stride as usize)
                || length.is_none_or(|length| length > image.pixels.len())
                || ![*b"AR24", *b"XR24", *b"AB24", *b"XB24"]
                    .map(u32::from_le_bytes)
                    .contains(&image.pixel_format)
            {
                return Err("unsupported cursor format or invalid cursor buffer geometry".into());
            }
            let format = image.pixel_format.to_le_bytes();
            let red_first = format == *b"AB24" || format == *b"XB24";
            let mut words = Vec::with_capacity(image.width as usize * image.height as usize);
            for y in 0..image.height as usize {
                for x in 0..image.width as usize {
                    let pixel = &image.pixels[y * image.stride as usize + x * 4..];
                    let alpha = if format[0] == b'X' {
                        255
                    } else {
                        u32::from(pixel[3])
                    };
                    let (red, blue) = if red_first {
                        (pixel[0], pixel[2])
                    } else {
                        (pixel[2], pixel[0])
                    };
                    words.push(
                        (alpha << 24)
                            | (u32::from(red) << 16)
                            | (u32::from(pixel[1]) << 8)
                            | u32::from(blue),
                    );
                }
            }
            self.snapshot_bitmap = Some(Arc::new(
                CursorBitmap::new(
                    image.width as usize,
                    image.height as usize,
                    image.width as usize,
                    words,
                )
                .map_err(|_| "invalid normalized cursor bitmap")?,
            ));
            self.image = Some(image);
        } else {
            // A hide event need not contain a bitmap or valid geometry.
            self.image = None;
            self.snapshot_bitmap = None;
        }
        self.changed = true;
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    pub fn move_to(&mut self, x: i32, y: i32) {
        if self.position != (x, y) {
            // EVDI reports the bitmap's top-left, already adjusted for its hotspot.
            self.position = (x, y);
            self.changed = true;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn snapshot(&self) -> CursorSnapshot {
        CursorSnapshot::new(self.snapshot_bitmap.clone(), self.position, self.revision)
    }

    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    pub fn is_changed(&self) -> bool {
        self.changed
    }

    /// Restore the clean desktop before blending, including after a move, hide or shape change.
    /// Rows below `height` are codec padding and must never receive cursor pixels.
    pub fn compose(
        &self,
        desktop: &[u8],
        output: &mut [u8],
        width: usize,
        height: usize,
    ) -> Result<(), String> {
        if output.len() != desktop.len()
            || width
                .checked_mul(height)
                .and_then(|pixels| pixels.checked_mul(3))
                .is_none_or(|length| length > desktop.len())
        {
            return Err("invalid cursor composition surface".into());
        }
        output.copy_from_slice(desktop);
        let Some(image) = &self.image else {
            return Ok(());
        };
        let (left, top) = (i64::from(self.position.0), i64::from(self.position.1));
        let right = (left + i64::from(image.width)).min(width as i64);
        let bottom = (top + i64::from(image.height)).min(height as i64);
        let format = image.pixel_format.to_le_bytes();
        let red_first = format == *b"AB24" || format == *b"XB24";
        for y in top.max(0)..bottom {
            for x in left.max(0)..right {
                let source = (y - top) as usize * image.stride as usize + (x - left) as usize * 4;
                let pixel = &image.pixels[source..source + 4];
                let alpha = if format[0] == b'X' {
                    255
                } else {
                    u32::from(pixel[3])
                };
                let rgb = if red_first {
                    [pixel[0], pixel[1], pixel[2]]
                } else {
                    [pixel[2], pixel[1], pixel[0]]
                };
                let target = (y as usize * width + x as usize) * 3;
                for (background, foreground) in output[target..target + 3].iter_mut().zip(rgb) {
                    // Match EVDI 1.15.1's blend_component, including integer rounding.
                    let mixed =
                        u32::from(*background) * (255 - alpha) + u32::from(foreground) * alpha;
                    *background = ((mixed + ((mixed + 1) << 8)) >> 16) as u8;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32, stride: u32, pixels: Vec<u8>) -> Cursor {
        Cursor {
            // Nonzero hotspots ensure we do not apply the adjustment twice.
            hot_x: 7,
            hot_y: 9,
            width,
            height,
            enabled: true,
            pixels,
            pixel_format: u32::from_le_bytes(*b"AR24"),
            stride,
        }
    }

    #[test]
    fn immutable_native_cursor_view_matches_existing_rgb_compositor() {
        use crate::surface::{NativeAllocationId, NativeSurface};
        let (width, visible, height, stride) = (7, 4, 6, 7 * 4 + 8);
        let mut bytes = vec![0x55; stride * visible];
        let mut desktop = vec![0; width * height * 3];
        let mut cursor = CursorPlane::default();
        let mut output = vec![0; desktop.len()];
        for format in [*b"AR24", *b"XR24", *b"AB24", *b"XB24"] {
            let mut shape = image(
                2,
                2,
                12,
                vec![
                    200, 40, 80, 128, 2, 3, 4, 0, 99, 99, 99, 99, 8, 15, 27, 255, 77, 51, 34, 1,
                    99, 99, 99, 99,
                ],
            );
            shape.pixel_format = u32::from_le_bytes(format);
            cursor.set_image(shape).unwrap();
            for (x, y) in [
                (0, 0),
                (2, 1),
                (-1, -1),
                (6, 3),
                (i32::MIN, 0),
                (i32::MAX, i32::MAX),
            ] {
                cursor.move_to(x, y);
                let snapshot = cursor.snapshot();
                for generation in 0..3 {
                    // Change pixels beneath an unmoving cursor. Composition must always use
                    // the new clean background, never accumulate alpha over prior output.
                    for yy in 0..visible {
                        for xx in 0..width {
                            let rgb = [
                                (xx * 19 + generation * 17) as u8,
                                (yy * 31 + generation * 29) as u8,
                                100,
                            ];
                            desktop[(yy * width + xx) * 3..][..3].copy_from_slice(&rgb);
                            bytes[yy * stride + xx * 4..][..3]
                                .copy_from_slice(&[rgb[2], rgb[1], rgb[0]]);
                        }
                    }
                    cursor
                        .compose(&desktop, &mut output, width, visible)
                        .unwrap();
                    let native = NativeSurface::new(
                        &bytes,
                        width,
                        height,
                        visible,
                        stride,
                        NativeAllocationId { pool: 1, slot: 0 },
                        Some(&snapshot),
                    )
                    .unwrap();
                    for yy in 0..height {
                        for xx in 0..width {
                            assert_eq!(native.rgb_at(xx, yy), output[(yy * width + xx) * 3..][..3]);
                        }
                    }
                }
            }
        }
        let previous = cursor.snapshot();
        let old_revision = previous.bitmap().unwrap().shape_revision();
        cursor
            .set_image(image(1, 1, 4, vec![0, 0, 255, 255]))
            .unwrap();
        assert_ne!(
            cursor.snapshot().bitmap().unwrap().shape_revision(),
            old_revision
        );
        assert_eq!(previous.bitmap().unwrap().shape_revision(), old_revision);
        let mut hide = image(0, 0, 0, vec![]);
        hide.enabled = false;
        cursor.set_image(hide).unwrap();
        assert!(cursor.snapshot().bitmap().is_none());
        assert!(
            previous.bitmap().is_some(),
            "a published snapshot survives shape changes/hide"
        );
    }

    #[test]
    fn moving_hiding_and_replacing_cursor_restore_the_clean_desktop() {
        let desktop = vec![40; 4 * 3];
        let mut output = vec![0; desktop.len()];
        let mut cursor = CursorPlane::default();
        cursor
            .set_image(image(1, 1, 4, vec![0, 0, 255, 255]))
            .unwrap();
        cursor.compose(&desktop, &mut output, 4, 1).unwrap();
        assert_eq!(&output[..3], &[255, 0, 0]);
        cursor.move_to(2, 0);
        cursor.compose(&desktop, &mut output, 4, 1).unwrap();
        assert_eq!(&output[..6], &[40; 6]);
        assert_eq!(&output[6..9], &[255, 0, 0]);
        cursor
            .set_image(image(1, 1, 4, vec![255, 0, 0, 255]))
            .unwrap();
        cursor.compose(&desktop, &mut output, 4, 1).unwrap();
        assert_eq!(&output[6..9], &[0, 0, 255]);
        let mut hidden = image(0, 0, 0, vec![]);
        hidden.enabled = false;
        cursor.set_image(hidden).unwrap();
        cursor.compose(&desktop, &mut output, 4, 1).unwrap();
        assert_eq!(output, desktop);
    }

    #[test]
    fn blends_alpha_and_honours_bitmap_stride() {
        let mut cursor = CursorPlane::default();
        cursor
            .set_image(image(
                1,
                2,
                8,
                vec![200, 40, 80, 128, 99, 99, 99, 99, 2, 3, 4, 0, 99, 99, 99, 99],
            ))
            .unwrap();
        let desktop = [20, 60, 100, 30, 70, 110];
        let mut output = [0; 6];
        cursor.compose(&desktop, &mut output, 1, 2).unwrap();
        assert_eq!(output, [50, 49, 150, 30, 70, 110]);
        // Repeated composition must not accumulate alpha over an old cursor.
        cursor.compose(&desktop, &mut output, 1, 2).unwrap();
        assert_eq!(output, [50, 49, 150, 30, 70, 110]);
    }

    #[test]
    fn clips_all_edges_without_drawing_into_codec_padding() {
        let mut cursor = CursorPlane::default();
        cursor
            .set_image(image(2, 2, 8, [0, 255, 0, 255].repeat(4)))
            .unwrap();
        let desktop = [10; 3 * 3 * 3];
        let mut output = [0; 27];
        for (x, y, count) in [
            (-1, -1, 1),
            (2, 1, 1),
            (i32::MIN, 0, 0),
            (i32::MAX, i32::MAX, 0),
        ] {
            cursor.move_to(x, y);
            cursor.compose(&desktop, &mut output, 3, 2).unwrap();
            assert_eq!(
                output[..18]
                    .chunks_exact(3)
                    .filter(|p| *p == [0, 255, 0])
                    .count(),
                count
            );
            assert_eq!(&output[18..], &[10; 9]);
        }
    }

    #[test]
    fn coalesces_position_changes_and_rejects_invalid_bitmaps() {
        let mut cursor = CursorPlane::default();
        cursor.move_to(1, 1);
        cursor.move_to(2, 2);
        assert!(cursor.take_changed());
        cursor.move_to(2, 2);
        assert!(!cursor.take_changed());
        assert!(cursor.set_image(image(2, 2, 4, vec![0; 8])).is_err());
        assert!(cursor.set_image(image(2, 2, 8, vec![0; 15])).is_err());
        let mut unsupported = image(1, 1, 4, vec![0; 4]);
        unsupported.pixel_format = u32::from_le_bytes(*b"AR30");
        assert!(cursor.set_image(unsupported).is_err());
    }
}
