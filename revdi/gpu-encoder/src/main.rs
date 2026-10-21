// SPDX-License-Identifier: GPL-2.0-or-later
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use vino_gpu_encoder::{benchmark, gpu, verify, workload};

#[derive(Parser)]
#[command(about = "Offline Ella CPU/Vulkan encoder experiment; never opens a USB device")]
struct Cli {
    #[arg(long, global = true)]
    device: Option<usize>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Devices,
    /// Private DMA-buf export/import/copy verification; does not open EVDI or USB.
    SharedCaptureTest {
        #[arg(long)]
        no_validation: bool,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    Bench(benchmark::Options),
    Sample(benchmark::SampleOptions),
    #[cfg(feature = "live")]
    CaptureBench(vino_gpu_encoder::capture_benchmark::Options),
    Export(workload::ExportOptions),
    Verify {
        #[arg(long)]
        quick: bool,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}
fn main() -> Result<()> {
    let args = Cli::parse();
    match args.command {
        Command::Bench(options) => {
            let report = benchmark::run(args.device, options)?;
            println!("{}", report.recommendation);
        }
        Command::Sample(options) => benchmark::sample(args.device, options)?,
        #[cfg(feature = "live")]
        Command::CaptureBench(options) => {
            vino_gpu_encoder::capture_benchmark::run(args.device, options)?
        }
        Command::Export(options) => workload::export(options)?,
        Command::SharedCaptureTest {
            no_validation,
            output,
        } => {
            let report = gpu::shared_capture::self_test(args.device, !no_validation)?;
            let json = serde_json::to_string_pretty(&report)?;
            if let Some(path) = output {
                std::fs::write(path, &json)?;
            }
            println!("{json}");
        }
        Command::Devices => println!("{}", serde_json::to_string_pretty(&gpu::devices()?)?),
        Command::Verify { quick, output } => println!(
            "{}",
            serde_json::to_string_pretty(&verify::run(args.device, quick, output.as_deref())?)?
        ),
    }
    Ok(())
}
