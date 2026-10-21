// SPDX-License-Identifier: GPL-2.0-or-later

//! What to send for one presented frame, following the driver's `drm_sink/scanout.rs`.
//!
//! DLM does not stream whole frames. It hashes the surface a strip at a time, sends only the
//! macro-tiles whose content moved, and sends absolutely nothing while the desktop is still --
//! measured at 100.86 MB/s under load and exactly 0 bytes idle. The rig used to send a complete
//! 3600-strip keyframe for every frame the compositor produced, which is both orders of magnitude
//! more bytes and a different wire shape from the one the dock was designed around.
//!
//! Two pieces of state per head reproduce it:
//!
//! * **content identities** -- one per strip: exact comparison revisions for direct native
//!   capture, or hashes for other inputs. libevdi does report damage rectangles, but they
//!   describe what the client redrew, not what ended up different.
//! * **a retransmit debt** -- the dock rotates `dock_buffers` buffers, and one presentation
//!   reaches exactly one of them. A changed strip is therefore charged
//!   the dock's `damage_frames` transmissions and stays selected until it has paid them, which is
//!   what stops a strip from ghosting between an old and a new copy on alternate refreshes.
//!
//! Committed identities and debt are published only once the frame has actually reached the dock
//! ([`presented`]), so a transport failure leaves the previous dock-visible state intact and the
//! next frame repairs it.
//!
//! [`presented`]: HeadScanout::presented

use crate::kvino;
use crate::native_changes::NativeChanges;
use crate::surface::{NativeSurface, SurfaceView};
use std::sync::Arc;

/// A batch encoder must return one strip body per coordinate, in the supplied order.
/// The source is the padded, tightly packed RGB888 surface used by the scanout planner.
/// Implementations must return an error rather than truncate or return a partial batch.
pub trait StripEncoder {
    type Error: From<crate::kshim::Error>;

    /// Exact comparison for native capture. Backends may perform this on the GPU; callers
    /// retain both immutable input leases until it completes. Fill every row-major flag.
    fn compare_native_strips(
        &mut self,
        previous: NativeSurface<'_>,
        current: NativeSurface<'_>,
        strip_width: usize,
        strip_height: usize,
        changed: &mut [bool],
    ) -> Result<(), Self::Error> {
        current.changed_strips(previous, strip_width, strip_height, changed)?;
        Ok(())
    }

    fn encode_strips(
        &mut self,
        dock: kvino::DockProfile,
        width: usize,
        rgb: &[u8],
        coords: &[(usize, usize)],
    ) -> Result<Vec<Vec<u8>>, Self::Error>;

    /// Encode immutable registered native pixels. Backends must explicitly opt in;
    /// unsupported input must not hide an RGB conversion or pixel upload.
    fn encode_native_strips(
        &mut self,
        _dock: kvino::DockProfile,
        _surface: NativeSurface<'_>,
        _coords: &[(usize, usize)],
    ) -> Result<Vec<Vec<u8>>, Self::Error> {
        Err(crate::kshim::EINVAL.into())
    }
}

/// The existing serial CPU path, retained as the default.
pub struct CpuStripEncoder;

impl StripEncoder for CpuStripEncoder {
    type Error = crate::kshim::Error;

    fn encode_strips(
        &mut self,
        dock: kvino::DockProfile,
        width: usize,
        rgb: &[u8],
        coords: &[(usize, usize)],
    ) -> Result<Vec<Vec<u8>>, Self::Error> {
        coords
            .iter()
            .map(|&(x, y)| kvino::encode_strip(dock, width, rgb, x, y))
            .collect()
    }
}

/// What one presented frame should put on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Send every strip, `dock_buffers` times, so all of the dock's buffers hold this image.
    Keyframe,
    /// Send the strips these half-open `(x0, y0, x1, y1)` clips select, once.
    Damage(Vec<(usize, usize, usize, usize)>),
    /// Send nothing: no strip's content moved and no strip still owes a retransmission.
    Idle,
}

/// Cap on the number of damage rectangles handed to the codec before giving up on describing the
/// change compactly and sending the whole surface. The driver uses the same bound.
const MAX_RECTS: usize = 128;

/// One frame's worth of EP08 records, and how many times to present it.
pub struct Frame {
    pub records: Vec<Vec<u8>>,
    pub presentations: u32,
}

/// Per-head content shadow and retransmit ledger.
pub struct HeadScanout {
    /// The dock being fed: it states the strip size and how a change is spread over its buffers.
    dock: kvino::DockProfile,
    w_pad: usize,
    h_pad: usize,
    hashes: Vec<u64>,
    /// `None` means content hashes; `Some` means exact native revisions in this domain.
    /// Keep the domain alive so replacing a tracker cannot alias old cached identities.
    native_domain: Option<Arc<()>>,
    debt: Vec<u8>,
    keyframe_owed: bool,
    /// Hashes or exact native revisions for the current input, committed by `presented`.
    pending: Vec<u64>,
    /// The last encoded body of each strip, and the content identity it was encoded from.
    ///
    /// `damage_repeats` transmissions of one change are the same bytes three times over; encoding
    /// them once is what keeps a delta's cost proportional to what actually moved. A body is only
    /// served when its recorded identity still matches the strip's current content -- a body paired
    /// with newer pixels would paint stale content the dock then keeps, with nothing scheduled to
    /// repair it.
    bodies: Vec<Option<Vec<u8>>>,
    body_hashes: Vec<u64>,
}

impl HeadScanout {
    pub fn new(dock: kvino::DockProfile) -> Self {
        Self {
            dock,
            w_pad: 0,
            h_pad: 0,
            hashes: Vec::new(),
            native_domain: None,
            debt: Vec::new(),
            keyframe_owed: true,
            pending: Vec::new(),
            bodies: Vec::new(),
            body_hashes: Vec::new(),
        }
    }

    /// Require a full keyframe for the next frame.
    ///
    /// The dock's framebuffer is undefined until one arrives, so every mode-set owes this; so does
    /// anything else that leaves what the dock holds unknown.
    pub fn owe_keyframe(&mut self) {
        self.keyframe_owed = true;
    }

    /// Whether some strip still owes a transmission.
    ///
    /// A delta pays one of its the dock's `damage_frames` transmissions per presented frame, so a
    /// desktop that goes still immediately after a change would strand the rest and leave the
    /// dock's other buffers holding stale pixels -- which it shows as ghosting the moment it
    /// rotates. The caller must re-present the last surface while this holds, even though the
    /// compositor has produced nothing new.
    pub fn owes_retransmission(&self) -> bool {
        self.keyframe_owed || self.debt.iter().any(|&d| d > 0)
    }

    /// Decide what to send for `rgb`, a padded surface of `w_pad` x `h_pad` packed RGB888.
    pub fn plan(&mut self, w_pad: usize, h_pad: usize, rgb: &[u8]) -> Plan {
        self.plan_surface(
            SurfaceView::Rgb {
                width: w_pad,
                height: h_pad,
                pixels: rgb,
            },
            false,
        )
    }

    /// Plan another presentation of the same pixels without hashing the image again.
    ///
    /// The caller must know that `rgb` is byte-for-byte identical to the surface most recently
    /// passed to `plan` or `plan_unchanged` on this head, including any cursor compositing.
    /// The first surface and any resized surface are hashed regardless. An uncommitted plan's
    /// hashes are retained for retries; only `presented` commits them and pays retransmit debt.
    pub fn plan_unchanged(&mut self, w_pad: usize, h_pad: usize, rgb: &[u8]) -> Plan {
        self.plan_surface(
            SurfaceView::Rgb {
                width: w_pad,
                height: h_pad,
                pixels: rgb,
            },
            true,
        )
    }

    /// Plan a borrowed logical surface, including its immutable cursor snapshot.
    pub fn plan_view(&mut self, surface: SurfaceView<'_>) -> Plan {
        self.plan_surface(surface, false)
    }

    /// The complete logical image (background and cursor) must match the previous input.
    pub fn plan_unchanged_view(&mut self, surface: SurfaceView<'_>) -> Plan {
        self.plan_surface(surface, true)
    }

    /// Plan the exact native input most recently supplied to `changes.update`.
    /// No pixels are scanned or hashed here. Geometry and the identity domain prevent cached
    /// bodies being reused across tracker resets, counter rollover or hashed/revision switching.
    pub fn plan_native_revisions(
        &mut self,
        surface: NativeSurface<'_>,
        changes: &NativeChanges,
    ) -> Result<Plan, crate::kshim::Error> {
        if !changes.matches(surface, self.dock) {
            return Err(crate::kshim::EINVAL);
        }
        self.prepare_dimensions(surface.width(), surface.height(), Some(changes.domain()));
        self.pending.clear();
        self.pending.extend_from_slice(changes.revisions());
        Ok(self.plan_pending())
    }

    fn plan_surface(&mut self, surface: SurfaceView<'_>, unchanged: bool) -> Plan {
        let (w_pad, h_pad) = (surface.width(), surface.height());
        let (strip_w, strip_h) = self.dock.strip_dims();
        let reset = self.prepare_dimensions(w_pad, h_pad, None);
        if unchanged && !reset {
            // A successful presentation moved pending into hashes. Otherwise pending still
            // describes the latest input, including when encoding or USB delivery failed.
            if self.pending.is_empty() {
                self.pending.clone_from(&self.hashes);
            }
        } else {
            self.pending = surface_hashes(surface, strip_w, strip_h);
        }
        self.plan_pending()
    }

    fn prepare_dimensions(&mut self, w_pad: usize, h_pad: usize, domain: Option<&Arc<()>>) -> bool {
        let (strip_w, strip_h) = self.dock.strip_dims();
        let tiles_x = w_pad / strip_w;
        let tiles_y = h_pad / strip_h;
        let expected = tiles_x * tiles_y;
        let same_domain = match (&self.native_domain, domain) {
            (None, None) => true,
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            _ => false,
        };
        let reset = self.w_pad != w_pad
            || self.h_pad != h_pad
            || self.hashes.len() != expected
            || !same_domain;
        if reset {
            self.w_pad = w_pad;
            self.h_pad = h_pad;
            self.native_domain = domain.cloned();
            self.hashes = vec![0; expected];
            self.pending.clear();
            self.debt = vec![0; expected];
            self.bodies = vec![None; expected];
            self.body_hashes = vec![0; expected];
            self.keyframe_owed = true;
        }
        reset
    }

    fn plan_pending(&mut self) -> Plan {
        let (strip_w, strip_h) = self.dock.strip_dims();
        let tiles_x = self.w_pad / strip_w;
        let tiles_y = self.h_pad / strip_h;
        if self.keyframe_owed {
            return Plan::Keyframe;
        }

        // Charge every strip whose content moved, then select every strip that still owes a
        // transmission -- including ones that changed on an earlier frame and have not yet reached
        // every dock buffer.
        for (i, &hash) in self.pending.iter().enumerate() {
            if self.hashes[i] != hash {
                self.debt[i] = self.dock.damage_frames();
            }
        }
        let rects = owed_rects(&self.debt, tiles_x, tiles_y, strip_w, strip_h);
        match rects {
            None => Plan::Keyframe,
            Some(rects) if rects.is_empty() => Plan::Idle,
            Some(rects) => Plan::Damage(rects),
        }
    }

    /// Content changes since the last successful presentation, excluding retransmit debt.
    /// Call after `plan` and before `presented`. Bounds cover whole strips, not exact pixels.
    pub fn content_changes(&self) -> (usize, Option<[usize; 4]>) {
        let (sw, sh) = self.dock.strip_dims();
        let columns = self.w_pad / sw;
        let mut count = 0;
        let mut bounds = [self.w_pad, self.h_pad, 0, 0];
        for (i, (old, new)) in self.hashes.iter().zip(&self.pending).enumerate() {
            if old != new {
                let x = (i % columns) * sw;
                let y = (i / columns) * sh;
                count += 1;
                bounds[0] = bounds[0].min(x);
                bounds[1] = bounds[1].min(y);
                bounds[2] = bounds[2].max(x + sw);
                bounds[3] = bounds[3].max(y + sh);
            }
        }
        (count, (count > 0).then_some(bounds))
    }

    /// Encode the strips `plan` selects into EP08 records, reusing bodies that are still current.
    ///
    /// `rgb` must be the surface [`HeadScanout::plan`] was called with. Returns `None` when the
    /// plan selects no strip, in which case nothing at all goes on the wire.
    pub fn encode(
        &mut self,
        plan: &Plan,
        rgb: &[u8],
        head: u8,
    ) -> Result<Option<Frame>, crate::kshim::Error> {
        self.encode_with(plan, rgb, head, &mut CpuStripEncoder)
    }

    /// Encode only cache misses with the supplied batch backend. Cache state is
    /// published only after the entire batch and its record framing succeed.
    pub fn encode_with<E: StripEncoder + ?Sized>(
        &mut self,
        plan: &Plan,
        rgb: &[u8],
        head: u8,
        encoder: &mut E,
    ) -> Result<Option<Frame>, E::Error> {
        self.encode_view_with(
            plan,
            SurfaceView::Rgb {
                width: self.w_pad,
                height: self.h_pad,
                pixels: rgb,
            },
            head,
            encoder,
        )
    }

    pub fn encode_view_with<E: StripEncoder + ?Sized>(
        &mut self,
        plan: &Plan,
        surface: SurfaceView<'_>,
        head: u8,
        encoder: &mut E,
    ) -> Result<Option<Frame>, E::Error> {
        if !surface.is_valid() || surface.width() != self.w_pad || surface.height() != self.h_pad {
            return Err(crate::kshim::EINVAL.into());
        }
        let (strip_w, strip_h) = self.dock.strip_dims();
        let tiles_x = self.w_pad / strip_w;
        let (coords, presentations) = match plan {
            Plan::Idle => return Ok(None),
            Plan::Damage(clips) => (
                kvino::strip_coords(self.dock, self.w_pad, self.h_pad, Some(clips))?,
                // Consecutive copies land in the same dock buffer on most hardware, so a delta is
                // presented as few times as the dock needs and its repeats are spread across
                // later frames by the retransmit debt.
                self.dock.delta_presentations(),
            ),
            // A keyframe must reach EVERY dock buffer: one presentation updates only one of them,
            // and the ledger is cleared on the strength of this frame, so a keyframe that comes up
            // short leaves stale pixels nothing will ever repair.
            Plan::Keyframe => (
                kvino::strip_coords(self.dock, self.w_pad, self.h_pad, None)?,
                self.dock.keyframe_presentations(),
            ),
        };
        if coords.is_empty() {
            return Ok(None);
        }

        let missing: Vec<_> = coords
            .iter()
            .copied()
            .filter(|&(sx, sy)| {
                let index = (sy / strip_h) * tiles_x + (sx / strip_w);
                self.bodies[index].is_none() || self.body_hashes[index] != self.pending[index]
            })
            .collect();
        let generated = if missing.is_empty() {
            Vec::new()
        } else {
            match surface {
                SurfaceView::Rgb { pixels, .. } => {
                    encoder.encode_strips(self.dock, self.w_pad, pixels, &missing)?
                }
                SurfaceView::Native(native) => {
                    encoder.encode_native_strips(self.dock, native, &missing)?
                }
            }
        };
        if generated.len() != missing.len() {
            return Err(crate::kshim::EINVAL.into());
        }
        for (&(x, y), body) in missing.iter().zip(&generated) {
            if body.len() < 16
                || usize::from(u16::from_le_bytes([body[2], body[3]])) != x
                || usize::from(u16::from_le_bytes([body[4], body[5]])) != y
            {
                return Err(crate::kshim::EINVAL.into());
            }
        }
        let mut fresh = generated.iter();
        let mut strips = Vec::with_capacity(coords.len());
        for &(sx, sy) in &coords {
            let index = (sy / strip_h) * tiles_x + (sx / strip_w);
            let cached = self.bodies[index]
                .as_ref()
                .filter(|_| self.body_hashes[index] == self.pending[index]);
            // The native framer owns its output; borrowed input keeps every cached and
            // newly encoded body in place until framing succeeds, without a payload clone.
            strips.push(cached.unwrap_or_else(|| fresh.next().expect("checked batch length")));
        }
        order_strips_for_framing(self.dock, &mut strips);
        let records = kvino::frame_records(self.dock, &strips, head)?;
        for ((sx, sy), body) in missing.into_iter().zip(generated) {
            let index = (sy / strip_h) * tiles_x + (sx / strip_w);
            self.bodies[index] = Some(body);
            self.body_hashes[index] = self.pending[index];
        }
        Ok(Some(Frame {
            records,
            presentations,
        }))
    }

    /// Record that `plan`'s frame reached the dock.
    ///
    /// A keyframe rewrites the whole surface in every buffer, so it clears the ledger outright.
    /// A delta pays one transmission for every strip that still owed one.
    pub fn presented(&mut self, plan: &Plan) {
        match plan {
            Plan::Idle => return,
            Plan::Keyframe => {
                self.debt.iter_mut().for_each(|d| *d = 0);
                self.keyframe_owed = false;
            }
            Plan::Damage(_) => {
                for d in self.debt.iter_mut() {
                    *d = d.saturating_sub(1);
                }
            }
        }
        core::mem::swap(&mut self.hashes, &mut self.pending);
        self.pending.clear();
    }
}

/// Preorder complete strip bodies so the native framer's insertion sort has no work to do.
/// Its sort repeatedly scans the band table and is quadratic on raster input; that cost is
/// several milliseconds on a large Ella update. This stable cached-key sort uses the same
/// (band rank, x) key, including the native fallback for bands outside the measured table.
/// The original framer still constructs every wire record and remains the byte oracle.
fn order_strips_for_framing<S: AsRef<[u8]>>(dock: kvino::DockProfile, strips: &mut [S]) {
    let Some(bands) = dock.frame_band_order() else {
        return;
    };
    let mut ranks = [usize::MAX; 256];
    for (rank, &band) in bands.iter().enumerate() {
        ranks[usize::from(band)] = rank;
    }
    let (_, strip_height) = dock.strip_dims();
    strips.sort_by_cached_key(|strip| {
        let strip = strip.as_ref();
        let x = u16::from_le_bytes([strip[2], strip[3]]);
        let y = u16::from_le_bytes([strip[4], strip[5]]);
        let band = (usize::from(y) / strip_height) as u8;
        (ranks[usize::from(band)], x)
    });
}

/// Hash each strip of a packed RGB888 surface.
///
/// Only change detection rests on this, so it is a cheap multiply-xor over 8-byte words with a
/// position-dependent seed; the driver uses xxh64 for the same purpose. What matters is that two
/// different strips of pixels are overwhelmingly unlikely to agree, and that a strip's own hash is
/// stable across frames. Four neighbouring strips advance together: their independent multiply
/// chains overlap on the CPU, while each strip keeps precisely the original hash and byte order.
/// This does not add threads or change capture acknowledgement/presentation scheduling.
fn strip_hashes(rgb: &[u8], w: usize, height: usize, sw: usize, sh: usize) -> Vec<u64> {
    const PRIME: u64 = 0x9e37_79b1_85eb_ca87;
    let columns = w / sw;
    let rows = height / sh;
    let stride = w * 3;
    let span = sw * 3;
    let mut hashes = Vec::with_capacity(columns * rows);
    for y in 0..rows {
        for x in (0..columns).step_by(4) {
            let count = (columns - x).min(4);
            let mut h = [0; 4];
            for (lane, hash) in h.iter_mut().enumerate().take(count) {
                *hash = PRIME ^ ((x + lane) as u64).rotate_left(17) ^ (y as u64).rotate_left(43);
            }
            for dy in 0..sh {
                let base = (y * sh + dy) * stride + x * span;
                if count == 4 {
                    for word in 0..span / 8 {
                        for (lane, hash) in h.iter_mut().enumerate() {
                            let start = base + lane * span + word * 8;
                            let v = u64::from_le_bytes(rgb[start..start + 8].try_into().unwrap());
                            *hash = (*hash ^ v).wrapping_mul(PRIME).rotate_left(31);
                        }
                    }
                } else {
                    for word in 0..span / 8 {
                        for (lane, hash) in h.iter_mut().enumerate().take(count) {
                            let start = base + lane * span + word * 8;
                            let v = u64::from_le_bytes(rgb[start..start + 8].try_into().unwrap());
                            *hash = (*hash ^ v).wrapping_mul(PRIME).rotate_left(31);
                        }
                    }
                }
                for byte in span / 8 * 8..span {
                    for (lane, hash) in h.iter_mut().enumerate().take(count) {
                        *hash =
                            (*hash ^ u64::from(rgb[base + lane * span + byte])).wrapping_mul(PRIME);
                    }
                }
            }
            hashes.extend_from_slice(&h[..count]);
        }
    }
    hashes
}

fn surface_hashes(surface: SurfaceView<'_>, sw: usize, sh: usize) -> Vec<u64> {
    match surface {
        SurfaceView::Rgb {
            width,
            height,
            pixels,
        } => strip_hashes(pixels, width, height, sw, sh),
        SurfaceView::Native(native) => native_surface_hashes(native, sw, sh),
    }
}

/// Native storage is traversed eight pixels at a time. The three packed words
/// advance precisely the same hash as RGB888, without a temporary surface.
fn native_surface_hashes(surface: NativeSurface<'_>, sw: usize, sh: usize) -> Vec<u64> {
    const PRIME: u64 = 0x9e37_79b1_85eb_ca87;
    let columns = surface.width() / sw;
    let rows = surface.height() / sh;
    let mut hashes = Vec::with_capacity(columns * rows);
    for y in 0..rows {
        for x in (0..columns).step_by(4) {
            let count = (columns - x).min(4);
            let mut h = [0u64; 4];
            for (lane, hash) in h.iter_mut().enumerate().take(count) {
                *hash = PRIME ^ ((x + lane) as u64).rotate_left(17) ^ (y as u64).rotate_left(43);
            }
            for dy in 0..sh {
                for block in 0..sw / 8 {
                    let mut words = [[0; 3]; 4];
                    for (lane, word) in words.iter_mut().enumerate().take(count) {
                        *word = surface.rgb_words_8((x + lane) * sw + block * 8, y * sh + dy);
                    }
                    for index in 0..3 {
                        for (hash, word) in h.iter_mut().zip(&words).take(count) {
                            *hash = (*hash ^ word[index]).wrapping_mul(PRIME).rotate_left(31);
                        }
                    }
                }
                let view = SurfaceView::Native(surface);
                for word in sw / 8 * 3..sw * 3 / 8 {
                    for (lane, hash) in h.iter_mut().enumerate().take(count) {
                        *hash = (*hash
                            ^ view.rgb_word(y * sh + dy, (x + lane) * sw * 3 + word * 8))
                        .wrapping_mul(PRIME)
                        .rotate_left(31);
                    }
                }
                for byte in sw * 3 / 8 * 8..sw * 3 {
                    for (lane, hash) in h.iter_mut().enumerate().take(count) {
                        *hash = (*hash
                            ^ u64::from(view.rgb_byte(y * sh + dy, (x + lane) * sw * 3 + byte)))
                        .wrapping_mul(PRIME);
                    }
                }
            }
            hashes.extend_from_slice(&h[..count]);
        }
    }
    hashes
}

/// Turn the strips that still owe a transmission into tile-aligned damage rectangles.
///
/// Runs of owing strips within one band become one rectangle, and a rectangle directly above an
/// identical span is grown downwards rather than added -- the same merge the driver does, because
/// each rectangle costs the codec an overlap test per strip. `None` means the change is too
/// scattered to describe within [`MAX_RECTS`], and the caller should send a keyframe.
fn owed_rects(
    debt: &[u8],
    tiles_x: usize,
    tiles_y: usize,
    strip_w: usize,
    strip_h: usize,
) -> Option<Vec<(usize, usize, usize, usize)>> {
    let mut rects: Vec<(usize, usize, usize, usize)> = Vec::new();
    for ty in 0..tiles_y {
        let mut tx = 0usize;
        while tx < tiles_x {
            if debt[ty * tiles_x + tx] == 0 {
                tx += 1;
                continue;
            }
            let run_start = tx;
            while tx < tiles_x && debt[ty * tiles_x + tx] > 0 {
                tx += 1;
            }
            let (x0, x1) = (run_start * strip_w, tx * strip_w);
            let y0 = ty * strip_h;
            let y1 = y0 + strip_h;
            match rects
                .iter_mut()
                .rev()
                .find(|prior| prior.0 == x0 && prior.2 == x1 && prior.3 == y0)
            {
                Some(prior) => prior.3 = y1,
                None => {
                    if rects.len() == MAX_RECTS {
                        return None;
                    }
                    rects.push((x0, y0, x1, y1));
                }
            }
        }
    }
    Some(rects)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Component measurement only; no GPU, capture ioctl or desktop changes.
    #[test]
    #[ignore = "manual native/RGB planner component benchmark"]
    fn native_planner_component_benchmark() {
        use crate::surface::{CursorBitmap, CursorSnapshot, NativeAllocationId};
        use std::{hint::black_box, sync::Arc, time::Instant};
        let (w, h, visible) = (1920, 1088, 1080);
        let bytes: Vec<u8> = (0..w * visible * 4)
            .map(|i| ((i * 13 + i / 71) & 255) as u8)
            .collect();
        let mut rgb = vec![0; w * h * 3];
        let cursor = CursorSnapshot::new(
            Some(Arc::new(
                CursorBitmap::new(32, 32, 32, vec![0x80_ff_40_10; 1024]).unwrap(),
            )),
            (970, 520),
            1,
        );
        let native = NativeSurface::new(
            &bytes,
            w,
            h,
            visible,
            w * 4,
            NativeAllocationId { pool: 1, slot: 0 },
            None,
        )
        .unwrap();
        let with_cursor = NativeSurface::new(
            &bytes,
            w,
            h,
            visible,
            w * 4,
            NativeAllocationId { pool: 1, slot: 0 },
            Some(&cursor),
        )
        .unwrap();
        for pair in 0..3 {
            for variant in if pair % 2 == 0 { [0, 1, 2] } else { [2, 1, 0] } {
                let start = Instant::now();
                for _ in 0..80 {
                    let view = match variant {
                        0 => {
                            for (input, output) in black_box(&bytes)
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .zip(rgb.as_chunks_mut::<3>().0.iter_mut())
                            {
                                output.copy_from_slice(&[input[2], input[1], input[0]]);
                            }
                            SurfaceView::Rgb {
                                width: w,
                                height: h,
                                pixels: &rgb,
                            }
                        }
                        1 => SurfaceView::Native(native),
                        _ => SurfaceView::Native(with_cursor),
                    };
                    black_box(surface_hashes(black_box(view), 64, 16));
                }
                println!(
                    "pair={pair} mode={} ms/frame={:.4}",
                    ["convert+RGBhash", "nativehash", "nativehash+cursor"][variant],
                    start.elapsed().as_secs_f64() * 1000.0 / 80.0
                );
            }
        }
    }

    #[test]
    fn native_hashes_match_canonical_rgb_for_cursor_and_partial_groups() {
        use crate::surface::{CursorBitmap, CursorSnapshot, NativeAllocationId};
        use std::sync::Arc;
        let cursor = CursorSnapshot::new(
            Some(Arc::new(
                CursorBitmap::new(
                    5,
                    3,
                    5,
                    (0..15).map(|i| (i * 17u32) << 24 | 0x40_a0_1f).collect(),
                )
                .unwrap(),
            )),
            (3, 2),
            1,
        );
        for (sw, sh) in [(64, 16), (128, 8), (13, 7), (1, 1)] {
            for columns in [1, 2, 3, 4, 5, 7] {
                let (w, h) = (sw * columns, sh * 3);
                let visible = (h - 1).max(1);
                let stride = w * 4 + 16;
                let bytes: Vec<_> = (0..stride * visible)
                    .map(|i| ((i * 13 + i / 19) & 255) as u8)
                    .collect();
                for snapshot in [None, Some(&cursor)] {
                    let native = NativeSurface::new(
                        &bytes,
                        w,
                        h,
                        visible,
                        stride,
                        NativeAllocationId { pool: 2, slot: 0 },
                        snapshot,
                    )
                    .unwrap();
                    let mut rgb = Vec::new();
                    for y in 0..h {
                        for x in 0..w {
                            rgb.extend(native.rgb_at(x, y));
                        }
                    }
                    assert_eq!(
                        surface_hashes(SurfaceView::Native(native), sw, sh),
                        strip_hashes(&rgb, w, h, sw, sh),
                        "strip {sw}x{sh} columns={columns} cursor={}",
                        snapshot.is_some()
                    );
                }
            }
        }
    }

    #[test]
    fn native_cursor_only_changes_pay_debt_and_restore_old_pixels() {
        use crate::surface::{CursorBitmap, CursorSnapshot, NativeAllocationId};
        use std::sync::Arc;
        let pixels = [30, 20, 10, 199].repeat(128 * 16);
        let red = Arc::new(CursorBitmap::new(2, 2, 2, vec![0xffff0000; 4]).unwrap());
        let blue = Arc::new(CursorBitmap::new(2, 2, 2, vec![0xff0000ff; 4]).unwrap());
        let transparent = Arc::new(CursorBitmap::new(2, 2, 2, vec![0x00123456; 4]).unwrap());
        let states = [
            CursorSnapshot::default(),
            CursorSnapshot::new(Some(red.clone()), (5, 3), 1),
            CursorSnapshot::new(Some(red), (70, 3), 2),
            CursorSnapshot::new(Some(blue), (70, 3), 3),
            CursorSnapshot::default(),
        ];
        let mut head = HeadScanout::new(ridge());
        for (step, cursor) in states.iter().enumerate() {
            let surface = SurfaceView::Native(
                NativeSurface::new(
                    &pixels,
                    128,
                    16,
                    16,
                    128 * 4,
                    NativeAllocationId { pool: 1, slot: 0 },
                    Some(cursor),
                )
                .unwrap(),
            );
            let plan = head.plan_view(surface);
            if step > 0 {
                assert_ne!(plan, Plan::Idle, "cursor-only state transition was lost");
            }
            head.presented(&plan);
            for _ in 0..8 {
                let repeated = head.plan_unchanged_view(surface);
                head.presented(&repeated);
            }
            assert_eq!(head.plan_view(surface), Plan::Idle);
        }
        let cursor = CursorSnapshot::new(Some(transparent), (70, 3), 4);
        let surface = SurfaceView::Native(
            NativeSurface::new(
                &pixels,
                128,
                16,
                16,
                128 * 4,
                NativeAllocationId { pool: 1, slot: 0 },
                Some(&cursor),
            )
            .unwrap(),
        );
        assert_eq!(
            head.plan_view(surface),
            Plan::Idle,
            "invisible cursor changes must not alter pixel hashes"
        );
    }

    #[test]
    fn native_hashes_and_transactional_cache_match_rgb() {
        use crate::surface::NativeAllocationId;
        struct NativeReference {
            batches: usize,
            fail: bool,
        }
        impl StripEncoder for NativeReference {
            type Error = crate::kshim::Error;
            fn encode_strips(
                &mut self,
                dock: kvino::DockProfile,
                width: usize,
                rgb: &[u8],
                coords: &[(usize, usize)],
            ) -> Result<Vec<Vec<u8>>, Self::Error> {
                CpuStripEncoder.encode_strips(dock, width, rgb, coords)
            }
            fn encode_native_strips(
                &mut self,
                dock: kvino::DockProfile,
                surface: NativeSurface<'_>,
                coords: &[(usize, usize)],
            ) -> Result<Vec<Vec<u8>>, Self::Error> {
                self.batches += 1;
                if std::mem::take(&mut self.fail) {
                    return Err(crate::kshim::EINVAL);
                }
                let mut rgb = Vec::new();
                for y in 0..surface.height() {
                    for x in 0..surface.width() {
                        rgb.extend(surface.rgb_at(x, y));
                    }
                }
                self.encode_strips(dock, surface.width(), &rgb, coords)
            }
        }
        let dock = ridge();
        let (w, h, visible, stride) = (128, 32, 29, 128 * 4 + 16);
        let mut bytes = vec![0xff; stride * visible];
        let mut rgb = vec![0; w * h * 3];
        let mut native_head = HeadScanout::new(dock);
        let mut rgb_head = HeadScanout::new(dock);
        let mut encoder = NativeReference {
            batches: 0,
            fail: false,
        };
        for generation in 0..4 {
            if generation > 0 {
                bytes[(generation * 7 + 4) * stride + generation * 20] ^= 0x71;
            }
            let native = NativeSurface::new(
                &bytes,
                w,
                h,
                visible,
                stride,
                NativeAllocationId {
                    pool: 1,
                    slot: generation as u32,
                },
                None,
            )
            .unwrap();
            for y in 0..visible {
                for x in 0..w {
                    let pixel = &bytes[y * stride + x * 4..];
                    rgb[(y * w + x) * 3..][..3].copy_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                }
            }
            assert_eq!(
                surface_hashes(SurfaceView::Native(native), 64, 16),
                strip_hashes(&rgb, w, h, 64, 16)
            );
            let native_plan = native_head.plan_view(SurfaceView::Native(native));
            let rgb_plan = rgb_head.plan(w, h, &rgb);
            assert_eq!(native_plan, rgb_plan);
            if generation == 2 {
                encoder.fail = true;
                assert!(native_head
                    .encode_view_with(&native_plan, SurfaceView::Native(native), 0, &mut encoder)
                    .is_err());
                assert_eq!(
                    native_head.plan_view(SurfaceView::Native(native)),
                    native_plan
                );
            }
            let native_frame = native_head
                .encode_view_with(&native_plan, SurfaceView::Native(native), 0, &mut encoder)
                .unwrap()
                .unwrap();
            let rgb_frame = rgb_head.encode(&rgb_plan, &rgb, 0).unwrap().unwrap();
            assert_eq!(native_frame.records, rgb_frame.records);
            assert_eq!(native_frame.presentations, rgb_frame.presentations);
            native_head.presented(&native_plan);
            rgb_head.presented(&rgb_plan);
            let batches = encoder.batches;
            for _ in 0..8 {
                let a = native_head.plan_unchanged_view(SurfaceView::Native(native));
                let b = rgb_head.plan_unchanged(w, h, &rgb);
                assert_eq!(a, b);
                let a_frame = native_head
                    .encode_view_with(&a, SurfaceView::Native(native), 0, &mut encoder)
                    .unwrap();
                let b_frame = rgb_head.encode(&b, &rgb, 0).unwrap();
                assert_eq!(
                    a_frame.as_ref().map(|f| &f.records),
                    b_frame.as_ref().map(|f| &f.records)
                );
                native_head.presented(&a);
                rgb_head.presented(&b);
            }
            assert_eq!(
                encoder.batches, batches,
                "retransmit debt must reuse encoded bodies"
            );
            assert!(!native_head.owes_retransmission());
        }
    }

    #[test]
    fn parallel_strip_hashes_preserve_serial_results() {
        // Independent scalar reference pins the previous hash function, including word
        // byte order, position seeds, row boundaries, and final partial words.
        fn scalar(rgb: &[u8], w: usize, h: usize, sw: usize, sh: usize) -> Vec<u64> {
            const PRIME: u64 = 0x9e37_79b1_85eb_ca87;
            let mut hashes = Vec::new();
            for ty in 0..h / sh {
                for tx in 0..w / sw {
                    let mut hash =
                        PRIME ^ (tx as u64).rotate_left(17) ^ (ty as u64).rotate_left(43);
                    for dy in 0..sh {
                        let start = (ty * sh + dy) * w * 3 + tx * sw * 3;
                        let mut words = rgb[start..start + sw * 3].chunks_exact(8);
                        for word in &mut words {
                            hash = (hash ^ u64::from_le_bytes(word.try_into().unwrap()))
                                .wrapping_mul(PRIME)
                                .rotate_left(31);
                        }
                        for &byte in words.remainder() {
                            hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
                        }
                    }
                    hashes.push(hash);
                }
            }
            hashes
        }

        let mut random = 0x35c1_9b47_6409_7809u64;
        for (sw, sh) in [(64, 16), (128, 8), (1, 1), (13, 7)] {
            for columns in [0, 1, 2, 3, 4, 5, 7, 8, 9, 30] {
                let w = sw * columns;
                let h = sh * 3;
                let mut rgb = vec![0; w * h * 3];
                for pattern in 0..3 {
                    for byte in &mut rgb {
                        random ^= random << 13;
                        random ^= random >> 7;
                        random ^= random << 17;
                        *byte = match pattern {
                            0 => 0,
                            1 => 255,
                            _ => random as u8,
                        };
                    }
                    assert_eq!(strip_hashes(&rgb, w, h, sw, sh), scalar(&rgb, w, h, sw, sh));
                    // Changes in the last partial group/word must not be dropped.
                    if let Some(last) = rgb.last_mut() {
                        *last ^= 0x80;
                        assert_eq!(strip_hashes(&rgb, w, h, sw, sh), scalar(&rgb, w, h, sw, sh));
                    }
                }
            }
        }
    }

    fn surface(w: usize, h: usize, fill: u8) -> Vec<u8> {
        vec![fill; w * h * 3]
    }

    /// A 64x16-strip, double-buffered dock, which is what these surfaces are sized for.
    fn ridge() -> kvino::DockProfile {
        kvino::DockProfile::for_family(kvino::Family::Ridge).expect("Ridge")
    }

    #[test]
    fn first_frame_is_a_keyframe_and_clears_the_ledger() {
        let mut head = HeadScanout::new(ridge());
        let rgb = surface(128, 32, 0x20);
        assert_eq!(head.plan(128, 32, &rgb), Plan::Keyframe);
        assert_eq!(head.content_changes(), (4, Some([0, 0, 128, 32])));
        head.presented(&Plan::Keyframe);
        // Same pixels again: nothing owed, nothing sent.
        assert_eq!(head.plan(128, 32, &rgb), Plan::Idle);
        assert_eq!(head.content_changes(), (0, None));
    }

    #[test]
    fn a_changed_strip_is_resent_until_every_dock_buffer_has_it() {
        let mut head = HeadScanout::new(ridge());
        let mut rgb = surface(128, 32, 0x20);
        head.plan(128, 32, &rgb);
        head.presented(&Plan::Keyframe);

        // Repaint the top-left strip only.
        for y in 0..16 {
            let row = y * 128 * 3;
            rgb[row..row + 64 * 3].fill(0xf0);
        }
        let plan = head.plan(128, 32, &rgb);
        assert_eq!(plan, Plan::Damage(vec![(0, 0, 64, 16)]));
        assert_eq!(head.content_changes(), (1, Some([0, 0, 64, 16])));
        head.presented(&plan);

        // Still owed on the following frames, with the surface now unchanged.
        for _ in 1..ridge().damage_frames() {
            let plan = head.plan(128, 32, &rgb);
            assert_eq!(plan, Plan::Damage(vec![(0, 0, 64, 16)]));
            assert_eq!(head.content_changes(), (0, None));
            head.presented(&plan);
        }
        assert_eq!(head.plan(128, 32, &rgb), Plan::Idle);
    }

    #[test]
    fn a_reused_body_is_the_bytes_a_fresh_encode_produces() {
        let mut head = HeadScanout::new(ridge());
        let mut rgb = surface(128, 32, 0x20);
        let plan = head.plan(128, 32, &rgb);
        head.encode(&plan, &rgb, 0).unwrap();
        head.presented(&plan);

        for y in 0..16 {
            let row = y * 128 * 3;
            for (x, px) in rgb[row..row + 64 * 3].chunks_exact_mut(3).enumerate() {
                px.copy_from_slice(&[x as u8, 0x40, 0x80]);
            }
        }
        // First delta encodes the changed strip; the retransmissions it still owes serve the
        // cached body, and must put exactly the same bytes on the wire.
        let first = head.plan(128, 32, &rgb);
        let fresh = head.encode(&first, &rgb, 0).unwrap().unwrap();
        head.presented(&first);
        let second = head.plan(128, 32, &rgb);
        let cached = head.encode(&second, &rgb, 0).unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(fresh.records, cached.records);
        assert!(!fresh.records.is_empty());
    }

    #[test]
    fn a_keyframe_reaches_every_dock_buffer() {
        let mut head = HeadScanout::new(ridge());
        let rgb = surface(128, 32, 0x20);
        let plan = head.plan(128, 32, &rgb);
        let frame = head.encode(&plan, &rgb, 0).unwrap().unwrap();
        assert_eq!(frame.presentations, ridge().keyframe_presentations());
        head.presented(&plan);
        // A delta is presented once; its repeats are the debt, not back-to-back copies.
        let mut moved = rgb.clone();
        moved[..64 * 3].fill(0xff);
        let plan = head.plan(128, 32, &moved);
        assert_eq!(
            head.encode(&plan, &moved, 0)
                .unwrap()
                .unwrap()
                .presentations,
            ridge().delta_presentations()
        );
    }

    #[test]
    fn an_idle_frame_encodes_nothing() {
        let mut head = HeadScanout::new(ridge());
        let rgb = surface(128, 32, 0x20);
        let plan = head.plan(128, 32, &rgb);
        head.encode(&plan, &rgb, 0).unwrap();
        head.presented(&plan);
        let plan = head.plan(128, 32, &rgb);
        assert_eq!(plan, Plan::Idle);
        assert!(head.encode(&plan, &rgb, 0).unwrap().is_none());
    }

    #[test]
    fn a_new_surface_size_owes_a_keyframe() {
        let mut head = HeadScanout::new(ridge());
        head.plan(128, 32, &surface(128, 32, 0x20));
        head.presented(&Plan::Keyframe);
        assert_eq!(head.plan(192, 32, &surface(192, 32, 0x20)), Plan::Keyframe);
    }

    /// Compare the optimized path's wire bytes and ledger to always hashing the input.
    fn compare_unchanged(
        heads: &mut [HeadScanout; 2],
        w: usize,
        h: usize,
        rgb: &[u8],
        unchanged: bool,
        delivered: bool,
    ) -> Plan {
        let [reference, reused] = heads;
        let expected = reference.plan(w, h, rgb);
        let actual = if unchanged {
            reused.plan_unchanged(w, h, rgb)
        } else {
            reused.plan(w, h, rgb)
        };
        assert_eq!(actual, expected);
        assert_eq!(reused.content_changes(), reference.content_changes());
        let wire = |frame: Option<Frame>| frame.map(|f| (f.records, f.presentations));
        assert_eq!(
            wire(reused.encode(&actual, rgb, 0).unwrap()),
            wire(reference.encode(&expected, rgb, 0).unwrap())
        );
        if delivered {
            reference.presented(&expected);
            reused.presented(&actual);
        }
        assert_eq!(
            reused.owes_retransmission(),
            reference.owes_retransmission()
        );
        actual
    }

    #[test]
    fn unchanged_hashes_preserve_first_black_frame_repeats_keyframes_and_resize() {
        for family in [
            kvino::Family::Ridge,
            kvino::Family::Navarro,
            kvino::Family::Ella,
        ] {
            let dock = kvino::DockProfile::for_family(family).unwrap().steady();
            let (sw, sh) = dock.strip_dims();
            let (w, h) = (3 * sw, 2 * sh);
            let mut heads = [HeadScanout::new(dock), HeadScanout::new(dock)];
            let mut rgb = surface(w, h, 0);
            // Exact capture comparison can report no change for the first all-black surface.
            assert_eq!(
                compare_unchanged(&mut heads, w, h, &rgb, true, true),
                Plan::Keyframe
            );
            // Changes in different strips arrive before the earlier debt has settled.
            for pixel in [0, w * 3 - 1, rgb.len() - 1] {
                rgb[pixel] = 0xff;
                assert!(matches!(
                    compare_unchanged(&mut heads, w, h, &rgb, false, true),
                    Plan::Damage(_)
                ));
                compare_unchanged(&mut heads, w, h, &rgb, true, true);
            }
            for _ in 0..dock.damage_frames() {
                compare_unchanged(&mut heads, w, h, &rgb, true, true);
            }
            assert_eq!(
                compare_unchanged(&mut heads, w, h, &rgb, true, true),
                Plan::Idle
            );
            for head in &mut heads {
                head.owe_keyframe();
            }
            assert_eq!(
                compare_unchanged(&mut heads, w, h, &rgb, true, true),
                Plan::Keyframe
            );
            // Same byte/strip count, different geometry: the old hashes must be discarded.
            assert_eq!(
                compare_unchanged(&mut heads, 2 * sw, 3 * sh, &rgb, true, true),
                Plan::Keyframe
            );
        }
    }

    #[test]
    fn unchanged_retries_preserve_uncommitted_pixels_and_retransmit_debt() {
        let dock = kvino::DockProfile::for_family(kvino::Family::Ella)
            .unwrap()
            .steady();
        let mut heads = [HeadScanout::new(dock), HeadScanout::new(dock)];
        let mut rgb = surface(128, 32, 0);
        // Failure after encoding a first keyframe must still leave a keyframe owed.
        compare_unchanged(&mut heads, 128, 32, &rgb, true, false);
        assert_eq!(
            compare_unchanged(&mut heads, 128, 32, &rgb, true, true),
            Plan::Keyframe
        );
        rgb[0] = 0xff;
        let delta = compare_unchanged(&mut heads, 128, 32, &rgb, false, false);
        assert_eq!(delta, Plan::Damage(vec![(0, 0, 64, 16)]));
        // A failed USB delivery must not reuse the previously committed image's hashes or
        // pay down the new image's debt. Retry twice before accepting delivery.
        assert_eq!(
            compare_unchanged(&mut heads, 128, 32, &rgb, true, false),
            delta
        );
        for _ in 0..dock.damage_frames() {
            assert_eq!(
                compare_unchanged(&mut heads, 128, 32, &rgb, true, true),
                delta
            );
        }
        assert_eq!(
            compare_unchanged(&mut heads, 128, 32, &rgb, true, true),
            Plan::Idle
        );
        // New pixels supersede an undelivered pending image, including a return to old pixels.
        rgb[0] = 0x80;
        compare_unchanged(&mut heads, 128, 32, &rgb, false, false);
        rgb[0] = 0xff;
        compare_unchanged(&mut heads, 128, 32, &rgb, false, true);
        for _ in 0..dock.damage_frames() {
            compare_unchanged(&mut heads, 128, 32, &rgb, true, true);
        }
        assert!(!heads[1].owes_retransmission());
    }

    #[test]
    fn ella_optimized_keyframes_match_the_original_native_framer() {
        let dock = kvino::DockProfile::for_family(kvino::Family::Ella)
            .unwrap()
            .steady();
        let rgb = surface(1920, 1088, 0x42);
        let strips: Vec<_> = kvino::strip_coords(dock, 1920, 1088, None)
            .unwrap()
            .iter()
            .map(|&(x, y)| kvino::encode_strip(dock, 1920, &rgb, x, y).unwrap())
            .collect();
        for connector in 0..2 {
            let expected = kvino::frame_records(dock, &strips, connector).unwrap();
            let mut head = HeadScanout::new(dock);
            let plan = head.plan(1920, 1088, &rgb);
            let actual = head.encode(&plan, &rgb, connector).unwrap().unwrap();
            assert_eq!(actual.records, expected);
        }
    }

    #[test]
    fn ella_presorting_preserves_sparse_duplicate_and_unlisted_band_bytes() {
        let dock = kvino::DockProfile::for_family(kvino::Family::Ella)
            .unwrap()
            .steady();
        let rgb = surface(128, 1120, 0x55);
        let mut sparse = Vec::new();
        for y in (0..1120).step_by(48) {
            for x in [64, 0] {
                sparse.push(kvino::encode_strip(dock, 128, &rgb, x, y).unwrap());
            }
        }
        // Native sorting is stable for duplicate coordinates and unknown bands with equal x.
        sparse.push(kvino::encode_strip(dock, 128, &surface(128, 1120, 0x77), 0, 0).unwrap());
        sparse.push(kvino::encode_strip(dock, 128, &rgb, 0, 1104).unwrap());
        sparse.push(kvino::encode_strip(dock, 128, &rgb, 0, 1088).unwrap());
        for mut input in [vec![], sparse.clone(), sparse.into_iter().rev().collect()] {
            for connector in 0..2 {
                let expected = kvino::frame_records(dock, &input, connector).unwrap();
                order_strips_for_framing(dock, &mut input);
                assert_eq!(
                    kvino::frame_records(dock, &input, connector).unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn other_dock_families_keep_their_original_input_order() {
        for family in [kvino::Family::Ridge, kvino::Family::Navarro] {
            let dock = kvino::DockProfile::for_family(family).unwrap();
            let mut strips = vec![vec![1, 2, 3], vec![4, 5, 6]];
            let expected = strips.clone();
            order_strips_for_framing(dock, &mut strips);
            assert_eq!(strips, expected);
        }
    }

    #[test]
    fn owed_runs_merge_across_bands() {
        // Two vertically adjacent bands owing the same span become one rectangle.
        let debt = vec![1, 1, 0, 1, 1, 0];
        assert_eq!(owed_rects(&debt, 3, 2, 64, 16), Some(vec![(0, 0, 128, 32)]));
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    struct PartialBatch;
    impl StripEncoder for PartialBatch {
        type Error = crate::kshim::Error;
        fn encode_strips(
            &mut self,
            _: kvino::DockProfile,
            _: usize,
            _: &[u8],
            _: &[(usize, usize)],
        ) -> Result<Vec<Vec<u8>>, Self::Error> {
            Ok(vec![])
        }
    }

    #[test]
    fn failed_batch_does_not_publish_cache_or_presentation() {
        let dock = kvino::DockProfile::for_family(kvino::Family::Ella).unwrap();
        let rgb = vec![127; 128 * 16 * 3];
        let mut scanout = HeadScanout::new(dock);
        let plan = scanout.plan(128, 16, &rgb);
        assert!(scanout
            .encode_with(&plan, &rgb, 0, &mut PartialBatch)
            .is_err());
        assert!(scanout.bodies.iter().all(Option::is_none));
        assert!(scanout.keyframe_owed);
        let retry = scanout.plan_unchanged(128, 16, &rgb);
        assert_eq!(retry, plan);
        let recovered = scanout.encode(&retry, &rgb, 0).unwrap().unwrap();
        let mut reference = HeadScanout::new(dock);
        let expected_plan = reference.plan(128, 16, &rgb);
        let expected = reference.encode(&expected_plan, &rgb, 0).unwrap().unwrap();
        assert_eq!(recovered.records, expected.records);
        assert_eq!(recovered.presentations, expected.presentations);
    }

    #[test]
    fn failed_framing_keeps_cached_bodies_and_new_hashes_separate() {
        struct OversizedStrip;
        impl StripEncoder for OversizedStrip {
            type Error = crate::kshim::Error;

            fn encode_strips(
                &mut self,
                _: kvino::DockProfile,
                _: usize,
                _: &[u8],
                coords: &[(usize, usize)],
            ) -> Result<Vec<Vec<u8>>, Self::Error> {
                Ok(coords
                    .iter()
                    .map(|&(x, y)| {
                        let mut body = vec![0; 4096];
                        body[2..4].copy_from_slice(&(x as u16).to_le_bytes());
                        body[4..6].copy_from_slice(&(y as u16).to_le_bytes());
                        body
                    })
                    .collect())
            }
        }

        let dock = kvino::DockProfile::for_family(kvino::Family::Ella).unwrap();
        let mut rgb = vec![127; 128 * 16 * 3];
        let mut scanout = HeadScanout::new(dock);
        let first = scanout.plan(128, 16, &rgb);
        scanout.encode(&first, &rgb, 0).unwrap();
        scanout.presented(&first);
        let old_bodies = scanout.bodies.clone();
        let old_hashes = scanout.body_hashes.clone();

        rgb[0] ^= 0xff;
        // Include the untouched cached strip alongside the changed, oversized new body.
        scanout.owe_keyframe();
        let changed = scanout.plan(128, 16, &rgb);
        assert!(scanout
            .encode_with(&changed, &rgb, 0, &mut OversizedStrip)
            .is_err());
        assert_eq!(scanout.bodies, old_bodies);
        assert_eq!(scanout.body_hashes, old_hashes);
        assert!(scanout.keyframe_owed);

        let retry = scanout.plan_unchanged(128, 16, &rgb);
        let recovered = scanout.encode(&retry, &rgb, 0).unwrap().unwrap();
        let mut reference = HeadScanout::new(dock);
        let expected = reference.plan(128, 16, &rgb);
        let expected = reference.encode(&expected, &rgb, 0).unwrap().unwrap();
        assert_eq!(recovered.records, expected.records);
        assert_eq!(recovered.presentations, expected.presentations);
    }
}
