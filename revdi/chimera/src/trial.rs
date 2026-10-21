// SPDX-License-Identifier: GPL-2.0-or-later

//! Bounded, single-head Ella trial. No reconnect loop, card creation or CPU fallback.

use crate::capture_pool::{CapturePool, FrameLease};
use crate::native_changes::NativeChanges;
use crate::revdi::{self, DeviceEvent};
use crate::scanout::StripEncoder;
use crate::session::{ControlSession, PresentStats};
use crate::shared_capture::SharedCaptureCopy;
use crate::surface::{CursorSnapshot, NativeAllocationId, NativeSurface};
use crate::trial_capture::CaptureWorker;
use crate::trial_cursor::CursorPlane;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Pattern,
    Desktop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMode {
    Client,
    Evdi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    Copied,
    Direct,
    Shared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComparisonMode {
    Cpu,
    Gpu,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureStats {
    pub captured: bool,
    /// Exact RGB comparison with the last surface taken by the presentation loop. The direct
    /// path compares complete logical pixels, including its independently supplied cursor.
    /// Capture and its EVDI acknowledgement still happen even when this is false.
    pub content_changed: bool,
    pub sequence: u64,
    /// Time the completed snapshot waited before the encoder took it.
    pub age: Duration,
    pub elapsed: Duration,
    pub damage_rects: usize,
    /// Sum of clipped rectangle areas; overlapping rectangles can count pixels twice.
    pub damage_pixels: usize,
    pub cursor_events: u64,
    pub cursor_composite: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureDiagnostics {
    pub unchanged: u64,
    pub event_waits: u64,
    pub conversion_time: Duration,
    pub comparison_time: Duration,
    pub snapshot_bytes: u64,
    pub repair_bytes: u64,
    pub repair_time: Duration,
    pub grab_time: Duration,
    pub pool_peak_leases: u64,
    pub pool_exhaustions: u64,
    pub shared_capture_copies: u64,
    pub shared_capture_bytes: u64,
    pub shared_capture_imports: u64,
    pub shared_capture_time: Duration,
}

pub struct Config {
    pub dock: String,
    pub head: u8,
    pub card: i32,
    pub source: Source,
    pub cursor: CursorMode,
    pub capture: CaptureMode,
    pub comparison: ComparisonMode,
    pub duration: Duration,
    pub fps: u32,
}

/// Hooks execute on the presentation loop. Capture heartbeat checks and the external watchdog
/// cover both threads; capture cannot keep the watchdog alive when USB or a GPU fence hangs.
pub trait Observer {
    fn phase(&mut self, phase: &str) -> Result<(), String>;
    fn connected(&mut self, edid: &[u8]) -> Result<(), String>;
    fn frame(&mut self, capture: CaptureStats, stats: PresentStats) -> Result<(), String>;
    fn capture_totals(&mut self, _captures: u64, _superseded: u64) {}
    fn capture_diagnostics(&mut self, _diagnostics: CaptureDiagnostics) {}
    fn progress(&mut self, ready: bool) -> Result<(), String>;
    fn stopping(&self) -> bool;
}

pub fn run<E: StripEncoder, O: Observer>(
    config: &Config,
    encoder: &mut E,
    observer: &mut O,
) -> Result<(), String>
where
    E::Error: core::fmt::Display,
{
    run_with_capture_pool(config, encoder, observer, None)
}

/// The GPU owner creates its mapped allocations before the trial touches USB or EVDI.
/// Capture owns its EVDI handle and receives storage leases plus an optional synchronized GPU copier.
pub fn run_with_capture_pool<E: StripEncoder, O: Observer>(
    config: &Config,
    encoder: &mut E,
    observer: &mut O,
    pool: Option<Arc<CapturePool>>,
) -> Result<(), String>
where
    E::Error: core::fmt::Display,
{
    run_with_capture_sources(config, encoder, observer, pool, None)
}

pub fn run_with_capture_sources<E: StripEncoder, O: Observer>(
    config: &Config,
    encoder: &mut E,
    observer: &mut O,
    pool: Option<Arc<CapturePool>>,
    copier: Option<Box<dyn SharedCaptureCopy>>,
) -> Result<(), String>
where
    E::Error: core::fmt::Display,
{
    if config.fps == 0
        || config.fps > 60
        || config.duration.is_zero()
        || config.duration.as_secs() > 900
        || config.head > 1
        || config.card < 0
    {
        return Err(
            "trial requires head 0/1, existing card, 1..60 fps and duration <=900 seconds".into(),
        );
    }
    let direct = config.capture != CaptureMode::Copied;
    if direct != pool.is_some()
        || (config.capture == CaptureMode::Shared) != copier.is_some()
        || (config.comparison == ComparisonMode::Gpu && !direct)
        || (direct
            && (config.source != Source::Desktop
                || config.cursor != CursorMode::Client
                || config.head != 0))
    {
        return Err(
            "direct capture requires a GPU pool, desktop source, client cursor and head 0".into(),
        );
    }
    observer.phase("engaging")?;
    let mut session = ControlSession::engage_ella_at(&config.dock)?;
    if usize::from(config.head) >= session.connectors() {
        return Err("head does not exist".into());
    }
    observer.phase("reading_edid")?;
    let edid = session.fetch_edid(config.head).map_err(|e| e.to_string())?;
    observer.connected(&edid)?;
    let mut card = if config.source == Source::Desktop {
        let card = if let Some(pool) = pool {
            if let Some(copier) = copier {
                CaptureWorker::start_shared(config.card, config.cursor, edid, pool, copier)?
            } else {
                CaptureWorker::start_direct(config.card, config.cursor, edid, pool)?
            }
        } else {
            CaptureWorker::start(config.card, config.cursor, edid)?
        };
        observer.phase("waiting_for_mode")?;
        Some(card)
    } else {
        if !session.activate_mode(config.head, 1920, 1080, 60)? {
            return Err("mode was deferred".into());
        }
        None
    };
    let interval = Duration::from_secs_f64(1.0 / f64::from(config.fps));
    let startup = Instant::now();
    let mut ready_at = None;
    let mut next_frame = Instant::now();
    let mut next_heartbeat = Instant::now() + Duration::from_secs(3);
    let keepalive = Duration::from_millis(session.profile().status_period_ms().max(1) as u64);
    let mut next_keepalive = Instant::now() + keepalive;
    let mut rgb = if direct {
        Vec::new()
    } else {
        vec![0; 1920 * 1088 * 3]
    };
    let mut native_frame: Option<FrameLease> = None;
    let mut native_changes = NativeChanges::new(session.profile());
    let mut previous_native_cursor: Option<CursorSnapshot> = None;
    let client_cursor = config.source == Source::Desktop && config.cursor == CursorMode::Client;
    let mut desktop_rgb = if client_cursor && !direct {
        vec![0; rgb.len()]
    } else {
        Vec::new()
    };
    let mut cursor = CursorPlane::default();
    let mut cursor_events = 0;
    let mut have_surface = false;
    let mut active = config.source == Source::Pattern;
    let mut pattern_index = 0;
    loop {
        if observer.stopping() {
            break;
        }
        if ready_at.is_some_and(|t: Instant| t.elapsed() >= config.duration) {
            break;
        }
        if ready_at.is_none() && startup.elapsed() > Duration::from_secs(25) {
            return Err("no first frame before startup deadline".into());
        }
        if let Some(card) = card.as_mut() {
            card.check()?;
            let (captures, superseded) = card.totals();
            observer.capture_totals(captures, superseded);
            observer.capture_diagnostics(card.diagnostics());
            for event in card.events() {
                match event {
                    DeviceEvent::Mode(mode) => {
                        revdi::validate_trial_mode(mode)?;
                        if !active {
                            if !session.activate_mode(config.head, 1920, 1080, 60)? {
                                return Err("mode was deferred".into());
                            }
                            active = true;
                        }
                    }
                    DeviceEvent::Dpms(value) if value != 0 && ready_at.is_some() => {
                        return Err("display powered down during trial".into());
                    }
                    DeviceEvent::Ddcci(request) => {
                        match session.ddcci(
                            config.head,
                            request.address,
                            request.flags,
                            &request.buffer,
                        ) {
                            Ok(response) => card.ddcci_response(response)?,
                            Err(e) => return Err(format!("DDC/CI: {e}")),
                        }
                    }
                    DeviceEvent::CursorSet(image) if client_cursor => {
                        cursor.set_image(image)?;
                        cursor_events += 1;
                    }
                    DeviceEvent::CursorMove(position) if client_cursor => {
                        cursor.move_to(position.x, position.y);
                        cursor_events += 1;
                    }
                    _ => {}
                }
            }
        }
        if active && Instant::now() >= next_frame {
            // Keep the output cadence despite late wakeups. Capture runs independently;
            // take only its newest complete surface, never a backlog of older frames.
            next_frame = advance_frame_deadline(next_frame, Instant::now(), interval);
            let capture_started = Instant::now();
            let mut previous_native_frame = None;
            let mut capture = if let Some(card) = card.as_ref() {
                if direct {
                    if let Some((frame, stats)) = card.take_direct() {
                        previous_native_frame = native_frame.replace(frame);
                        stats
                    } else {
                        CaptureStats::default()
                    }
                } else {
                    let target = if client_cursor {
                        &mut desktop_rgb
                    } else {
                        &mut rgb
                    };
                    card.take(target).unwrap_or_default()
                }
            } else {
                pattern(&mut rgb, pattern_index);
                pattern_index += 1;
                observer.capture_totals(pattern_index as u64, 0);
                CaptureStats {
                    captured: true,
                    content_changed: true,
                    sequence: pattern_index as u64,
                    elapsed: capture_started.elapsed(),
                    ..CaptureStats::default()
                }
            };
            have_surface |= capture.captured;
            let cursor_changed = cursor.take_changed();
            let mut pixels_changed = capture.content_changed || cursor_changed;
            let native_cursor = direct.then(|| cursor.snapshot());
            if direct && have_surface && (capture.captured || cursor_changed) {
                let started = Instant::now();
                let frame = native_frame
                    .as_ref()
                    .ok_or("missing direct capture frame")?;
                let snapshot = native_cursor.as_ref().expect("direct cursor snapshot");
                let current = direct_view(frame, snapshot)?;
                let previous = previous_native_frame
                    .as_ref()
                    .or(native_frame.as_ref())
                    .zip(previous_native_cursor.as_ref())
                    .map(|(frame, cursor)| direct_view(frame, cursor))
                    .transpose()?;
                pixels_changed = if config.comparison == ComparisonMode::Gpu {
                    native_changes
                        .update_with(
                            previous,
                            current,
                            |previous, current, width, height, flags| {
                                encoder
                                    .compare_native_strips(previous, current, width, height, flags)
                            },
                        )
                        .map_err(|e| format!("GPU native comparison: {e}"))?
                } else {
                    native_changes
                        .update(previous, current)
                        .map_err(|e| format!("native comparison: {e}"))?
                };
                capture.content_changed = pixels_changed;
                // Advance even if the logical image stayed equal (e.g. pixels hidden by a
                // cursor). The next comparison must use the newest consumed background.
                previous_native_cursor = Some(snapshot.clone());
                if let Some(card) = card.as_ref() {
                    card.direct_comparison(capture.captured, pixels_changed, started.elapsed());
                }
            }
            // Keep capture's four-slot pool moving while encoding/USB may wait. Only current
            // pixels remain leased by this loop once the exact comparison has completed.
            drop(previous_native_frame);
            if client_cursor && !direct && have_surface && pixels_changed {
                let started = Instant::now();
                cursor.compose(&desktop_rgb, &mut rgb, 1920, 1080)?;
                capture.cursor_composite = started.elapsed();
            }
            if have_surface && (pixels_changed || session.owes_repaint(config.head)) {
                let stats = if direct {
                    let frame = native_frame
                        .as_ref()
                        .ok_or("missing direct capture frame")?;
                    let snapshot = native_cursor.as_ref().expect("direct cursor snapshot");
                    session.present_native_revisions_with_encoder(
                        config.head,
                        direct_view(frame, snapshot)?,
                        &native_changes,
                        encoder,
                    )?
                } else if pixels_changed {
                    session.present_rgb_with_encoder(config.head, 1920, 1088, &rgb, encoder)?
                } else {
                    // No new pixels, or capture already compared them equal outside its mutex.
                    // Reuse the hashes while still paying the dock's remaining buffer copies.
                    session.present_unchanged_rgb_with_encoder(
                        config.head,
                        1920,
                        1088,
                        &rgb,
                        encoder,
                    )?
                };
                if stats.presentations > 0 && ready_at.is_none() {
                    ready_at = Some(Instant::now());
                    observer.phase("running")?;
                }
                capture.cursor_events = std::mem::take(&mut cursor_events);
                observer.frame(capture, stats)?;
            }
        }
        if Instant::now() >= next_keepalive {
            session.keepalive_poll()?;
            next_keepalive = Instant::now() + keepalive;
        }
        if !session.profile().video_on_ctrl_pipe() && Instant::now() >= next_heartbeat {
            session.heartbeat()?;
            next_heartbeat = Instant::now() + Duration::from_secs(3);
        }
        session.drain_pending_replies()?;
        observer.progress(ready_at.is_some())?;
        // A new surface/event wakes the desktop loop immediately. Otherwise only wake for
        // actual retransmit debt or bounded control/health maintenance, not every two ms.
        let maintenance = (Instant::now() + Duration::from_millis(50)).min(next_keepalive);
        if let Some(card) = card.as_ref() {
            card.wait(
                next_frame,
                maintenance,
                have_surface && (cursor.is_changed() || session.owes_repaint(config.head)),
            );
        } else {
            std::thread::sleep(
                next_frame
                    .min(maintenance)
                    .saturating_duration_since(Instant::now()),
            );
        }
    }
    observer.phase("stopping")?;
    // The external supervisor bounds even this teardown, including libusb/Vulkan destructors.
    if let Some(card) = card.as_mut() {
        card.stop()?;
        let (captures, superseded) = card.totals();
        observer.capture_totals(captures, superseded);
        observer.capture_diagnostics(card.diagnostics());
    }
    drop(card);
    if active {
        session.deactivate_mode(config.head, 1920, 1080)?;
    }
    Ok(())
}

fn direct_view<'a>(
    frame: &'a FrameLease,
    cursor: &'a CursorSnapshot,
) -> Result<NativeSurface<'a>, String> {
    NativeSurface::new(
        frame.pixels(),
        1920,
        1088,
        1080,
        1920 * 4,
        NativeAllocationId {
            pool: frame.pool_id(),
            slot: frame.slot_id(),
        },
        Some(cursor),
    )
    .map_err(|e| e.to_string())
}

fn pattern(rgb: &mut [u8], frame: usize) {
    let colours = [
        [255, 255, 255],
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
        [0, 0, 0],
    ];
    let moving_x = (frame * 24) % 1800;
    for y in 0..1080 {
        for x in 0..1920 {
            let pixel = if y < 540 {
                colours[x / 240]
            } else if y < 800 {
                if (x / 8 + y / 8) % 2 == 0 {
                    [255; 3]
                } else {
                    [0; 3]
                }
            } else if (moving_x..moving_x + 120).contains(&x) {
                [255, 128, 0]
            } else {
                [32, 32, 32]
            };
            rgb[(y * 1920 + x) * 3..(y * 1920 + x) * 3 + 3].copy_from_slice(&pixel);
        }
    }
    rgb[1920 * 1080 * 3..].fill(0);
}

/// Advance a due deadline along its original cadence, skipping all elapsed slots.
/// `interval` is nonzero and at most one second (the validated 1..60 FPS range).
fn advance_frame_deadline(previous: Instant, now: Instant, interval: Duration) -> Instant {
    let late = now.saturating_duration_since(previous);
    let remainder = Duration::from_nanos((late.as_nanos() % interval.as_nanos()) as u64);
    now + (interval - remainder)
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    #[test]
    fn late_wakeups_do_not_accumulate_cadence_drift() {
        let start = Instant::now();
        let interval = Duration::from_millis(20);
        let mut deadline = start;
        for slot in 0..100 {
            let woke = start + interval * slot + Duration::from_millis(2);
            deadline = advance_frame_deadline(deadline, woke, interval);
            assert_eq!(deadline, start + interval * (slot + 1));
        }
    }

    #[test]
    fn a_slow_frame_skips_missed_slots_without_a_catchup_burst() {
        let start = Instant::now();
        let interval = Duration::from_millis(20);
        let next = advance_frame_deadline(start, start + Duration::from_millis(75), interval);
        assert_eq!(next, start + Duration::from_millis(80));
        assert_eq!(
            advance_frame_deadline(next, next, interval),
            start + Duration::from_millis(100)
        );
    }
}
