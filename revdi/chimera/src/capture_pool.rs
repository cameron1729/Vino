// SPDX-License-Identifier: GPL-2.0-or-later

//! Four bounded, owned capture destinations. Only the producer's writer may change pixels;
//! published leases remain immutable until both CPU and GPU readers have finished.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, MutexGuard,
};
use std::time::{Duration, Instant};

pub const CAPTURE_SLOTS: usize = 4;
static NEXT_POOL: AtomicU64 = AtomicU64::new(1);

/// Stable, mapped storage owned for the entire lifetime of this object.
///
/// # Safety
/// The pointer must address `len()` initialized, writable bytes and remain stable until drop.
/// The allocation must not be supplied to another pool or accessed independently of this
/// pool's leases. GPU reads are allowed only while retaining the corresponding `FrameLease`.
/// Dropping the object must keep any parent GPU device alive until its allocation is released.
pub unsafe trait CaptureStorage: Send + Sync {
    fn mapped_ptr(&self) -> *mut u8;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureRect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PoolStats {
    pub repair_bytes: u64,
    pub repair_time: Duration,
    pub peak_leases: u64,
    pub exhaustions: u64,
}

#[derive(Clone, Copy, Default)]
struct Span {
    left: usize,
    right: usize,
}
impl Span {
    fn include(&mut self, left: usize, right: usize) {
        if left >= right {
            return;
        }
        if self.left == self.right {
            *self = Self { left, right };
        } else {
            self.left = self.left.min(left);
            self.right = self.right.max(right);
        }
    }
}

struct SlotState {
    readers: usize,
    writing: bool,
    stale: Vec<Span>,
}
struct State {
    slots: Vec<SlotState>,
    baseline: Option<usize>,
    writer: bool,
    stats: PoolStats,
}

pub struct CapturePool {
    id: u64,
    width: usize,
    height: usize,
    stride: usize,
    length: usize,
    storage: Vec<Arc<dyn CaptureStorage>>,
    state: Mutex<State>,
}

impl CapturePool {
    /// # Safety
    /// These allocations must not be registered in another pool or accessed independently
    /// while this pool lives. Retained allocator/GPU references may only use pixels through
    /// the corresponding read/write lease; registration alone does not grant pixel access.
    pub unsafe fn new(
        storage: Vec<Arc<dyn CaptureStorage>>,
        width: usize,
        height: usize,
        stride: usize,
    ) -> Result<Arc<Self>, String> {
        let padded = height.checked_add(15).map(|n| n / 16 * 16);
        let length = padded.and_then(|n| stride.checked_mul(n));
        if storage.len() != CAPTURE_SLOTS
            || width == 0
            || height == 0
            || width > i32::MAX as usize
            || height > i32::MAX as usize
            || width.checked_mul(4).is_none_or(|n| n > stride)
            || length.is_none()
        {
            return Err("invalid capture pool geometry or slot count".into());
        }
        let length = length.unwrap();
        let mut ranges = Vec::new();
        for item in &storage {
            let start = item.mapped_ptr() as usize;
            let end = start.checked_add(length);
            if start == 0
                || item.len() < length
                || end.is_none()
                || ranges.iter().any(|&(a, b)| start < b && a < end.unwrap())
            {
                return Err("invalid, undersized or overlapping capture storage".into());
            }
            ranges.push((start, end.unwrap()));
        }
        for item in &storage {
            // SAFETY: the unsafe storage contract transfers exclusive pixel access to this
            // pool, and checked ranges are disjoint. This also clears all codec padding.
            unsafe { std::ptr::write_bytes(item.mapped_ptr(), 0, length) };
        }
        Ok(Arc::new(Self {
            id: NEXT_POOL.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            stride,
            length,
            storage,
            state: Mutex::new(State {
                slots: (0..CAPTURE_SLOTS)
                    .map(|_| SlotState {
                        readers: 0,
                        writing: false,
                        stale: vec![Span::default(); height],
                    })
                    .collect(),
                baseline: None,
                writer: false,
                stats: PoolStats::default(),
            }),
        }))
    }

    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn stride(&self) -> usize {
        self.stride
    }
    pub fn stats(&self) -> PoolStats {
        self.lock().stats
    }

    /// Registration retains storage ownership but does not grant permission to write.
    /// Only a `WriteLease` for this slot permits a synchronous EVDI grab.
    pub(crate) fn storage(&self, slot: usize) -> Arc<dyn CaptureStorage> {
        Arc::clone(&self.storage[slot])
    }

    pub fn acquire(self: &Arc<Self>) -> Result<WriteLease, String> {
        let mut state = self.lock();
        if state.writer {
            return Err("capture pool already has a pending writer".into());
        }
        let slot = (0..CAPTURE_SLOTS)
            .filter(|&i| {
                !state.slots[i].writing && state.slots[i].readers == 0 && state.baseline != Some(i)
            })
            .min_by_key(|&i| {
                state.slots[i]
                    .stale
                    .iter()
                    .map(|s| s.right - s.left)
                    .sum::<usize>()
            });
        let Some(slot) = slot else {
            state.stats.exhaustions += 1;
            return Err("capture pool exhausted: too many retained frame leases".into());
        };
        let initializing = state.baseline.is_none();
        state.writer = true;
        state.slots[slot].writing = true;
        Ok(WriteLease {
            pool: Arc::clone(self),
            slot,
            active: true,
            initialized: initializing.then(|| vec![0; (self.width * self.height).div_ceil(64)]),
            initialized_count: 0,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// An immutable image. Keep this lease alive through GPU submission and fence completion.
/// Deliberately not Clone: the consumer keeps only its current and one replacement image.
pub struct FrameLease {
    pool: Arc<CapturePool>,
    slot: usize,
}
impl FrameLease {
    pub fn pixels(&self) -> &[u8] {
        // SAFETY: a reader prevents writer reservation, and all producers honor that state.
        unsafe {
            std::slice::from_raw_parts(self.pool.storage[self.slot].mapped_ptr(), self.pool.length)
        }
    }
    pub fn pool_id(&self) -> u64 {
        self.pool.id
    }
    pub fn slot_id(&self) -> u32 {
        self.slot as u32
    }
}
impl Drop for FrameLease {
    fn drop(&mut self) {
        let mut state = self.pool.lock();
        debug_assert!(state.slots[self.slot].readers > 0);
        state.slots[self.slot].readers -= 1;
    }
}

/// The sole write permission for one destination, including an outstanding EVDI request.
pub struct WriteLease {
    pool: Arc<CapturePool>,
    slot: usize,
    active: bool,
    initialized: Option<Vec<u64>>,
    initialized_count: usize,
}
impl WriteLease {
    pub fn pool_id(&self) -> u64 {
        self.pool.id
    }

    pub fn slot_id(&self) -> u32 {
        self.slot as u32
    }

    /// Pixel access for a synchronous capture provider. No reader can observe this slot.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        assert!(self.active);
        // SAFETY: this unique writer is reserved until publication/drop, and its allocation
        // is never the current repair baseline or an active reader.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.pool.storage[self.slot].mapped_ptr(),
                self.pool.length,
            )
        }
    }

    /// Repair untouched stale pixels, then publish an immutable complete frame. Initial
    /// partial grabs return None until every visible pixel has actually been captured.
    pub fn complete_grab(&mut self, damage: &[CaptureRect]) -> Result<Option<FrameLease>, String> {
        if !self.active {
            return Err("capture writer was already published".into());
        }
        let started = Instant::now();
        if damage.len() > 16 {
            return Err("capture supplied more than sixteen damage rectangles".into());
        }
        let mut rectangles = Vec::with_capacity(damage.len());
        for rect in damage {
            let left = rect.x1.clamp(0, self.pool.width as i32) as usize;
            let right = rect.x2.clamp(0, self.pool.width as i32) as usize;
            let top = rect.y1.clamp(0, self.pool.height as i32) as usize;
            let bottom = rect.y2.clamp(0, self.pool.height as i32) as usize;
            if left < right && top < bottom {
                rectangles.push((left, right, top, bottom));
            }
        }
        if rectangles.is_empty() {
            return Ok(None);
        }
        rectangles.sort_unstable();
        let full = rectangles.iter().any(|&(left, right, top, bottom)| {
            left == 0 && right == self.pool.width && top == 0 && bottom == self.pool.height
        });
        let first = self.initialized.is_some();
        if let Some(bits) = self.initialized.as_mut() {
            if full {
                self.initialized_count = self.pool.width * self.pool.height;
            } else {
                for &(left, right, top, bottom) in &rectangles {
                    for y in top..bottom {
                        for pixel in y * self.pool.width + left..y * self.pool.width + right {
                            let mask = 1_u64 << (pixel % 64);
                            self.initialized_count += usize::from(bits[pixel / 64] & mask == 0);
                            bits[pixel / 64] |= mask;
                        }
                    }
                }
            }
            if self.initialized_count != self.pool.width * self.pool.height {
                return Ok(None);
            }
        }
        let (baseline, stale) = {
            let state = self.pool.lock();
            (
                state.baseline,
                if full {
                    Vec::new()
                } else {
                    state.slots[self.slot].stale.clone()
                },
            )
        };
        let mut repaired = 0_u64;
        if let Some(baseline) = baseline {
            let source = self.pool.storage[baseline].mapped_ptr();
            let target = self.pool.storage[self.slot].mapped_ptr();
            for (y, span) in stale.iter().enumerate() {
                let mut left = span.left;
                let mut copy = |a: usize, b: usize| {
                    if a < b {
                        let offset = y * self.pool.stride + a * 4;
                        let bytes = (b - a) * 4;
                        // SAFETY: source is the immutable baseline, destination is exclusively
                        // reserved, and spans are clipped to disjoint valid allocations.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                source.add(offset),
                                target.add(offset),
                                bytes,
                            );
                        }
                        repaired += bytes as u64;
                    }
                };
                // Rectangles are sorted by x, so subtraction needs no per-row allocation.
                for &(a, b, top, bottom) in &rectangles {
                    if y < top || y >= bottom {
                        continue;
                    }
                    if a > left {
                        copy(left, a.min(span.right));
                    }
                    left = left.max(b);
                    if left >= span.right {
                        break;
                    }
                }
                copy(left, span.right);
            }
        }
        let mut state = self.pool.lock();
        for (i, slot) in state.slots.iter_mut().enumerate() {
            if i == self.slot {
                slot.stale.fill(Span::default());
            } else if first || full {
                slot.stale.fill(Span {
                    left: 0,
                    right: self.pool.width,
                });
            } else {
                for &(left, right, top, bottom) in &rectangles {
                    for span in &mut slot.stale[top..bottom] {
                        span.include(left, right);
                    }
                }
            }
        }
        state.slots[self.slot].writing = false;
        state.slots[self.slot].readers += 1;
        state.baseline = Some(self.slot);
        state.writer = false;
        state.stats.repair_bytes += repaired;
        state.stats.repair_time += started.elapsed();
        state.stats.peak_leases = state
            .stats
            .peak_leases
            .max(state.slots.iter().map(|s| s.readers as u64).sum());
        self.active = false;
        Ok(Some(FrameLease {
            pool: Arc::clone(&self.pool),
            slot: self.slot,
        }))
    }
}
impl Drop for WriteLease {
    fn drop(&mut self) {
        if self.active {
            let mut state = self.pool.lock();
            state.slots[self.slot].writing = false;
            state.writer = false;
            // A failed/abandoned grab may have changed unreported pixels.
            state.slots[self.slot].stale.fill(Span {
                left: 0,
                right: self.pool.width,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::UnsafeCell;

    struct Ram(UnsafeCell<Box<[u8]>>);
    // SAFETY: test access is exclusively through the pool's leases.
    unsafe impl Send for Ram {}
    unsafe impl Sync for Ram {}
    unsafe impl CaptureStorage for Ram {
        fn mapped_ptr(&self) -> *mut u8 {
            unsafe { (&mut *self.0.get()).as_mut_ptr() }
        }
        fn len(&self) -> usize {
            unsafe { (&*self.0.get()).len() }
        }
    }
    fn pool() -> Arc<CapturePool> {
        // SAFETY: all allocations are created uniquely for this pool.
        unsafe {
            CapturePool::new(
                (0..4)
                    .map(|_| Arc::new(Ram(UnsafeCell::new(vec![99; 16 * 16 * 4].into()))) as _)
                    .collect(),
                16,
                9,
                64,
            )
        }
        .unwrap()
    }
    fn rect(x1: i32, y1: i32, x2: i32, y2: i32) -> CaptureRect {
        CaptureRect { x1, y1, x2, y2 }
    }
    fn update(
        pool: &Arc<CapturePool>,
        truth: &mut [u8],
        damage: &[CaptureRect],
        value: u8,
    ) -> FrameLease {
        let mut write = pool.acquire().unwrap();
        for r in damage {
            for y in r.y1.clamp(0, 9)..r.y2.clamp(0, 9) {
                for x in r.x1.clamp(0, 16)..r.x2.clamp(0, 16) {
                    let offset = (y as usize * 16 + x as usize) * 4;
                    truth[offset..offset + 4].fill(value);
                    write.pixels_mut()[offset..offset + 4].fill(value);
                }
            }
        }
        let frame = write.complete_grab(damage).unwrap().unwrap();
        assert_eq!(frame.pixels(), truth);
        frame
    }

    #[test]
    fn partial_damage_matches_full_reference_and_padding_stays_black() {
        let pool = pool();
        let mut truth = vec![0; 16 * 16 * 4];
        let mut frame = update(&pool, &mut truth, &[rect(0, 0, 16, 9)], 3);
        for n in 0..250 {
            let x = n % 16;
            let y = n % 9;
            frame = update(
                &pool,
                &mut truth,
                &[rect(x, y, x + 2, y + 2), rect(2, 2, 4, 4)],
                n as u8,
            );
        }
        assert_eq!(frame.pixels(), truth);
        assert!(frame.pixels()[16 * 9 * 4..].iter().all(|&v| v == 0));
    }

    #[test]
    fn two_retained_frames_never_block_or_change_while_producer_keeps_grabbing() {
        let pool = pool();
        let mut truth = vec![0; 16 * 16 * 4];
        let a = update(&pool, &mut truth, &[rect(0, 0, 16, 9)], 7);
        let b = update(&pool, &mut truth, &[rect(0, 0, 16, 9)], 8);
        let a_copy = a.pixels().to_vec();
        let b_copy = b.pixels().to_vec();
        let mut latest = None;
        for n in 0..1000 {
            latest = Some(update(
                &pool,
                &mut truth,
                &[rect(n % 16, n % 9, n % 16 + 1, n % 9 + 1)],
                n as u8,
            ));
        }
        assert_eq!(a.pixels(), a_copy);
        assert_eq!(b.pixels(), b_copy);
        assert_eq!(latest.unwrap().pixels(), truth);
        assert_eq!(pool.stats().exhaustions, 0);
    }

    #[test]
    fn initialization_requires_exact_coverage_including_holes() {
        let pool = pool();
        let mut write = pool.acquire().unwrap();
        write.pixels_mut()[..64 * 9].fill(11);
        assert!(write.complete_grab(&[rect(0, 0, 5, 9)]).unwrap().is_none());
        assert!(write.complete_grab(&[rect(6, 0, 16, 9)]).unwrap().is_none());
        let first = write.complete_grab(&[rect(5, 0, 6, 9)]).unwrap().unwrap();
        assert!(first.pixels()[..64 * 9].iter().all(|&v| v == 11));
        assert_eq!(pool.stats().repair_bytes, 0);
    }

    #[test]
    fn full_damage_needs_no_repair_and_abandoned_writes_are_repaired() {
        let pool = pool();
        let mut truth = vec![0; 16 * 16 * 4];
        let frame = update(&pool, &mut truth, &[rect(0, 0, 16, 9)], 3);
        drop(frame);
        let mut bad = pool.acquire().unwrap();
        bad.pixels_mut()[..64 * 9].fill(99);
        drop(bad);
        let next = update(&pool, &mut truth, &[rect(0, 0, 16, 9)], 4);
        assert_eq!(pool.stats().repair_bytes, 0);
        drop(next);
        let _last = update(&pool, &mut truth, &[rect(3, 2, 4, 3)], 8);
        assert!(pool.stats().repair_bytes > 0);
    }
}
