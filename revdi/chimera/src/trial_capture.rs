// SPDX-License-Identifier: GPL-2.0-or-later

//! Capture acknowledges EVDI independently of the encoder and its presentation cadence.
//! The device is created, used and dropped on this thread; no Send assertion for its C handle.
//! One replaceable surface plus recycled storage bounds memory and avoids stale-frame queues.

use crate::capture_pool::{CapturePool, CaptureRect, FrameLease, CAPTURE_SLOTS};
use crate::revdi::{self, DeviceEvent, RevdiCard};
use crate::shared_capture::{SharedCaptureCopy, SharedFramebuffer};
use crate::trial::{CaptureDiagnostics, CaptureStats, CursorMode};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex, MutexGuard,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const EVENT_LIMIT: usize = 64;
const STALL_LIMIT: Duration = Duration::from_secs(3);
// DRM readiness wakes this early, including cursor and DDC/CI events. The timeout bounds
// stop/response handling and heartbeat checks; it does not cap active capture at 20 Hz.
const EVENT_WAIT: Duration = Duration::from_millis(50);

struct Surface {
    rgb: Vec<u8>,
    stats: CaptureStats,
    completed: Instant,
}

struct NativeSurface {
    frame: FrameLease,
    stats: CaptureStats,
    completed: Instant,
}

struct Mailbox {
    latest: Option<Surface>,
    latest_native: Option<NativeSurface>,
    spare: Vec<u8>,
    events: Vec<DeviceEvent>,
    responses: Vec<Vec<u8>>,
    connected: bool,
    progress: Instant,
    error: Option<String>,
    finished: bool,
    captures: u64,
    superseded: u64,
    diagnostics: CaptureDiagnostics,
}

impl Mailbox {
    fn new() -> Self {
        Self {
            latest: None,
            latest_native: None,
            spare: Vec::new(),
            events: Vec::new(),
            responses: Vec::new(),
            connected: false,
            progress: Instant::now(),
            error: None,
            finished: false,
            captures: 0,
            superseded: 0,
            diagnostics: CaptureDiagnostics::default(),
        }
    }

    /// Swap whole owned buffers only. The consumer never reads storage the producer can modify.
    fn publish(&mut self, rgb: &mut Vec<u8>, mut stats: CaptureStats) {
        self.captures += 1;
        stats.sequence = self.captures;
        let previous = self.latest.replace(Surface {
            rgb: std::mem::take(rgb),
            stats,
            completed: Instant::now(),
        });
        *rgb = if let Some(previous) = previous {
            self.superseded += 1;
            previous.rgb
        } else {
            std::mem::take(&mut self.spare)
        };
    }

    fn check(&self, now: Instant) -> Result<(), String> {
        if let Some(error) = &self.error {
            return Err(format!("capture thread: {error}"));
        }
        if self.finished {
            return Err("capture thread exited unexpectedly".into());
        }
        if now.saturating_duration_since(self.progress) > STALL_LIMIT {
            return Err("capture thread made no progress for three seconds".into());
        }
        Ok(())
    }

    fn publish_native(&mut self, frame: FrameLease, mut stats: CaptureStats) {
        self.captures += 1;
        stats.sequence = self.captures;
        if self
            .latest_native
            .replace(NativeSurface {
                frame,
                stats,
                completed: Instant::now(),
            })
            .is_some()
        {
            self.superseded += 1;
        }
    }
}

/// Compare outside the mailbox lock: capture can keep acknowledging and publishing while
/// the consumer reads its own buffers. Exact equality skips expensive strip hashing of
/// unchanged full-screen damage without trusting EVDI's rectangles or a hash collision.
fn take_surface(shared: &Mutex<Mailbox>, rgb: &mut Vec<u8>) -> Option<CaptureStats> {
    let mut surface = lock(shared).latest.take()?;
    surface.stats.age = surface.completed.elapsed();
    let started = Instant::now();
    surface.stats.content_changed = *rgb != surface.rgb;
    let elapsed = started.elapsed();
    std::mem::swap(rgb, &mut surface.rgb);
    let mut state = lock(shared);
    state.spare = surface.rgb;
    state.diagnostics.unchanged += u64::from(!surface.stats.content_changed);
    state.diagnostics.comparison_time += elapsed;
    Some(surface.stats)
}

fn lock(shared: &Mutex<Mailbox>) -> MutexGuard<'_, Mailbox> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct CaptureWorker {
    shared: Arc<Mutex<Mailbox>>,
    wake: Arc<Condvar>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl CaptureWorker {
    pub fn start(index: i32, cursor: CursorMode, edid: Vec<u8>) -> Result<Self, String> {
        Self::start_inner(index, cursor, edid, None, None)
    }

    pub fn start_direct(
        index: i32,
        cursor: CursorMode,
        edid: Vec<u8>,
        pool: Arc<CapturePool>,
    ) -> Result<Self, String> {
        if cursor != CursorMode::Client
            || (pool.width(), pool.height(), pool.stride()) != (1920, 1080, 1920 * 4)
        {
            return Err("direct capture requires client cursor and a 1920x1080 XRGB pool".into());
        }
        Self::start_inner(index, cursor, edid, Some(pool), None)
    }

    pub fn start_shared(
        index: i32,
        cursor: CursorMode,
        edid: Vec<u8>,
        pool: Arc<CapturePool>,
        copier: Box<dyn SharedCaptureCopy>,
    ) -> Result<Self, String> {
        if cursor != CursorMode::Client
            || (pool.width(), pool.height(), pool.stride()) != (1920, 1080, 1920 * 4)
        {
            return Err("shared capture requires client cursor and a 1920x1080 XRGB pool".into());
        }
        Self::start_inner(index, cursor, edid, Some(pool), Some(copier))
    }

    fn start_inner(
        index: i32,
        cursor: CursorMode,
        edid: Vec<u8>,
        pool: Option<Arc<CapturePool>>,
        copier: Option<Box<dyn SharedCaptureCopy>>,
    ) -> Result<Self, String> {
        let shared = Arc::new(Mutex::new(Mailbox::new()));
        let wake = Arc::new(Condvar::new());
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_wake = Arc::clone(&wake);
        let worker_stopping = Arc::clone(&stopping);
        let thread = thread::Builder::new()
            .name("vino-capture".into())
            .spawn(move || {
                let result = if let Some(pool) = pool {
                    if let Some(mut copier) = copier {
                        capture_shared(
                            index,
                            &edid,
                            &pool,
                            copier.as_mut(),
                            &worker_shared,
                            &worker_wake,
                            &worker_stopping,
                        )
                    } else {
                        capture_direct(
                            index,
                            &edid,
                            &pool,
                            &worker_shared,
                            &worker_wake,
                            &worker_stopping,
                        )
                    }
                } else {
                    capture(
                        index,
                        cursor,
                        &edid,
                        &worker_shared,
                        &worker_wake,
                        &worker_stopping,
                    )
                };
                let mut state = lock(&worker_shared);
                state.error = result.as_ref().err().cloned();
                state.finished = true;
                worker_wake.notify_one();
                result
            })
            .map_err(|e| format!("start capture thread: {e}"))?;
        let worker = Self {
            shared,
            wake,
            stopping,
            thread: Some(thread),
        };
        // Xrandr must see CONNECT before the main thread runs its layout hook. Capture
        // is already live by then, so a modeset cannot wait on the layout hook to finish.
        loop {
            worker.check()?;
            let state = lock(&worker.shared);
            if state.connected {
                drop(state);
                return Ok(worker);
            }
            drop(
                worker
                    .wake
                    .wait_timeout(state, EVENT_WAIT)
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
    }

    pub fn check(&self) -> Result<(), String> {
        lock(&self.shared).check(Instant::now())?;
        if self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
        {
            return Err("capture thread exited unexpectedly".into());
        }
        Ok(())
    }

    pub fn events(&self) -> Vec<DeviceEvent> {
        std::mem::take(&mut lock(&self.shared).events)
    }

    pub fn totals(&self) -> (u64, u64) {
        let state = lock(&self.shared);
        (state.captures, state.superseded)
    }

    pub fn take(&self, rgb: &mut Vec<u8>) -> Option<CaptureStats> {
        take_surface(&self.shared, rgb)
    }

    /// Move out the newest immutable lease. The consumer performs one per-strip comparison
    /// including cursor composition, retaining its previous input until that comparison ends.
    pub fn take_direct(&self) -> Option<(FrameLease, CaptureStats)> {
        let mut surface = lock(&self.shared).latest_native.take()?;
        surface.stats.age = surface.completed.elapsed();
        Some((surface.frame, surface.stats))
    }

    pub fn direct_comparison(&self, captured: bool, changed: bool, elapsed: Duration) {
        let mut state = lock(&self.shared);
        state.diagnostics.comparison_time += elapsed;
        state.diagnostics.unchanged += u64::from(captured && !changed);
    }

    pub fn diagnostics(&self) -> CaptureDiagnostics {
        lock(&self.shared).diagnostics
    }

    /// Sleep until there is eligible image work, a control event, failure, or maintenance.
    /// Holding the same mutex for the predicate and wait prevents lost notifications.
    pub fn wait(&self, frame_due: Instant, maintenance: Instant, repaint: bool) {
        let mut state = lock(&self.shared);
        loop {
            if !state.events.is_empty() || state.error.is_some() || state.finished {
                return;
            }
            let deadline = if repaint || state.latest.is_some() || state.latest_native.is_some() {
                frame_due.min(maintenance)
            } else {
                maintenance
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            (state, _) = self
                .wake
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn ddcci_response(&self, bytes: Vec<u8>) -> Result<(), String> {
        let mut state = lock(&self.shared);
        if state.responses.len() >= EVENT_LIMIT {
            return Err("capture DDC/CI response queue overflow".into());
        }
        state.responses.push(bytes);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| "capture thread panicked")??;
        }
        Ok(())
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        // The existing external watchdog/stop timeout also bounds a stuck ioctl or destructor.
        let _ = self.stop();
    }
}

fn capture(
    index: i32,
    cursor: CursorMode,
    edid: &[u8],
    shared: &Mutex<Mailbox>,
    wake: &Condvar,
    stopping: &AtomicBool,
) -> Result<(), String> {
    let client_cursor = cursor == CursorMode::Client;
    let mut card = RevdiCard::open_existing(index, client_cursor).map_err(|e| e.to_string())?;
    if client_cursor && !card.cursor_events_enabled() {
        return Err("EVDI could not enable separate cursor events".into());
    }
    card.connect(edid, 1920 * 1080, 1920 * 1080 * 60)
        .map_err(|e| e.to_string())?;
    lock(shared).connected = true;
    wake.notify_one();
    let mut rgb = Vec::new();
    while !stopping.load(Ordering::Acquire) {
        let responses = {
            let mut state = lock(shared);
            state.progress = Instant::now();
            std::mem::take(&mut state.responses)
        };
        for response in responses {
            card.ddcci_response(&response, true);
        }
        publish_events(shared, wake, card.events(Duration::ZERO))?;
        if let Some(mode) = card.mode() {
            revdi::validate_trial_mode(mode)?;
        }
        let started = Instant::now();
        let frame = card
            .try_next_frame(Duration::ZERO)
            .map_err(|e| e.to_string())?;
        let stats = if let Some(frame) = frame {
            lock(shared).diagnostics.grab_time += started.elapsed();
            let mut stats = CaptureStats {
                captured: true,
                damage_rects: frame.damage.len(),
                damage_pixels: frame
                    .damage
                    .iter()
                    .map(|rect| {
                        let width = (rect.x2.clamp(0, 1920) - rect.x1.clamp(0, 1920)).max(0);
                        let height = (rect.y2.clamp(0, 1080) - rect.y1.clamp(0, 1080)).max(0);
                        width as usize * height as usize
                    })
                    .sum(),
                ..CaptureStats::default()
            };
            let conversion = Instant::now();
            revdi::copy_trial_rgb(&frame, &mut rgb)?;
            let converted = conversion.elapsed();
            lock(shared).diagnostics.conversion_time += converted;
            lock(shared).diagnostics.snapshot_bytes += rgb.len() as u64;
            stats.elapsed = started.elapsed();
            Some(stats)
        } else {
            None
        };
        // try_next_frame can process a mode change or a cursor mapping failure too.
        if let Some(mode) = card.mode() {
            revdi::validate_trial_mode(mode)?;
        }
        if client_cursor && !card.cursor_events_enabled() {
            return Err(
                "EVDI disabled separate cursor events after a cursor bitmap mapping failure".into(),
            );
        }
        if let Some(stats) = stats {
            lock(shared).publish(&mut rgb, stats);
            wake.notify_one();
        } else {
            // Request/grab never waits on the presentation loop. Once a request is pending,
            // wait on DRM readiness itself, which also delivers non-frame events immediately.
            lock(shared).diagnostics.event_waits += 1;
            publish_events(shared, wake, card.events(EVENT_WAIT))?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn equal_xrgb(a: &[u8], b: &[u8]) -> bool {
    crate::surface::xrgb8888_eq(a, b)
}

fn capture_direct(
    index: i32,
    edid: &[u8],
    pool: &Arc<CapturePool>,
    shared: &Mutex<Mailbox>,
    wake: &Condvar,
    stopping: &AtomicBool,
) -> Result<(), String> {
    let mut card = RevdiCard::open_existing(index, true).map_err(|e| e.to_string())?;
    if !card.cursor_events_enabled() {
        return Err("EVDI could not enable separate cursor events".into());
    }
    let mut buffers = Vec::with_capacity(CAPTURE_SLOTS);
    for slot in 0..CAPTURE_SLOTS {
        let storage = pool.storage(slot);
        let owner: Arc<dyn Send + Sync> = storage.clone();
        // SAFETY: the registration retains allocation ownership, while only the matching
        // WriteLease below ever authorizes a grab. All storage was checked by CapturePool.
        buffers.push(
            unsafe {
                evdi::safe::ExternalBuffer::new(
                    slot as i32 + 1,
                    storage.mapped_ptr(),
                    storage.len(),
                    (pool.width(), pool.height(), pool.stride()),
                    owner,
                )
            }
            .map_err(|e| e.to_string())?,
        );
    }
    card.register_external_buffers(buffers)
        .map_err(|e| e.to_string())?;
    card.connect(edid, 1920 * 1080, 1920 * 1080 * 60)
        .map_err(|e| e.to_string())?;
    lock(shared).connected = true;
    wake.notify_one();
    let mut writer = None;
    while !stopping.load(Ordering::Acquire) {
        let responses = {
            let mut state = lock(shared);
            state.progress = Instant::now();
            std::mem::take(&mut state.responses)
        };
        for response in responses {
            card.ddcci_response(&response, true);
        }
        publish_events(shared, wake, card.events(Duration::ZERO))?;
        if let Some(mode) = card.mode() {
            revdi::validate_trial_mode(mode)?;
        }
        if !card.cursor_events_enabled() {
            return Err("EVDI disabled separate cursor events during direct capture".into());
        }
        if writer.is_none() {
            writer = Some(pool.acquire()?);
        }
        let target = writer.as_mut().unwrap();
        let started = Instant::now();
        // SAFETY: target holds the exclusive reservation across every timeout and late reply;
        // registration and pool owners keep memory valid. No reader/GPU can use this slot.
        let grabbed =
            unsafe { card.try_next_external_frame(target.slot_id() as i32 + 1, Duration::ZERO) }
                .map_err(|e| e.to_string())?;
        let grab_time = started.elapsed();
        if let Some(mode) = card.mode() {
            revdi::validate_trial_mode(mode)?;
        }
        if !card.cursor_events_enabled() {
            return Err("EVDI disabled separate cursor events during direct capture".into());
        }
        if let Some(frame) = grabbed {
            let damage = frame
                .damage
                .iter()
                .map(|r| CaptureRect {
                    x1: r.x1,
                    y1: r.y1,
                    x2: r.x2,
                    y2: r.y2,
                })
                .collect::<Vec<_>>();
            let complete = target.complete_grab(&damage)?;
            let pool_stats = pool.stats();
            {
                let mut state = lock(shared);
                state.diagnostics.grab_time += grab_time;
                state.diagnostics.repair_time = pool_stats.repair_time;
                state.diagnostics.repair_bytes = pool_stats.repair_bytes;
                state.diagnostics.pool_peak_leases = pool_stats.peak_leases;
                state.diagnostics.pool_exhaustions = pool_stats.exhaustions;
            }
            if let Some(frame) = complete {
                writer = None;
                lock(shared).publish_native(
                    frame,
                    CaptureStats {
                        captured: true,
                        damage_rects: damage.len(),
                        damage_pixels: damage
                            .iter()
                            .map(|r| {
                                let width = (r.x2.clamp(0, 1920) - r.x1.clamp(0, 1920)).max(0);
                                let height = (r.y2.clamp(0, 1080) - r.y1.clamp(0, 1080)).max(0);
                                width as usize * height as usize
                            })
                            .sum(),
                        elapsed: started.elapsed(),
                        ..CaptureStats::default()
                    },
                );
                wake.notify_one();
            }
        } else {
            lock(shared).diagnostics.event_waits += 1;
            publish_events(shared, wake, card.events(EVENT_WAIT))?;
        }
    }
    // `card` unregisters the retained allocations before this function's pool borrow ends.
    // Frame leases held by the consumer independently retain the pool through GPU completion.
    Ok(())
}

/// Hold the kernel read lease only while the GPU snapshots the framebuffer. The ACK is
/// released before comparison, encoding or USB, preserving the compositor's frame cadence.
fn capture_shared(
    index: i32,
    edid: &[u8],
    pool: &Arc<CapturePool>,
    copier: &mut dyn SharedCaptureCopy,
    shared: &Mutex<Mailbox>,
    wake: &Condvar,
    stopping: &AtomicBool,
) -> Result<(), String> {
    let mut card = RevdiCard::open_existing(index, true).map_err(|e| e.to_string())?;
    if !card.cursor_events_enabled() {
        return Err("EVDI could not enable separate cursor events".into());
    }
    card.enable_shared_capture().map_err(|e| e.to_string())?;
    card.connect(edid, 1920 * 1080, 1920 * 1080 * 60)
        .map_err(|e| e.to_string())?;
    lock(shared).connected = true;
    wake.notify_one();
    let damage = [CaptureRect {
        x1: 0,
        y1: 0,
        x2: 1920,
        y2: 1080,
    }];
    while !stopping.load(Ordering::Acquire) {
        let responses = {
            let mut state = lock(shared);
            state.progress = Instant::now();
            std::mem::take(&mut state.responses)
        };
        for response in responses {
            card.ddcci_response(&response, true);
        }
        publish_events(shared, wake, card.events(Duration::ZERO))?;
        if let Some(mode) = card.mode() {
            revdi::validate_trial_mode(mode)?;
        }
        if !card.cursor_events_enabled() {
            return Err("EVDI disabled separate cursor events during shared capture".into());
        }
        let mut writer = pool.acquire()?;
        let started = Instant::now();
        let copied = if let Some(frame) = card
            .try_next_shared_frame(Duration::ZERO)
            .map_err(|e| format!("shared framebuffer acquire: {e}"))?
        {
            let info = frame.info();
            if (info.width, info.height) != (1920, 1080) {
                return Err("shared framebuffer changed dimensions during capture".into());
            }
            let stats = copier.copy_frame(
                SharedFramebuffer {
                    fd: frame.dmabuf(),
                    producer_fence: Some(frame.ready_fence()),
                    width: info.width,
                    height: info.height,
                    stride: info.stride,
                    offset: info.offset,
                    modifier: info.modifier,
                    format: info.format,
                    size: info.size,
                },
                &mut writer,
            )?;
            // A revoked/expired lease means the producer may have changed the input. Never
            // publish this snapshot if release cannot prove the lease stayed valid.
            frame
                .finish()
                .map_err(|e| format!("shared framebuffer release: {e}"))?;
            Some(stats)
        } else {
            None
        };
        if let Some(stats) = copied {
            let frame = writer
                .complete_grab(&damage)?
                .ok_or("missing shared snapshot")?;
            let pool_stats = pool.stats();
            let mut state = lock(shared);
            state.diagnostics.grab_time += started.elapsed();
            state.diagnostics.shared_capture_copies += 1;
            state.diagnostics.shared_capture_bytes += stats.bytes;
            state.diagnostics.shared_capture_imports += stats.imports;
            state.diagnostics.shared_capture_time += stats.elapsed;
            state.diagnostics.pool_peak_leases = pool_stats.peak_leases;
            state.diagnostics.pool_exhaustions = pool_stats.exhaustions;
            state.publish_native(
                frame,
                CaptureStats {
                    captured: true,
                    damage_rects: 1,
                    damage_pixels: 1920 * 1080,
                    elapsed: started.elapsed(),
                    ..CaptureStats::default()
                },
            );
            wake.notify_one();
        } else {
            drop(writer);
            lock(shared).diagnostics.event_waits += 1;
            publish_events(shared, wake, card.events(EVENT_WAIT))?;
        }
    }
    Ok(())
}

fn publish_events(
    shared: &Mutex<Mailbox>,
    wake: &Condvar,
    events: Vec<DeviceEvent>,
) -> Result<(), String> {
    if !events.is_empty() {
        let mut state = lock(shared);
        if state.events.len() + events.len() > EVENT_LIMIT {
            return Err("capture event queue overflow".into());
        }
        state.events.extend(events);
        wake.notify_one();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_comparison_ignores_unused_x_but_detects_every_color_byte() {
        assert!(equal_xrgb(
            &[1, 2, 3, 0, 4, 5, 6, 255],
            &[1, 2, 3, 99, 4, 5, 6, 0]
        ));
        for channel in 0..3 {
            let mut b = [1, 2, 3, 0];
            b[channel] ^= 1;
            assert!(!equal_xrgb(&[1, 2, 3, 0], &b));
        }
        assert!(!equal_xrgb(&[1, 2, 3, 0], &[]));
    }

    fn worker() -> CaptureWorker {
        CaptureWorker {
            shared: Arc::new(Mutex::new(Mailbox::new())),
            wake: Arc::new(Condvar::new()),
            stopping: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    #[test]
    fn duplicate_captures_are_exactly_filtered_and_a_last_pixel_change_is_not() {
        let worker = worker();
        let mut producer = vec![7; 1920 * 1088 * 3];
        let mut consumer = Vec::new();
        for index in 0..5 {
            producer.resize(consumer.len().max(1920 * 1088 * 3), 7);
            producer.fill(7);
            if index >= 3 {
                producer[1920 * 1080 * 3 - 1] = 8;
            }
            lock(&worker.shared).publish(
                &mut producer,
                CaptureStats {
                    captured: true,
                    ..Default::default()
                },
            );
            let capture = worker.take(&mut consumer).unwrap();
            assert!(capture.captured);
            assert_eq!(capture.sequence, index + 1);
            assert_eq!(capture.content_changed, index == 0 || index == 3);
        }
        assert_eq!(worker.diagnostics().unchanged, 3);
        assert_eq!(consumer[1920 * 1080 * 3 - 1], 8);
    }

    #[test]
    fn duplicate_filter_preserves_first_black_image_and_retransmit_debt() {
        use crate::{
            kvino,
            scanout::{CpuStripEncoder, HeadScanout},
        };
        let worker = worker();
        let mut consumer = vec![0; 64 * 16 * 3];
        let mut producer = consumer.clone();
        let dock = kvino::DockProfile::for_family(kvino::Family::Ella)
            .unwrap()
            .steady();
        let mut scanout = HeadScanout::new(dock);
        let mut presented = 0;
        for index in 0..10 {
            producer.resize(consumer.len(), 0);
            producer.fill(0);
            if index > 0 {
                producer[0] = 255;
            }
            lock(&worker.shared).publish(
                &mut producer,
                CaptureStats {
                    captured: true,
                    ..Default::default()
                },
            );
            let capture = worker.take(&mut consumer).unwrap();
            if index == 0 {
                assert!(!capture.content_changed);
            }
            if capture.content_changed || scanout.owes_retransmission() {
                let plan = if capture.content_changed {
                    scanout.plan(64, 16, &consumer)
                } else {
                    scanout.plan_unchanged(64, 16, &consumer)
                };
                assert!(scanout
                    .encode_with(&plan, &consumer, 0, &mut CpuStripEncoder)
                    .unwrap()
                    .is_some());
                scanout.presented(&plan);
                presented += 1;
            }
        }
        assert_eq!(presented, 1 + usize::from(dock.damage_frames()));
        assert!(!scanout.owes_retransmission());
    }

    #[test]
    fn settled_idle_ignores_an_expired_frame_timer_and_waits_for_maintenance() {
        let worker = worker();
        let now = Instant::now();
        worker.wait(now, now + Duration::from_millis(25), false);
        assert!(now.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn pending_image_waits_for_its_frame_deadline_but_retransmits_are_not_stranded() {
        let worker = worker();
        let mut pixels = vec![1; 24];
        lock(&worker.shared).publish(&mut pixels, CaptureStats::default());
        let now = Instant::now();
        worker.wait(
            now + Duration::from_millis(20),
            now + Duration::from_secs(2),
            false,
        );
        assert!(now.elapsed() >= Duration::from_millis(20));
        worker.take(&mut pixels).unwrap();
        // No new capture is needed to wake when the current image still owes a repeat.
        worker.wait(
            Instant::now(),
            Instant::now() + Duration::from_secs(2),
            true,
        );
    }

    #[test]
    fn arriving_frames_controls_and_failure_wake_the_consumer() {
        use std::sync::mpsc;
        for kind in 0..4 {
            let worker = worker();
            let shared = Arc::clone(&worker.shared);
            let wake = Arc::clone(&worker.wake);
            let (started, start) = mpsc::channel();
            let (completed, done) = mpsc::channel();
            let waiter = thread::spawn(move || {
                started.send(()).unwrap();
                worker.wait(
                    Instant::now(),
                    Instant::now() + Duration::from_secs(5),
                    false,
                );
                completed.send(()).unwrap();
            });
            start.recv_timeout(Duration::from_secs(1)).unwrap();
            {
                let mut state = lock(&shared);
                match kind {
                    0 => state.publish(&mut vec![1; 24], CaptureStats::default()),
                    1 => state.events.push(DeviceEvent::Dpms(0)),
                    2 => state.error = Some("capture failed".into()),
                    _ => state.finished = true,
                }
                wake.notify_one();
            }
            // Also covers notification before the consumer entered wait: the predicate
            // must notice it without waiting for a second notification or a frame timer.
            done.recv_timeout(Duration::from_secs(1)).unwrap();
            waiter.join().unwrap();
        }
    }

    #[test]
    fn slow_consumer_receives_latest_frame_without_blocking_producer_or_mutating_its_image() {
        let shared = Arc::new(Mutex::new(Mailbox::new()));
        let mut producer = vec![7; 24];
        lock(&shared).publish(
            &mut producer,
            CaptureStats {
                captured: true,
                ..Default::default()
            },
        );
        let mut consumer = vec![0; 24];
        assert_eq!(take_surface(&shared, &mut consumer).unwrap().sequence, 1);
        let worker = Arc::clone(&shared);
        thread::spawn(move || {
            for i in 2..=1000 {
                producer.resize(24, 0);
                producer.fill((i % 251) as u8);
                lock(&worker).publish(
                    &mut producer,
                    CaptureStats {
                        captured: true,
                        ..Default::default()
                    },
                );
            }
        })
        .join()
        .unwrap();
        // While the encoder held this buffer, capture completed 999 more updates independently.
        assert_eq!(consumer, vec![7; 24]);
        {
            let state = lock(&shared);
            assert_eq!((state.captures, state.superseded), (1000, 998));
        }
        assert_eq!(take_surface(&shared, &mut consumer).unwrap().sequence, 1000);
        assert_eq!(consumer, vec![(1000 % 251) as u8; 24]);
        assert!(take_surface(&shared, &mut consumer).is_none());
    }

    #[test]
    fn alternating_capture_and_encoding_reuses_three_owned_buffers() {
        let state = Mutex::new(Mailbox::new());
        let mut producer = vec![1; 24];
        let mut consumer = vec![2; 24];
        let mut pointers = std::collections::HashSet::new();
        for _ in 0..100 {
            producer.resize(24, 0);
            pointers.insert(producer.as_ptr() as usize);
            pointers.insert(consumer.as_ptr() as usize);
            lock(&state).publish(&mut producer, CaptureStats::default());
            take_surface(&state, &mut consumer).unwrap();
        }
        assert_eq!(pointers.len(), 3);
    }

    #[test]
    fn idle_capture_progress_is_healthy_but_stalls_and_errors_are_not() {
        let mut state = Mailbox::new();
        let now = state.progress;
        assert!(state.check(now + Duration::from_secs(2)).is_ok());
        assert!(state
            .check(now + Duration::from_secs(4))
            .unwrap_err()
            .contains("no progress"));
        state.progress = now + Duration::from_secs(4);
        assert!(state.check(state.progress).is_ok());
        state.error = Some("GRABPIX failed".into());
        assert!(state
            .check(state.progress)
            .unwrap_err()
            .contains("GRABPIX failed"));
    }

    #[test]
    fn stopping_joins_the_owner_thread_and_propagates_its_failure() {
        let shared = Arc::new(Mutex::new(Mailbox::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let release = Arc::clone(&released);
        let owner = thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                thread::yield_now();
            }
            release.store(true, Ordering::Release);
            Err("device teardown failed".to_string())
        });
        let mut worker = CaptureWorker {
            shared,
            wake: Arc::new(Condvar::new()),
            stopping,
            thread: Some(owner),
        };
        assert_eq!(worker.stop().unwrap_err(), "device teardown failed");
        assert!(released.load(Ordering::Acquire));
        assert!(worker.thread.is_none());
        assert!(worker.stop().is_ok());
    }
}
