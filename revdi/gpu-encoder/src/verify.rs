// SPDX-License-Identifier: GPL-2.0-or-later
use crate::{
    ella,
    gpu::GpuEncoder,
    workload::{self, Case},
    CpuEncoder,
};
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::{fs, path::Path, sync::Arc};
use vino_chimera::{
    capture_pool::CaptureRect,
    kvino,
    scanout::{HeadScanout, StripEncoder},
    surface::{CursorBitmap, CursorSnapshot, NativeAllocationId, NativeSurface},
};

#[derive(Serialize)]
pub struct Report {
    pub passed: bool,
    pub device: crate::gpu::DeviceInfo,
    pub surfaces: usize,
    pub strips: usize,
    pub framed_heads: usize,
    pub dispatches: u64,
    pub validation_errors: usize,
    pub scenarios: Vec<String>,
}
fn equal(expected: &[Vec<u8>], actual: &[Vec<u8>], context: &str) -> Result<()> {
    ensure!(
        expected.len() == actual.len(),
        "{context}: different strip/record counts"
    );
    for (index, (a, b)) in expected.iter().zip(actual).enumerate() {
        if a != b {
            let at = a
                .iter()
                .zip(b)
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(b.len()));
            anyhow::bail!("{context}: item {index}, first mismatch at byte {at}, lengths {}/{}, expected {:?}, got {:?}",
                a.len(),b.len(),a.get(at),b.get(at));
        }
    }
    Ok(())
}

/// Exercise the same mapped storage and descriptor binding used by EVDI, while
/// writing private RAM only. The CPU oracle sees the logically composed RGB
/// image, including native channel order, stride, padding and cursor blending.
fn verify_native(
    gpu: &mut GpuEncoder,
    cpu: &mut CpuEncoder,
    report: &mut Report,
    quick: bool,
) -> Result<()> {
    if gpu.info.vendor_id != 0x1002 || gpu.info.device_type != "INTEGRATED_GPU" {
        report
            .scenarios
            .push("direct capture skipped: requires an integrated Radeon GPU".into());
        return Ok(());
    }
    let mut dimensions: Vec<(usize, usize, usize)> = vec![(64, 16, 256), (128, 17, 128 * 4 + 16)];
    if !quick {
        dimensions.push((1920, 1080, 1920 * 4));
    }
    for (width, visible, stride) in dimensions {
        let height = visible.div_ceil(16) * 16;
        let pool = gpu.create_capture_pool_with_stride(width, visible, height, stride)?;
        let coords = kvino::strip_coords(ella(), width, height, None)?;
        let full = [CaptureRect {
            x1: 0,
            y1: 0,
            x2: width as i32,
            y2: visible as i32,
        }];
        let cursor_width = 13;
        let cursor_height = 11;
        let cursor_stride = cursor_width + 3;
        let bitmap = Arc::new(CursorBitmap::new(
            cursor_width,
            cursor_height,
            cursor_stride,
            (0..cursor_stride * cursor_height)
                .map(|i| {
                    let alpha = [0, 1, 127, 254, 255][i % 5];
                    (alpha << 24)
                        | (((i * 71) as u32 & 255) << 16)
                        | (((i * 113) as u32 & 255) << 8)
                        | ((i * 53) as u32 & 255)
                })
                .collect(),
        )?);
        let replacement = Arc::new(CursorBitmap::new(
            cursor_width,
            cursor_height,
            cursor_stride,
            bitmap.words().iter().map(|p| p ^ 0x00ff_ffff).collect(),
        )?);
        let mut previous = None;
        for (iteration, case) in [
            Case::Black,
            Case::White,
            Case::Primaries,
            Case::Checkerboard,
            Case::Gradient,
            Case::Noise,
        ]
        .into_iter()
        .enumerate()
        {
            let raw = workload::generate(case, width, visible, 3, 0, workload::DEFAULT_SEED);
            let mut writer = pool.acquire().map_err(anyhow::Error::msg)?;
            // Deliberately poison X, row padding and codec padding. Only BGR
            // visible pixels are meaningful to the logical image.
            writer.pixels_mut().fill(0x53 ^ iteration as u8);
            for y in 0..visible {
                for x in 0..width {
                    let source = &raw[(y * width + x) * 3..][..3];
                    let target = &mut writer.pixels_mut()[y * stride + x * 4..][..4];
                    target[..3].copy_from_slice(&[source[2], source[1], source[0]]);
                }
            }
            let frame = writer
                .complete_grab(&full)
                .map_err(anyhow::Error::msg)?
                .context("full native frame did not publish")?;
            // Retain the preceding read lease while publishing a replacement,
            // just as the live consumer does while comparing desktop pixels.
            drop(previous.take());
            let positions = [
                (None, (0, 0)),
                (Some(bitmap.clone()), (-3, -5)),
                (Some(bitmap.clone()), (3, 4)),
                (Some(bitmap.clone()), (width as i32 - 5, visible as i32 - 6)),
                (Some(bitmap.clone()), (i32::MIN, i32::MAX)),
                (None, (3, 4)),
                (Some(replacement.clone()), (3, 4)),
                (Some(bitmap.clone()), (3, 4)),
            ];
            for (cursor_index, (image, position)) in positions.into_iter().enumerate() {
                let cursor = CursorSnapshot::new(image, position, cursor_index as u64);
                let surface = NativeSurface::new(
                    frame.pixels(),
                    width,
                    height,
                    visible,
                    stride,
                    NativeAllocationId {
                        pool: frame.pool_id(),
                        slot: frame.slot_id(),
                    },
                    Some(&cursor),
                )?;
                let rgb: Vec<u8> = (0..height)
                    .flat_map(|y| (0..width).flat_map(move |x| surface.rgb_at(x, y)))
                    .collect();
                let before = gpu.stage_times;
                equal(
                    &cpu.encode_strips(ella(), width, &rgb, &coords)?,
                    &gpu.encode_native_strips(ella(), surface, &coords)?,
                    &format!(
                        "native {} {width}x{visible} cursor {cursor_index}",
                        case.name()
                    ),
                )?;
                let delta = gpu.stage_times.since(before);
                ensure!(
                    delta.pixel_upload_bytes == 0 && delta.direct_dispatches == 1,
                    "native encoder copied desktop pixels or omitted its direct dispatch"
                );
                if (2..=5).contains(&cursor_index) {
                    ensure!(
                        delta.cursor_upload_bytes == 0,
                        "cursor movement/hiding uploaded its unchanged bitmap"
                    );
                }
                if cursor_index >= 6 {
                    ensure!(
                        delta.cursor_upload_bytes == (bitmap.words().len() * 4) as u64,
                        "changed cursor shape was not uploaded exactly once"
                    );
                }
                report.surfaces += 1;
                report.strips += coords.len();
                if iteration == 0 && cursor_index == 0 {
                    let wrong = NativeSurface::new(
                        frame.pixels(),
                        width,
                        height,
                        visible,
                        stride,
                        NativeAllocationId {
                            pool: frame.pool_id() ^ u64::MAX,
                            slot: frame.slot_id(),
                        },
                        None,
                    )?;
                    ensure!(
                        gpu.encode_native_strips(ella(), wrong, &coords).is_err(),
                        "unregistered native pool was accepted"
                    );
                    let unrelated = frame.pixels().to_vec();
                    let wrong = NativeSurface::new(
                        &unrelated,
                        width,
                        height,
                        visible,
                        stride,
                        surface.allocation(),
                        None,
                    )?;
                    ensure!(
                        gpu.encode_native_strips(ella(), wrong, &coords).is_err(),
                        "unregistered pixel allocation was accepted"
                    );
                    ensure!(
                        gpu.encode_native_strips(ella().ten_bit(), surface, &coords)
                            .is_err(),
                        "HDR native input was accepted"
                    );
                    ensure!(
                        gpu.encode_native_strips(ella(), surface, &[(width, 0)])
                            .is_err(),
                        "out-of-bounds native strip was accepted"
                    );
                    let dispatches = gpu.dispatches;
                    ensure!(
                        gpu.encode_native_strips(ella(), surface, &[])?.is_empty()
                            && gpu.dispatches == dispatches,
                        "empty native batch submitted GPU work"
                    );
                }
            }
            previous = Some(frame);
        }
        ensure!(
            pool.stats().repair_bytes == 0,
            "full-frame native captures unexpectedly repaired pixels"
        );
        report.scenarios.push(format!(
            "native {width}x{visible} stride {stride}: BGRX, ignored X/padding, exact cursor blend and clipping, allocation identity, zero desktop copies"
        ));
    }
    Ok(())
}

pub fn run(device: Option<usize>, quick: bool, output: Option<&Path>) -> Result<Report> {
    let mut gpu = GpuEncoder::new(device, true)?;
    let mut cpu = CpuEncoder::new(1)?;
    let mut report = Report {
        passed: false,
        device: gpu.info.clone(),
        surfaces: 0,
        strips: 0,
        framed_heads: 0,
        dispatches: 0,
        validation_errors: 0,
        scenarios: vec![],
    };
    let cases = [
        Case::Black,
        Case::White,
        Case::Primaries,
        Case::Gradient,
        Case::Checkerboard,
        Case::Noise,
        Case::Scroll,
        Case::Motion,
    ];
    let dimensions = if quick {
        vec![(64, 16), (65, 17)]
    } else {
        vec![(64, 16), (65, 17), (128, 32), (1920, 1080)]
    };
    // Sparse uploads must work without a prior full upload, after changing
    // dimensions, with arbitrary order/duplicates and with old contents present.
    for (w, h) in [(512, 64), (1024, 32), (512, 64)] {
        let coords = [(w - 64, h - 16), (0, 0), (64, 0), (w - 64, h - 16)];
        for case in [Case::White, Case::Noise, Case::Black] {
            let raw = workload::generate(case, w, h, 5, 0, workload::DEFAULT_SEED);
            equal(
                &cpu.encode_strips(ella(), w, &raw, &coords)?,
                &gpu.encode_strips(ella(), w, &raw, &coords)?,
                &format!("sparse {} {w}x{h}", case.name()),
            )?;
            report.surfaces += 1;
            report.strips += coords.len();
        }
    }
    report
        .scenarios
        .push("sparse uploads across layout changes, reordered and duplicate strips".into());
    let mut examples = String::new();
    for (w, h) in dimensions {
        for case in cases {
            let raw = workload::generate(case, w, h, 3, 0, workload::DEFAULT_SEED);
            let mut padded = vec![];
            let (wp, hp) = workload::pad_into(w, h, &raw, &mut padded)?;
            let coords = kvino::strip_coords(ella(), wp, hp, None)?;
            let expected = cpu.encode_strips(ella(), wp, &padded, &coords)?;
            let actual = gpu.encode_strips(ella(), wp, &padded, &coords)?;
            let label = format!("{} {w}x{h}", case.name());
            equal(&expected, &actual, &label)?;
            for head in [0, 1] {
                equal(
                    &kvino::frame_records(ella(), &expected, head)?,
                    &kvino::frame_records(ella(), &actual, head)?,
                    &format!("{label} head {head}"),
                )?;
                report.framed_heads += 1;
            }
            if w <= 128 {
                for strip in &actual {
                    for byte in strip {
                        examples.push_str(&format!("{byte:02x}"));
                    }
                    examples.push('\n');
                }
            }
            report.surfaces += 1;
            report.strips += coords.len();
            report.scenarios.push(label.clone());
            eprintln!("PASS {label}: {} strips", coords.len());
        }
    }
    // Exercise many negative/positive quantisation boundaries and variable code lengths.
    for seed in 0..if quick { 8 } else { 128 } {
        let raw = workload::generate(
            Case::Noise,
            64,
            16,
            seed,
            0,
            workload::DEFAULT_SEED + seed as u64,
        );
        let expected = cpu.encode_strips(ella(), 64, &raw, &[(0, 0)])?;
        let actual = gpu.encode_strips(ella(), 64, &raw, &[(0, 0)])?;
        equal(&expected, &actual, &format!("seed {seed}"))?;
        report.strips += 1;
        report.surfaces += 1;
    }
    // Reach the signed coefficient bounds at each Haar level. In particular,
    // chroma can approach +/-1020 and AC luma +/-510; luma DC is unsigned.
    // Random noise rarely reaches these extrema of the compact intermediate.
    for size in [2, 4, 8] {
        for direction in 0..3 {
            for (a, b) in [
                ([0, 0, 0], [255, 255, 255]),
                ([0, 0, 255], [0, 255, 0]),
                ([255, 0, 0], [0, 255, 0]),
            ] {
                for invert in [false, true] {
                    let mut raw = Vec::with_capacity(64 * 16 * 3);
                    for y in 0..16 {
                        for x in 0..64 {
                            let right = x % size >= size / 2;
                            let bottom = y % size >= size / 2;
                            let side = match direction {
                                0 => right,
                                1 => bottom,
                                _ => right ^ bottom,
                            };
                            raw.extend_from_slice(if side ^ invert { &a } else { &b });
                        }
                    }
                    equal(
                        &cpu.encode_strips(ella(), 64, &raw, &[(0, 0)])?,
                        &gpu.encode_strips(ella(), 64, &raw, &[(0, 0)])?,
                        &format!("coefficient bounds size={size} direction={direction} {a:?}/{b:?} invert={invert}"),
                    )?;
                    report.strips += 1;
                    report.surfaces += 1;
                }
            }
        }
    }
    report
        .scenarios
        .push("54 signed coefficient boundary surfaces".into());
    verify_native(&mut gpu, &mut cpu, &mut report, quick)?;
    let raw = vec![0; 64 * 16 * 3];
    let before = gpu.dispatches;
    ensure!(
        gpu.encode_strips(ella(), 64, &raw, &[])?.is_empty(),
        "empty batch emitted output"
    );
    ensure!(gpu.dispatches == before, "empty batch dispatched GPU");
    ensure!(
        gpu.encode_strips(ella(), 64, &raw, &[(64, 0)]).is_err(),
        "invalid coordinate accepted"
    );
    ensure!(
        gpu.encode_strips(ella().ten_bit(), 64, &raw, &[(0, 0)])
            .is_err(),
        "HDR accepted"
    );
    gpu.set_output_limit(16)?;
    ensure!(
        gpu.encode_strips(ella(), 64, &raw, &[(0, 0)]).is_err(),
        "output overflow was not detected"
    );
    gpu.set_output_limit(8192)?;
    equal(
        &cpu.encode_strips(ella(), 64, &raw, &[(0, 0)])?,
        &gpu.encode_strips(ella(), 64, &raw, &[(0, 0)])?,
        "recovery after capacity error",
    )?;
    for case in [Case::Idle, Case::Small, Case::Scroll] {
        let (mut a, mut b) = (HeadScanout::new(ella()), HeadScanout::new(ella()));
        let mut settled_dispatches = 0;
        let mut previous = Vec::new();
        for frame in 0..12 {
            let raw = workload::generate(case, 128, 32, frame.min(4), 0, workload::DEFAULT_SEED);
            let pa = a.plan(128, 32, &raw);
            let pb = if raw == previous {
                b.plan_unchanged(128, 32, &raw)
            } else {
                b.plan(128, 32, &raw)
            };
            ensure!(pa == pb, "scanout plans differ");
            ensure!(
                a.content_changes() == b.content_changes(),
                "change reports differ"
            );
            let ea = a.encode_with(&pa, &raw, 0, &mut cpu)?;
            let eb = b.encode_with(&pb, &raw, 0, &mut gpu)?;
            match (ea, eb) {
                (Some(x), Some(y)) => {
                    ensure!(
                        x.presentations == y.presentations,
                        "presentation count differs"
                    );
                    equal(&x.records, &y.records, "scanout")?;
                }
                (None, None) => {}
                _ => anyhow::bail!("CPU/GPU idle disagreement"),
            }
            a.presented(&pa);
            b.presented(&pb);
            previous = raw;
            if frame == 8 {
                settled_dispatches = gpu.dispatches;
            }
        }
        ensure!(
            gpu.dispatches == settled_dispatches,
            "settled surface continued to dispatch"
        );
        report.scenarios.push(format!(
            "scanout {} cache/debt/idle/unchanged hashes",
            case.name()
        ));
    }
    report.validation_errors = gpu.validation_errors();
    ensure!(report.validation_errors == 0, "validation errors");
    report.dispatches = gpu.dispatches;
    report.passed = true;
    if let Some(path) = output {
        fs::create_dir_all(path).context("creating verification output directory")?;
        fs::write(
            path.join("verification.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        fs::write(path.join("ella-strips.hex"), examples)?;
    }
    Ok(report)
}
