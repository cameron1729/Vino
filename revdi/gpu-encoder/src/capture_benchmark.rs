// SPDX-License-Identifier: GPL-2.0-or-later
//! Fixed-input capture replay. Simulates dirty EVDI writes into private RAM; never opens
//! EVDI/USB or changes displays. Both paths include the same simulated capture write.

use crate::{
    ella,
    gpu::{GpuEncoder, StageTimes},
    workload::{self, Case},
};
use anyhow::{ensure, Context, Result};
use clap::Args;
use serde::Serialize;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};
use vino_chimera::{
    capture_pool::{CaptureRect, FrameLease},
    native_changes::NativeChanges,
    revdi::{copy_trial_rgb, Frame as CapturedFrame},
    scanout::{HeadScanout, StripEncoder},
    surface::{CursorBitmap, CursorSnapshot, NativeAllocationId, NativeSurface, SurfaceView},
};

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const PADDED: usize = 1088;
const STRIDE: usize = WIDTH * 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    Copied,
    DirectHashed,
    DirectRevisions,
    DirectGpuRevisions,
}
impl Path {
    fn name(self) -> &'static str {
        match self {
            Self::Copied => "copied",
            // Preserve the historical label for the original direct path.
            Self::DirectHashed => "direct",
            Self::DirectRevisions => "direct-revisions",
            Self::DirectGpuRevisions => "direct-gpu-revisions",
        }
    }
    fn direct(self) -> bool {
        self != Self::Copied
    }
    fn revisions(self) -> bool {
        matches!(self, Self::DirectRevisions | Self::DirectGpuRevisions)
    }
}

#[derive(Clone, Copy)]
enum ReplayCase {
    Pixels(Case),
    Cursor,
    CursorOnly,
}
impl ReplayCase {
    fn name(self) -> &'static str {
        match self {
            Self::Pixels(case) => case.name(),
            Self::Cursor => "cursor",
            Self::CursorOnly => "cursor-only",
        }
    }
    fn pixels(self) -> Case {
        match self {
            Self::Pixels(case) => case,
            Self::Cursor | Self::CursorOnly => Case::Idle,
        }
    }
    fn partial(self) -> bool {
        self.pixels() == Case::Small
    }
}

#[derive(Args)]
pub struct Options {
    #[arg(long, default_value_t = 48)]
    frames: usize,
    #[arg(long, default_value_t = 8)]
    warmup: usize,
    #[arg(long, default_value_t = 3)]
    rounds: usize,
    #[arg(long, default_value = "gpu-artifacts/direct-capture-benchmark.json")]
    output: PathBuf,
}

#[derive(Default, Serialize)]
struct Timings {
    cpu_ms: f64,
    elapsed_ms: f64,
    simulated_grab_ms: f64,
    repair_ms: f64,
    snapshot_ms: f64,
    comparison_ms: f64,
    planning_ms: f64,
    encoding_framing_ms: f64,
}
#[derive(Serialize)]
struct Sample {
    case: String,
    path: &'static str,
    round: usize,
    frames: usize,
    simulated_captures: usize,
    total: Timings,
    snapshot_bytes: u64,
    repair_bytes: u64,
    stages: StageTimes,
    encoded_strips: u64,
    output_bytes: u64,
    output_fingerprint: u64,
}

fn cpu_ms() -> Result<f64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid writable timespec; this reads the process CPU clock only.
    ensure!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut value) } == 0,
        "process CPU clock unavailable"
    );
    Ok(value.tv_sec as f64 * 1000.0 + value.tv_nsec as f64 / 1e6)
}
fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn native<'a>(frame: &'a FrameLease, cursor: Option<&'a CursorSnapshot>) -> NativeSurface<'a> {
    NativeSurface::new(
        frame.pixels(),
        WIDTH,
        PADDED,
        HEIGHT,
        STRIDE,
        NativeAllocationId {
            pool: frame.pool_id(),
            slot: frame.slot_id(),
        },
        cursor,
    )
    .expect("validated benchmark geometry")
}

fn cursor_bitmaps() -> Result<[Arc<CursorBitmap>; 2]> {
    let width = 19;
    let height = 23;
    let stride = width + 3;
    let make = |invert| {
        let pixels = (0..stride * height)
            .map(|i| {
                let alpha = [0, 1, 127, 254, 255][i % 5];
                let rgb = (((i * 71) as u32 & 255) << 16)
                    | (((i * 113) as u32 & 255) << 8)
                    | ((i * 53) as u32 & 255);
                (alpha << 24) | (rgb ^ invert)
            })
            .collect();
        CursorBitmap::new(width, height, stride, pixels).map(Arc::new)
    };
    Ok([make(0)?, make(0x00ff_ffff)?])
}

fn cursor_snapshot(
    case: ReplayCase,
    index: usize,
    bitmaps: &[Arc<CursorBitmap>; 2],
) -> Option<CursorSnapshot> {
    if matches!(case, ReplayCase::Pixels(_)) {
        return None;
    }
    // Every state lasts two captures. Exercise unchanged input, crossings of
    // both strip boundaries, shape replacement, hiding and screen-edge clipping.
    let step = index / 2;
    let (bitmap, position) = match step % 6 {
        0 => (Some(bitmaps[0].clone()), (-4, 5)),
        1 => (Some(bitmaps[0].clone()), (60, 15)),
        2 => (Some(bitmaps[0].clone()), (71, 17)),
        3 => (Some(bitmaps[1].clone()), (71, 17)),
        4 => (None, (71, 17)),
        _ => (
            Some(bitmaps[0].clone()),
            (WIDTH as i32 - 7, HEIGHT as i32 - 5),
        ),
    };
    Some(CursorSnapshot::new(bitmap, position, step as u64))
}

fn fixtures(case: Case) -> [Vec<u8>; 2] {
    std::array::from_fn(|index| {
        let rgb = workload::generate(case, WIDTH, HEIGHT, index, 0, workload::DEFAULT_SEED);
        let mut xrgb = vec![0; STRIDE * HEIGHT];
        for (target, source) in xrgb
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(rgb.as_chunks::<3>().0)
        {
            target.copy_from_slice(&[source[2], source[1], source[0], (index * 119) as u8]);
        }
        xrgb
    })
}

fn grab(target: &mut [u8], source: &[u8], rect: CaptureRect) {
    for y in rect.y1 as usize..rect.y2 as usize {
        let start = y * STRIDE + rect.x1 as usize * 4;
        let end = y * STRIDE + rect.x2 as usize * 4;
        target[start..end].copy_from_slice(&source[start..end]);
    }
}

fn sample(
    device: Option<usize>,
    options: &Options,
    case: ReplayCase,
    images: &[Vec<u8>; 2],
    path: Path,
    round: usize,
) -> Result<Sample> {
    let mut gpu = GpuEncoder::new(device, false)?;
    let direct = path.direct();
    let pool = direct
        .then(|| gpu.create_capture_pool(WIDTH, HEIGHT, PADDED))
        .transpose()?;
    let mut current: Option<FrameLease> = None;
    let mut capture = if direct {
        Vec::new()
    } else {
        vec![0; STRIDE * HEIGHT]
    };
    let mut rgb = Vec::new();
    let mut spare = Vec::new();
    let mut scanout = HeadScanout::new(ella());
    let mut changes = NativeChanges::new(ella());
    let bitmaps = cursor_bitmaps()?;
    let mut previous_cursor: Option<CursorSnapshot> = None;
    let mut result = Sample {
        case: case.name().into(),
        path: path.name(),
        round,
        frames: options.frames,
        simulated_captures: 0,
        total: Timings::default(),
        snapshot_bytes: 0,
        repair_bytes: 0,
        stages: StageTimes::default(),
        encoded_strips: 0,
        output_bytes: 0,
        output_fingerprint: 0,
    };
    let mut fingerprint = DefaultHasher::new();
    let mut stages_before = gpu.stage_times;
    let mut strips_before = gpu.encoded_strips;
    let mut repair_before = 0;
    for index in 0..options.warmup + options.frames {
        if index == options.warmup {
            stages_before = gpu.stage_times;
            strips_before = gpu.encoded_strips;
            repair_before = pool.as_ref().map_or(0, |p| p.stats().repair_bytes);
        }
        let measured = index >= options.warmup;
        let rect = if case.partial() && index > 0 {
            CaptureRect {
                x1: 0,
                y1: 0,
                x2: 16,
                y2: 16,
            }
        } else {
            CaptureRect {
                x1: 0,
                y1: 0,
                x2: WIDTH as i32,
                y2: HEIGHT as i32,
            }
        };
        let source = &images[index % 2];
        let cursor = cursor_snapshot(case, index, &bitmaps);
        let capture_due = !matches!(case, ReplayCase::CursorOnly) || index == 0;
        let cursor_changed = previous_cursor.as_ref().map(CursorSnapshot::revision)
            != cursor.as_ref().map(CursorSnapshot::revision);
        let mut frame_times = Timings::default();
        let cpu_start = cpu_ms()?;
        let elapsed_start = Instant::now();
        let changed;
        if let Some(pool) = &pool {
            let latest = if capture_due {
                let mut writer = pool.acquire().map_err(anyhow::Error::msg)?;
                let start = Instant::now();
                grab(writer.pixels_mut(), source, rect);
                frame_times.simulated_grab_ms = ms(start);
                let start = Instant::now();
                let latest = writer
                    .complete_grab(&[rect])
                    .map_err(anyhow::Error::msg)?
                    .context("benchmark capture failed to initialize")?;
                frame_times.repair_ms = ms(start);
                Some(latest)
            } else {
                None
            };
            let start = Instant::now();
            changed = if !capture_due && !cursor_changed {
                // A repeated cursor state on the retained immutable capture is not a new
                // logical input. Only planner debt needs servicing on this iteration.
                false
            } else if path.revisions() {
                let previous = current
                    .as_ref()
                    .map(|old| native(old, previous_cursor.as_ref()));
                let next = native(
                    latest
                        .as_ref()
                        .or(current.as_ref())
                        .expect("initial capture establishes a retained frame"),
                    cursor.as_ref(),
                );
                if path == Path::DirectGpuRevisions {
                    changes.update_with(previous, next, |before, after, width, height, flags| {
                        gpu.compare_native_strips(before, after, width, height, flags)
                    })?
                } else {
                    changes.update(previous, next)?
                }
            } else {
                latest.as_ref().is_some_and(|latest| {
                    current
                        .as_ref()
                        .is_none_or(|old| !native(old, None).background_eq(native(latest, None)))
                }) || cursor_changed
            };
            if let Some(latest) = latest {
                current = Some(latest);
            }
            frame_times.comparison_ms = ms(start);
        } else {
            let start = Instant::now();
            grab(&mut capture, source, rect);
            frame_times.simulated_grab_ms = ms(start);
            let start = Instant::now();
            copy_trial_rgb(
                &CapturedFrame {
                    pixels: &capture,
                    width: WIDTH,
                    height: HEIGHT,
                    stride: STRIDE,
                    damage: Vec::new(),
                },
                &mut spare,
            )
            .map_err(anyhow::Error::msg)?;
            frame_times.snapshot_ms = ms(start);
            let start = Instant::now();
            changed = rgb != spare;
            std::mem::swap(&mut rgb, &mut spare);
            frame_times.comparison_ms = ms(start);
            if measured {
                result.snapshot_bytes += rgb.len() as u64;
            }
        }
        let view = match &current {
            Some(frame) => SurfaceView::Native(native(frame, cursor.as_ref())),
            None => SurfaceView::rgb(WIDTH, PADDED, &rgb)?,
        };
        let start = Instant::now();
        let plan = if path.revisions() {
            scanout.plan_native_revisions(
                native(
                    current.as_ref().expect("direct capture frame"),
                    cursor.as_ref(),
                ),
                &changes,
            )?
        } else if changed {
            scanout.plan_view(view)
        } else {
            scanout.plan_unchanged_view(view)
        };
        frame_times.planning_ms = ms(start);
        let start = Instant::now();
        let encoded = scanout.encode_view_with(&plan, view, 0, &mut gpu)?;
        scanout.presented(&plan);
        frame_times.encoding_framing_ms = ms(start);
        frame_times.elapsed_ms = ms(elapsed_start);
        frame_times.cpu_ms = cpu_ms()? - cpu_start;
        previous_cursor = cursor;
        if measured {
            result.simulated_captures += usize::from(capture_due);
            result.total.cpu_ms += frame_times.cpu_ms;
            result.total.elapsed_ms += frame_times.elapsed_ms;
            result.total.simulated_grab_ms += frame_times.simulated_grab_ms;
            result.total.repair_ms += frame_times.repair_ms;
            result.total.snapshot_ms += frame_times.snapshot_ms;
            result.total.comparison_ms += frame_times.comparison_ms;
            result.total.planning_ms += frame_times.planning_ms;
            result.total.encoding_framing_ms += frame_times.encoding_framing_ms;
            // Outside the timed region: verify the workload produced the same framed stream.
            changed.hash(&mut fingerprint);
            if let Some(frame) = encoded {
                frame.presentations.hash(&mut fingerprint);
                frame.records.hash(&mut fingerprint);
                result.output_bytes += frame.records.iter().map(|r| r.len() as u64).sum::<u64>();
            }
        }
    }
    result.stages = gpu.stage_times.since(stages_before);
    result.encoded_strips = gpu.encoded_strips - strips_before;
    result.repair_bytes = pool.as_ref().map_or(0, |p| p.stats().repair_bytes) - repair_before;
    result.output_fingerprint = fingerprint.finish();
    if direct {
        ensure!(
            result.stages.pixel_upload_bytes == 0 && result.snapshot_bytes == 0,
            "direct capture copied a desktop input"
        );
        if !case.partial() {
            ensure!(result.repair_bytes == 0, "full capture needed repair");
        }
        if matches!(case, ReplayCase::CursorOnly) {
            ensure!(
                result.simulated_captures == 0
                    && result.total.simulated_grab_ms == 0.0
                    && result.total.repair_ms == 0.0,
                "cursor-only replay unexpectedly captured desktop pixels"
            );
        }
    }
    Ok(result)
}

pub fn run(device: Option<usize>, options: Options) -> Result<()> {
    ensure!(
        options.frames > 0
            && options.frames <= 10000
            && options.warmup >= 4
            && options.warmup <= 1000
            && options.rounds > 0
            && options.rounds <= 10,
        "require frames 1..10000, warmup 4..1000 and rounds 1..10"
    );
    let mut samples = Vec::new();
    for case in [
        ReplayCase::Pixels(Case::Idle),
        ReplayCase::Pixels(Case::Small),
        ReplayCase::Pixels(Case::Scroll),
        ReplayCase::Pixels(Case::Motion),
        ReplayCase::Pixels(Case::Noise),
        ReplayCase::Cursor,
        ReplayCase::CursorOnly,
    ] {
        let images = fixtures(case.pixels());
        for round in 0..options.rounds {
            let mut pair = Vec::new();
            let mut paths = vec![
                Path::Copied,
                Path::DirectHashed,
                Path::DirectRevisions,
                Path::DirectGpuRevisions,
            ];
            if !matches!(case, ReplayCase::Pixels(_)) {
                paths.remove(0);
            }
            if round % 2 != 0 {
                paths.reverse();
            }
            for path in paths {
                let value = sample(device, &options, case, &images, path, round)?;
                eprintln!(
                    "{} {}: {:.3} CPU ms/frame, {:.3} elapsed ms/frame",
                    value.case,
                    value.path,
                    value.total.cpu_ms / value.frames as f64,
                    value.total.elapsed_ms / value.frames as f64
                );
                pair.push(value);
            }
            for candidate in &pair[1..] {
                ensure!(
                    pair[0].output_fingerprint == candidate.output_fingerprint
                        && pair[0].output_bytes == candidate.output_bytes
                        && pair[0].encoded_strips == candidate.encoded_strips,
                    "{} / {} replay output differs for {}",
                    pair[0].path,
                    candidate.path,
                    case.name()
                );
            }
            samples.extend(pair);
        }
    }
    if let Some(parent) = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &options.output,
        serde_json::to_vec_pretty(&serde_json::json!({
            "scope": "Private RAM replay including simulated dirty capture writes, snapshot/repair, comparison, planning, GPU encoding and framing; compares copied input, original direct comparison plus hashing, CPU exact native strip revisions, and GPU exact native strip revisions (CPU fast path for cursor-only input); excludes EVDI kernel, compositor, USB and physical display",
            "geometry": [WIDTH, HEIGHT], "warmup_frames": options.warmup, "samples": samples,
            "output_matches": true, "hardware_changed": false
        }))?,
    )?;
    println!("Capture replay results: {}", options.output.display());
    Ok(())
}
