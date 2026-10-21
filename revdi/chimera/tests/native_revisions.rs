//! Public-API checks against an independently materialized RGB image and the CPU codec.
//! These tests do not open capture, GPU or USB devices.

use std::sync::Arc;
use vino_chimera::kshim::{Error, EINVAL};
use vino_chimera::kvino::{DockProfile, Family};
use vino_chimera::native_changes::NativeChanges;
use vino_chimera::scanout::{CpuStripEncoder, Frame, HeadScanout, Plan, StripEncoder};
use vino_chimera::surface::{
    CursorBitmap, CursorSnapshot, NativeAllocationId, NativeSurface, SurfaceView,
};

#[derive(Clone)]
struct Image {
    width: usize,
    height: usize,
    visible: usize,
    stride: usize,
    bytes: Vec<u8>,
    cursor: CursorSnapshot,
}

impl Image {
    fn new(width: usize, height: usize, visible: usize) -> Self {
        let stride = width * 4 + 20;
        let mut bytes = vec![0xa5; stride * visible];
        for y in 0..visible {
            for x in 0..width {
                let pixel = &mut bytes[y * stride + x * 4..][..4];
                pixel.copy_from_slice(&[
                    (x * 13 + y * 11) as u8,
                    (x * 3 + y * 19) as u8,
                    (x * 7 + y * 5) as u8,
                    0xee,
                ]);
            }
        }
        Self {
            width,
            height,
            visible,
            stride,
            bytes,
            cursor: CursorSnapshot::default(),
        }
    }

    fn view(&self) -> NativeSurface<'_> {
        NativeSurface::new(
            &self.bytes,
            self.width,
            self.height,
            self.visible,
            self.stride,
            // Deliberately identical IDs for independent buffers: IDs are not pixel identity.
            NativeAllocationId { pool: 9, slot: 0 },
            Some(&self.cursor),
        )
        .unwrap()
    }

    fn change(&mut self, x: usize, y: usize) {
        self.bytes[y * self.stride + x * 4] ^= 0x7f;
    }
}

/// Independent scalar RGB/cursor oracle: no use of rgb_at, region_eq or native hashes.
fn rgb_reference(surface: NativeSurface<'_>) -> Vec<u8> {
    let mut rgb = vec![0; surface.width() * surface.height() * 3];
    for y in 0..surface.visible_height() {
        for x in 0..surface.width() {
            let pixel = &surface.pixels()[y * surface.stride() + x * 4..];
            let mut color = [pixel[2], pixel[1], pixel[0]];
            if let Some(cursor) = surface.cursor() {
                if let Some(bitmap) = cursor.bitmap() {
                    let (left, top) = cursor.position();
                    let cx = x as i64 - i64::from(left);
                    let cy = y as i64 - i64::from(top);
                    if cx >= 0
                        && cy >= 0
                        && cx < bitmap.width() as i64
                        && cy < bitmap.height() as i64
                    {
                        let p = bitmap.words()[cy as usize * bitmap.stride_words() + cx as usize];
                        let alpha = p >> 24;
                        for (channel, shift) in color.iter_mut().zip([16, 8, 0]) {
                            let fg = (p >> shift) & 255;
                            *channel =
                                ((u32::from(*channel) * (255 - alpha) + fg * alpha) / 255) as u8;
                        }
                    }
                }
            }
            rgb[(y * surface.width() + x) * 3..][..3].copy_from_slice(&color);
        }
    }
    rgb
}

#[derive(Default)]
struct NativeCpu {
    calls: Vec<Vec<(usize, usize)>>,
    fail_next: bool,
}

impl StripEncoder for NativeCpu {
    type Error = Error;

    fn encode_strips(
        &mut self,
        dock: DockProfile,
        width: usize,
        rgb: &[u8],
        coords: &[(usize, usize)],
    ) -> Result<Vec<Vec<u8>>, Error> {
        CpuStripEncoder.encode_strips(dock, width, rgb, coords)
    }

    fn encode_native_strips(
        &mut self,
        dock: DockProfile,
        surface: NativeSurface<'_>,
        coords: &[(usize, usize)],
    ) -> Result<Vec<Vec<u8>>, Error> {
        self.calls.push(coords.to_vec());
        if std::mem::take(&mut self.fail_next) {
            return Err(EINVAL);
        }
        self.encode_strips(dock, surface.width(), &rgb_reference(surface), coords)
    }
}

fn wire(frame: Option<Frame>) -> Option<(Vec<Vec<u8>>, u32)> {
    frame.map(|f| (f.records, f.presentations))
}

struct Comparison {
    dock: DockProfile,
    changes: NativeChanges,
    native: HeadScanout,
    rgb: HeadScanout,
    encoder: NativeCpu,
    previous: Option<Image>,
}

impl Comparison {
    fn new(dock: DockProfile) -> Self {
        Self {
            dock,
            changes: NativeChanges::new(dock),
            native: HeadScanout::new(dock),
            rgb: HeadScanout::new(dock),
            encoder: NativeCpu::default(),
            previous: None,
        }
    }

    fn step(&mut self, image: &Image, delivered: bool, fail_encode: bool) -> Plan {
        let surface = image.view();
        let pixels = rgb_reference(surface);
        let changed = self
            .changes
            .update(self.previous.as_ref().map(Image::view), surface)
            .unwrap();
        assert_eq!(
            changed,
            self.previous.as_ref().is_none_or(|previous| {
                (previous.width, previous.height) != (image.width, image.height)
                    || rgb_reference(previous.view()) != pixels
            })
        );
        let native = self
            .native
            .plan_native_revisions(surface, &self.changes)
            .unwrap();
        let reference = self.rgb.plan(image.width, image.height, &pixels);
        assert_eq!(native, reference);
        assert_eq!(self.native.content_changes(), self.rgb.content_changes());
        if fail_encode {
            self.encoder.fail_next = true;
            assert!(self
                .native
                .encode_view_with(&native, SurfaceView::Native(surface), 0, &mut self.encoder)
                .is_err());
            assert_eq!(
                self.native
                    .plan_native_revisions(surface, &self.changes)
                    .unwrap(),
                native
            );
        }
        assert_eq!(
            wire(
                self.native
                    .encode_view_with(&native, SurfaceView::Native(surface), 0, &mut self.encoder)
                    .unwrap()
            ),
            wire(self.rgb.encode(&reference, &pixels, 0).unwrap())
        );
        if delivered {
            self.native.presented(&native);
            self.rgb.presented(&reference);
        }
        assert_eq!(
            self.native.owes_retransmission(),
            self.rgb.owes_retransmission()
        );
        self.previous = Some(image.clone());
        native
    }

    fn settle(&mut self, image: &Image) {
        let calls = self.encoder.calls.len();
        for _ in 0..self.dock.damage_frames() {
            self.step(image, true, false);
        }
        assert_eq!(self.step(image, true, false), Plan::Idle);
        assert!(!self.native.owes_retransmission());
        assert_eq!(self.encoder.calls.len(), calls, "reuse encoded retransmits");
    }
}

#[test]
fn native_revisions_match_rgb_wire_bytes_cache_and_delivery_debt() {
    for family in [Family::Ella, Family::Ridge, Family::Navarro] {
        let dock = DockProfile::for_family(family).unwrap().steady();
        let (sw, sh) = dock.strip_dims();
        let mut image = Image::new(3 * sw, 2 * sh, 2 * sh - 3);
        let mut comparison = Comparison::new(dock);
        assert_eq!(comparison.step(&image, true, false), Plan::Keyframe);
        comparison.settle(&image);

        // An independently allocated duplicate, including changed X bytes and row padding.
        let mut duplicate = image.clone();
        for y in 0..duplicate.visible {
            for x in 0..duplicate.width {
                duplicate.bytes[y * duplicate.stride + x * 4 + 3] ^= 0xff;
            }
            duplicate.bytes[y * duplicate.stride + duplicate.width * 4..(y + 1) * duplicate.stride]
                .fill(0x91);
        }
        assert_eq!(comparison.step(&duplicate, true, false), Plan::Idle);
        image = duplicate;
        image.change(sw + 5, sh + 3);
        let calls = comparison.encoder.calls.len();
        assert_eq!(
            comparison.step(&image, false, true),
            Plan::Damage(vec![(sw, sh, 2 * sw, 2 * sh)])
        );
        assert_eq!(
            &comparison.encoder.calls[calls..],
            &[vec![(sw, sh)], vec![(sw, sh)]],
            "failed encode retries only the changed strip"
        );
        let calls = comparison.encoder.calls.len();
        comparison.step(&image, false, false); // failed USB: leave debt and cached body intact
        comparison.step(&image, true, false);
        assert_eq!(comparison.encoder.calls.len(), calls);
        comparison.settle(&image);

        // A newer capture changes another strip while the previous one still owes copies.
        image.change(1, 1);
        comparison.step(&image, true, false);
        image.change(2 * sw + 1, 1);
        let calls = comparison.encoder.calls.len();
        comparison.step(&image, true, false);
        assert_eq!(comparison.encoder.calls.len(), calls + 1);
        assert_eq!(
            comparison.encoder.calls.last().unwrap(),
            &vec![(2 * sw, 0)],
            "older strip debt reuses its body when a different strip changes"
        );
        comparison.settle(&image);

        let calls = comparison.encoder.calls.len();
        comparison.native.owe_keyframe();
        comparison.rgb.owe_keyframe();
        assert_eq!(comparison.step(&image, true, false), Plan::Keyframe);
        assert_eq!(comparison.encoder.calls.len(), calls);

        // Same strip count, different row/column layout must invalidate every cached body.
        let resized = Image::new(2 * sw, 3 * sh, 3 * sh - 1);
        assert_eq!(comparison.step(&resized, true, false), Plan::Keyframe);
        assert_eq!(comparison.encoder.calls.last().unwrap().len(), 6);
        comparison.settle(&resized);
    }
}

#[test]
fn cursor_only_changes_and_hidden_background_match_independent_composition() {
    let dock = DockProfile::for_family(Family::Ella).unwrap().steady();
    let (sw, sh) = dock.strip_dims();
    let mut image = Image::new(3 * sw, 2 * sh, 2 * sh - 2);
    let opaque = Arc::new(CursorBitmap::new(5, 5, 5, vec![0xffa0_5020; 25]).unwrap());
    let translucent = Arc::new(CursorBitmap::new(5, 5, 5, vec![0x8070_c030; 25]).unwrap());
    image.cursor = CursorSnapshot::new(Some(opaque.clone()), (2, 2), 7);
    let mut comparison = Comparison::new(dock);
    comparison.step(&image, true, false);
    comparison.settle(&image);

    // Raw pixels change underneath a fully opaque cursor; no logical image changes yet.
    image.change(3, 3);
    assert_eq!(comparison.step(&image, true, false), Plan::Idle);
    // Use the same snapshot revision deliberately: equality must inspect actual position/shape.
    image.cursor = CursorSnapshot::new(Some(opaque), (sw as i32 + 2, 2), 7);
    comparison.step(&image, true, false);
    comparison.settle(&image);
    image.cursor = CursorSnapshot::new(Some(translucent), (-2, sh as i32 - 2), 7);
    comparison.step(&image, true, false);
    comparison.settle(&image);
    image.cursor = CursorSnapshot::default();
    comparison.step(&image, true, false);
    comparison.settle(&image);
}

#[test]
fn new_tracker_and_hash_revision_domain_switches_cannot_reuse_stale_bodies() {
    let dock = DockProfile::for_family(Family::Ella).unwrap().steady();
    let mut image = Image::new(128, 32, 29);
    let mut head = HeadScanout::new(dock);
    let mut encoder = NativeCpu::default();
    let mut changes = NativeChanges::new(dock);
    changes.update(None, image.view()).unwrap();
    let plan = head.plan_native_revisions(image.view(), &changes).unwrap();
    head.encode_view_with(&plan, SurfaceView::Native(image.view()), 0, &mut encoder)
        .unwrap();
    head.presented(&plan);

    image.change(7, 3);
    // A fresh tracker starts its counters again; domain identity must force a full invalidation.
    changes = NativeChanges::new(dock);
    changes.update(None, image.view()).unwrap();
    let plan = head.plan_native_revisions(image.view(), &changes).unwrap();
    assert_eq!(plan, Plan::Keyframe);
    let reference_rgb = rgb_reference(image.view());
    let mut reference = HeadScanout::new(dock);
    let reference_plan = reference.plan(128, 32, &reference_rgb);
    assert_eq!(
        wire(
            head.encode_view_with(&plan, SurfaceView::Native(image.view()), 0, &mut encoder)
                .unwrap()
        ),
        wire(
            reference
                .encode(&reference_plan, &reference_rgb, 0)
                .unwrap()
        )
    );
    assert_eq!(encoder.calls.last().unwrap().len(), 4);
    head.presented(&plan);

    let hash_plan = head.plan(128, 32, &reference_rgb);
    assert_eq!(hash_plan, Plan::Keyframe);
    head.encode(&hash_plan, &reference_rgb, 0).unwrap();
    head.presented(&hash_plan);
    let plan = head.plan_native_revisions(image.view(), &changes).unwrap();
    assert_eq!(plan, Plan::Keyframe);
    let calls = encoder.calls.len();
    head.encode_view_with(&plan, SurfaceView::Native(image.view()), 0, &mut encoder)
        .unwrap();
    assert_eq!(encoder.calls.len(), calls + 1);
    assert_eq!(encoder.calls.last().unwrap().len(), 4);
}

#[test]
fn superseding_an_undelivered_input_never_reuses_its_encoded_pixels() {
    let dock = DockProfile::for_family(Family::Ella).unwrap().steady();
    let original = Image::new(128, 32, 29);
    let mut head = HeadScanout::new(dock);
    let mut changes = NativeChanges::new(dock);
    let mut encoder = NativeCpu::default();
    changes.update(None, original.view()).unwrap();
    let initial = head
        .plan_native_revisions(original.view(), &changes)
        .unwrap();
    head.encode_view_with(
        &initial,
        SurfaceView::Native(original.view()),
        0,
        &mut encoder,
    )
    .unwrap();
    head.presented(&initial);
    let mut altered = original.clone();
    altered.change(9, 5);
    changes
        .update(Some(original.view()), altered.view())
        .unwrap();
    let failed = head
        .plan_native_revisions(altered.view(), &changes)
        .unwrap();
    head.encode_view_with(
        &failed,
        SurfaceView::Native(altered.view()),
        0,
        &mut encoder,
    )
    .unwrap();
    // Do not call presented: this models a frame encoded but not delivered by USB.
    changes
        .update(Some(altered.view()), original.view())
        .unwrap();
    let plan = head
        .plan_native_revisions(original.view(), &changes)
        .unwrap();
    assert!(matches!(plan, Plan::Damage(_)));
    let calls = encoder.calls.len();
    let actual = head
        .encode_view_with(&plan, SurfaceView::Native(original.view()), 0, &mut encoder)
        .unwrap();
    assert_eq!(encoder.calls.len(), calls + 1);
    assert_eq!(encoder.calls.last().unwrap(), &vec![(0, 0)]);
    // Compare this conservative resend to a fresh encoding of exactly the selected rectangles.
    let rgb = rgb_reference(original.view());
    let mut fresh = HeadScanout::new(dock);
    fresh.plan(128, 32, &rgb);
    assert_eq!(wire(actual), wire(fresh.encode(&plan, &rgb, 0).unwrap()));
    head.presented(&plan);
    for _ in 0..dock.damage_frames() {
        let plan = head
            .plan_native_revisions(original.view(), &changes)
            .unwrap();
        head.encode_view_with(&plan, SurfaceView::Native(original.view()), 0, &mut encoder)
            .unwrap();
        head.presented(&plan);
    }
    assert_eq!(
        head.plan_native_revisions(original.view(), &changes)
            .unwrap(),
        Plan::Idle
    );
}

#[test]
fn planner_rejects_uninitialized_or_mismatched_revision_geometry() {
    let dock = DockProfile::for_family(Family::Ella).unwrap().steady();
    let image = Image::new(128, 32, 29);
    let resized = Image::new(64, 64, 61);
    let mut head = HeadScanout::new(dock);
    let mut changes = NativeChanges::new(dock);
    assert!(head.plan_native_revisions(image.view(), &changes).is_err());
    changes.update(None, image.view()).unwrap();
    assert!(head
        .plan_native_revisions(resized.view(), &changes)
        .is_err());
    let navarro = DockProfile::for_family(Family::Navarro).unwrap().steady();
    let mut other_layout = NativeChanges::new(navarro);
    other_layout.update(None, image.view()).unwrap();
    assert!(head
        .plan_native_revisions(image.view(), &other_layout)
        .is_err());
}
