// SPDX-License-Identifier: GPL-2.0-or-later
use anyhow::{ensure, Context, Result};
use clap::ValueEnum;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Case {
    Idle,
    Small,
    Scroll,
    Motion,
    Noise,
    Gradient,
    Checkerboard,
    Black,
    White,
    Primaries,
}
impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Small => "small",
            Self::Scroll => "scroll",
            Self::Motion => "motion",
            Self::Noise => "noise",
            Self::Gradient => "gradient",
            Self::Checkerboard => "checkerboard",
            Self::Black => "black",
            Self::White => "white",
            Self::Primaries => "primaries",
        }
    }
}
pub const DEFAULT_SEED: u64 = 0x56494e4f;
pub const BENCH_CASES: [Case; 5] = [
    Case::Idle,
    Case::Small,
    Case::Scroll,
    Case::Motion,
    Case::Noise,
];

pub fn generate(
    case: Case,
    width: usize,
    height: usize,
    frame: usize,
    head: usize,
    seed: u64,
) -> Vec<u8> {
    let mut data = vec![0; width * height * 3];
    let mut random = seed
        ^ (frame as u64).wrapping_mul(0x9e3779b97f4a7c15)
        ^ ((head + 1) as u64).wrapping_mul(0x123456789);
    for y in 0..height {
        for x in 0..width {
            let rgb = match case {
                Case::Black => [0, 0, 0],
                Case::White => [255, 255, 255],
                Case::Primaries => {
                    let mut c = [0; 3];
                    c[(x / 8 + y / 8 + frame + head) % 3] = 255;
                    c
                }
                Case::Checkerboard => {
                    let v = if (x + y + frame).is_multiple_of(2) {
                        0
                    } else {
                        255
                    };
                    [v, v, v]
                }
                Case::Gradient => [
                    (x * 255 / width.max(1)) as u8,
                    (y * 255 / height.max(1)) as u8,
                    ((x + y + head) % 256) as u8,
                ],
                Case::Noise => {
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    [random as u8, (random >> 8) as u8, (random >> 16) as u8]
                }
                Case::Motion => [
                    ((x + frame * 7 + head * 43) % 256) as u8,
                    ((y + frame * 11 + x / 8) % 256) as u8,
                    (((x ^ (y + frame * 5)) + frame * 3) % 256) as u8,
                ],
                Case::Scroll => {
                    // Reproducible text-like rows, including sharp edges and coloured glyphs.
                    let yy = y + frame * 3;
                    let character = (x / 8 + (yy / 12) * 13 + head * 7) as u32;
                    let lit = yy % 12 < 8
                        && x % 8 < 6
                        && ((character.wrapping_mul(0x45d9f3b) >> ((x % 6 + yy % 8) % 24)) & 1)
                            != 0;
                    if lit {
                        [210, ((character * 17) % 192 + 32) as u8, 180]
                    } else {
                        [16, 18, 20]
                    }
                }
                Case::Idle | Case::Small => {
                    if case == Case::Small && x < 16 && y < 16 {
                        [((frame * 37) % 256) as u8, 220, 64]
                    } else {
                        [((x / 8 + head * 17) % 256) as u8, ((y / 8) % 256) as u8, 40]
                    }
                }
            };
            let at = (y * width + x) * 3;
            data[at..at + 3].copy_from_slice(&rgb);
        }
    }
    data
}

pub fn pad_into(
    width: usize,
    height: usize,
    raw: &[u8],
    out: &mut Vec<u8>,
) -> Result<(usize, usize)> {
    ensure!(
        width > 0 && height > 0 && width <= 65472 && height <= 65520,
        "unsupported surface dimensions"
    );
    let size = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(3))
        .context("surface size overflow")?;
    ensure!(
        raw.len() == size,
        "RGB888 input length does not match dimensions"
    );
    let wp = width.div_ceil(64) * 64;
    let hp = height.div_ceil(16) * 16;
    out.resize(wp * hp * 3, 0);
    if wp == width {
        out[..raw.len()].copy_from_slice(raw);
    } else {
        for y in 0..height {
            out[y * wp * 3..y * wp * 3 + width * 3]
                .copy_from_slice(&raw[y * width * 3..(y + 1) * width * 3]);
            out[y * wp * 3 + width * 3..(y + 1) * wp * 3].fill(0);
        }
    }
    out[height * wp * 3..].fill(0);
    Ok((wp, hp))
}
/// Export exactly the benchmark's 16-frame, two-head corpus for an X11 replay.
#[derive(clap::Args)]
pub struct ExportOptions {
    #[arg(long, value_enum)]
    pub case: Case,
    #[arg(long, default_value_t = 1920)]
    pub width: usize,
    #[arg(long, default_value_t = 1080)]
    pub height: usize,
    #[arg(long, default_value_t = DEFAULT_SEED)]
    pub seed: u64,
    #[arg(long)]
    pub output: std::path::PathBuf,
}

pub fn export(options: ExportOptions) -> Result<()> {
    use std::io::Write;
    ensure!(
        options.width > 0 && options.width <= 3840 && options.height > 0 && options.height <= 2160,
        "export dimensions must fit within 3840x2160"
    );
    let mut file = std::io::BufWriter::new(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(options.output)?,
    );
    for frame in 0..16 {
        for head in 0..2 {
            let rgb = generate(
                options.case,
                options.width,
                options.height,
                frame,
                head,
                options.seed,
            );
            let mut bgrx = Vec::with_capacity(options.width * options.height * 4);
            for pixel in rgb.as_chunks::<3>().0 {
                bgrx.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0]);
            }
            file.write_all(&bgrx)?;
        }
    }
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pads_edges_black_and_reuses_storage() {
        let mut out = vec![99; 128 * 32 * 3];
        let (w, h) = pad_into(65, 17, &vec![255; 65 * 17 * 3], &mut out).unwrap();
        assert_eq!((w, h), (128, 32));
        for y in 0..h {
            for x in 0..w {
                assert_eq!(out[(y * w + x) * 3], if x < 65 && y < 17 { 255 } else { 0 });
            }
        }
        assert!(pad_into(0, 1, &[], &mut out).is_err());
    }
}
