// SPDX-License-Identifier: GPL-2.0-or-later
pub mod benchmark;
#[cfg(feature = "live")]
pub mod capture_benchmark;
pub mod gpu;
pub mod verify;
pub mod workload;

use anyhow::{ensure, Context, Result};
use rayon::prelude::*;
use vino_chimera::{kvino, scanout::StripEncoder};

pub fn ella() -> kvino::DockProfile {
    kvino::DockProfile::for_family(kvino::Family::Ella)
        .expect("vendored Ella profile")
        .steady()
}

/// Validate the prototype's supported format before either backend reads pixels.
pub fn validate(
    dock: kvino::DockProfile,
    width: usize,
    rgb: &[u8],
    coords: &[(usize, usize)],
) -> Result<()> {
    ensure!(
        dock.video_on_ctrl_pipe() && dock.strip_dims() == (64, 16) && !dock.is_ten_bit(),
        "only the Ella 8-bit, 64x16 strip profile is supported"
    );
    ensure!(
        width > 0 && width <= 65535 && width.is_multiple_of(64),
        "width must be a nonzero multiple of 64, at most 65535"
    );
    let row = width.checked_mul(3).context("row size overflow")?;
    ensure!(
        rgb.len().is_multiple_of(row),
        "RGB888 source has an incomplete row"
    );
    let height = rgb.len() / row;
    ensure!(
        height > 0 && height <= 65535 && height.is_multiple_of(16),
        "height must be a nonzero multiple of 16, at most 65535"
    );
    for &(x, y) in coords {
        ensure!(
            x % 64 == 0 && y % 16 == 0 && x < width && y < height,
            "strip coordinate ({x},{y}) is outside the padded surface or unaligned"
        );
    }
    Ok(())
}

pub struct CpuEncoder {
    pool: rayon::ThreadPool,
    pub threads: usize,
    pub encoded_strips: u64,
}

impl CpuEncoder {
    pub fn new(threads: usize) -> Result<Self> {
        ensure!(threads > 0, "CPU thread count must be positive");
        Ok(Self {
            pool: rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()?,
            threads,
            encoded_strips: 0,
        })
    }
}

impl StripEncoder for CpuEncoder {
    type Error = anyhow::Error;
    fn encode_strips(
        &mut self,
        dock: kvino::DockProfile,
        width: usize,
        rgb: &[u8],
        coords: &[(usize, usize)],
    ) -> Result<Vec<Vec<u8>>> {
        validate(dock, width, rgb, coords)?;
        let encode = |&(x, y): &(usize, usize)| {
            kvino::encode_strip(dock, width, rgb, x, y).map_err(Into::into)
        };
        let result: Result<Vec<Vec<u8>>> = if self.threads == 1 {
            coords.iter().map(encode).collect()
        } else {
            self.pool
                .install(|| coords.par_iter().map(encode).collect())
        };
        if result.is_ok() {
            self.encoded_strips += coords.len() as u64;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_malformed_surfaces_and_coordinates() {
        let image = vec![0; 64 * 16 * 3];
        assert!(validate(ella(), 0, &image, &[]).is_err());
        assert!(validate(ella(), 64, &image[..image.len() - 1], &[]).is_err());
        assert!(validate(ella(), 64, &image, &[(1, 0)]).is_err());
        assert!(validate(ella(), 64, &image, &[(64, 0)]).is_err());
        assert!(validate(ella().ten_bit(), 64, &image, &[]).is_err());
        assert!(validate(ella(), 64, &image, &[]).is_ok());
    }
    #[test]
    fn parallel_cpu_preserves_strip_order_and_bytes() {
        let image: Vec<_> = (0..128 * 32 * 3)
            .map(|i| ((i * 137 + i / 31) % 256) as u8)
            .collect();
        let coords = kvino::strip_coords(ella(), 128, 32, None).unwrap();
        let reference = CpuEncoder::new(1)
            .unwrap()
            .encode_strips(ella(), 128, &image, &coords)
            .unwrap();
        let parallel = CpuEncoder::new(2)
            .unwrap()
            .encode_strips(ella(), 128, &image, &coords)
            .unwrap();
        assert_eq!(reference, parallel);
    }
}
