// SPDX-License-Identifier: GPL-2.0-or-later
//! Fixed-input end-to-end timing and whole-APU energy measurements.
use crate::{
    ella,
    gpu::{DeviceInfo, GpuEncoder, StageTimes},
    workload::{self, Case},
    CpuEncoder,
};
use anyhow::{ensure, Result};
use clap::Args;
use serde::Serialize;
use std::{
    fs,
    hint::black_box,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use vino_chimera::scanout::{HeadScanout, StripEncoder};

#[derive(Args, Clone, Debug, Serialize)]
pub struct Options {
    #[arg(long, default_value_t = 1920)]
    pub width: usize,
    #[arg(long, default_value_t = 1080)]
    pub height: usize,
    #[arg(long, default_value_t = 60.0)]
    pub seconds: f64,
    #[arg(long, default_value_t = 15.0)]
    pub warmup: f64,
    #[arg(long, default_value_t = 5)]
    pub pairs: usize,
    #[arg(long, default_value_t = 2.0)]
    pub sweep_seconds: f64,
    #[arg(long)]
    pub cpu_threads: Option<usize>,
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "idle,small,scroll,motion,noise"
    )]
    pub cases: Vec<Case>,
    #[arg(long,default_value_t=workload::DEFAULT_SEED)]
    pub seed: u64,
    #[arg(long, default_value = "gpu-artifacts/benchmark")]
    pub output: PathBuf,
}
#[derive(Clone, Serialize)]
pub struct PowerSample {
    seconds: f64,
    watts: f64,
    temperature_c: Option<f64>,
}
#[derive(Clone)]
struct Sensor {
    power: PathBuf,
    temperature: PathBuf,
}
impl Sensor {
    fn discover(info: &DeviceInfo) -> Option<Self> {
        if info.vendor_id != 0x1002 || info.device_type != "INTEGRATED_GPU" {
            return None;
        }
        for entry in fs::read_dir("/sys/class/drm").ok()?.flatten() {
            let device = entry.path().join("device");
            let expected = format!("0x{:04x}", info.device_id);
            if fs::read_to_string(device.join("device"))
                .ok()
                .as_deref()
                .map(str::trim)
                != Some(expected.as_str())
            {
                continue;
            }
            let Ok(hwmons) = fs::read_dir(device.join("hwmon")) else {
                continue;
            };
            for hw in hwmons.flatten() {
                if fs::read_to_string(hw.path().join("name"))
                    .ok()
                    .as_deref()
                    .map(str::trim)
                    == Some("amdgpu")
                {
                    let power = hw.path().join("power1_average");
                    if power.exists() {
                        return Some(Self {
                            power,
                            temperature: hw.path().join("temp1_input"),
                        });
                    }
                }
            }
        }
        None
    }
    fn read(&self, start: Instant) -> Result<PowerSample> {
        let watts = fs::read_to_string(&self.power)?.trim().parse::<f64>()? / 1_000_000.0;
        ensure!(
            watts.is_finite() && watts > 0.0,
            "invalid package power sample"
        );
        let temperature_c = fs::read_to_string(&self.temperature)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(|v| v / 1000.0);
        Ok(PowerSample {
            seconds: start.elapsed().as_secs_f64(),
            watts,
            temperature_c,
        })
    }
}
struct Sampler {
    stop: mpsc::Sender<()>,
    worker: thread::JoinHandle<Result<Vec<PowerSample>>>,
}
impl Sampler {
    fn start(sensor: Sensor, start: Instant) -> Self {
        let (stop, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut values = vec![sensor.read(start)?];
            loop {
                let finished = !matches!(
                    rx.recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                );
                values.push(sensor.read(start)?);
                if finished {
                    break;
                }
            }
            Ok(values)
        });
        Self { stop, worker }
    }
    fn finish(self) -> Result<Vec<PowerSample>> {
        let _ = self.stop.send(());
        self.worker
            .join()
            .map_err(|_| anyhow::anyhow!("power sampler panicked"))?
    }
}
#[derive(Clone, Serialize)]
pub struct Measurement {
    pub backend: String,
    pub cpu_threads: Option<usize>,
    pub frame_pairs: u64,
    pub seconds: f64,
    pub frame_pairs_per_second: f64,
    pub cpu_seconds: f64,
    pub average_cpu_cores: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub deadline_misses: u64,
    pub encoded_bytes: u64,
    pub encoded_strips: u64,
    pub gpu_dispatches: u64,
    pub gpu_stage_totals: Option<StageTimes>,
    pub package_joules: Option<f64>,
    pub joules_per_frame_pair: Option<f64>,
    pub average_package_watts: Option<f64>,
    pub power_error: Option<String>,
    pub power_samples: Vec<PowerSample>,
}
#[derive(Serialize)]
pub struct Pair {
    pub cpu_first: bool,
    pub cpu_throughput: Measurement,
    pub gpu_throughput: Measurement,
    pub cpu_energy: Measurement,
    pub gpu_energy: Measurement,
}
#[derive(Serialize)]
pub struct CaseReport {
    pub case: Case,
    pub cpu_sweep: Vec<Measurement>,
    pub energy_sweep: Vec<Measurement>,
    pub gpu_probe: Measurement,
    pub fastest_cpu_threads: usize,
    pub energy_cpu_threads: usize,
    pub common_rate_hz: f64,
    pub pairs: Vec<Pair>,
    pub throughput_speedup: f64,
    pub throughput_ratio_ci95: Option<[f64; 2]>,
    pub energy_ratio_ci95: Option<[f64; 2]>,
    pub gpu_p99_ms: f64,
    pub energy_rate_matched: bool,
    pub meets_speed: bool,
    pub meets_energy: bool,
    pub meets_60hz: bool,
}
#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub unix_time: u64,
    pub device: DeviceInfo,
    pub options: Options,
    pub rust_flags: String,
    pub cpu_features: Vec<&'static str>,
    pub kernel: String,
    pub cpu_model: String,
    pub power_sensor: Option<String>,
    pub energy_scope: &'static str,
    pub cases: Vec<CaseReport>,
    pub acceptance_run: bool,
    pub recommendation: String,
}
enum Backend {
    Cpu(CpuEncoder),
    Gpu(Box<GpuEncoder>),
}
impl Backend {
    fn encoder(&mut self) -> &mut dyn StripEncoder<Error = anyhow::Error> {
        match self {
            Self::Cpu(x) => x,
            Self::Gpu(x) => x.as_mut(),
        }
    }
    fn name(&self) -> &'static str {
        match self {
            Self::Cpu(_) => "cpu",
            Self::Gpu(_) => "gpu",
        }
    }
    fn threads(&self) -> Option<usize> {
        match self {
            Self::Cpu(x) => Some(x.threads),
            _ => None,
        }
    }
    fn stages(&self) -> Option<StageTimes> {
        match self {
            Self::Gpu(x) if x.has_timestamps() => Some(x.stage_times),
            _ => None,
        }
    }
    fn counts(&self) -> (u64, u64) {
        match self {
            Self::Cpu(x) => (x.encoded_strips, 0),
            Self::Gpu(x) => (x.encoded_strips, x.dispatches),
        }
    }
}
struct Corpus {
    frames: Vec<[Vec<u8>; 2]>,
    width: usize,
    height: usize,
}
impl Corpus {
    fn new(case: Case, options: &Options) -> Self {
        Self {
            frames: (0..16)
                .map(|frame| {
                    std::array::from_fn(|head| {
                        workload::generate(
                            case,
                            options.width,
                            options.height,
                            frame,
                            head,
                            options.seed,
                        )
                    })
                })
                .collect(),
            width: options.width,
            height: options.height,
        }
    }
}
struct Runner<'a> {
    corpus: &'a Corpus,
    heads: [HeadScanout; 2],
    padded: [Vec<u8>; 2],
    index: usize,
}
impl<'a> Runner<'a> {
    fn new(corpus: &'a Corpus) -> Self {
        Self {
            corpus,
            heads: std::array::from_fn(|_| HeadScanout::new(ella())),
            padded: std::array::from_fn(|_| vec![]),
            index: 0,
        }
    }
    fn pair(&mut self, backend: &mut Backend) -> Result<u64> {
        let mut bytes = 0;
        for head in 0..2 {
            let raw = &self.corpus.frames[self.index % self.corpus.frames.len()][head];
            let (w, h) = workload::pad_into(
                self.corpus.width,
                self.corpus.height,
                raw,
                &mut self.padded[head],
            )?;
            let plan = self.heads[head].plan(w, h, &self.padded[head]);
            if let Some(frame) = self.heads[head].encode_with(
                &plan,
                &self.padded[head],
                head as u8,
                backend.encoder(),
            )? {
                bytes += frame.records.iter().map(|r| r.len() as u64).sum::<u64>()
                    * u64::from(frame.presentations);
                black_box(&frame.records);
            }
            self.heads[head].presented(&plan);
        }
        self.index += 1;
        Ok(bytes)
    }
}
fn cpu_time() -> f64 {
    unsafe {
        let mut t: libc::timespec = std::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) != 0 {
            return f64::NAN;
        }
        t.tv_sec as f64 + t.tv_nsec as f64 * 1e-9
    }
}
fn pace(start: Instant, frames: u64, rate: Option<f64>) {
    if let Some(rate) = rate {
        let target = Duration::from_secs_f64(frames as f64 / rate);
        if let Some(wait) = target.checked_sub(start.elapsed()) {
            thread::sleep(wait);
        }
    }
}
fn quantile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values[((values.len() - 1) as f64 * q).round() as usize]
}
fn measure(
    backend: &mut Backend,
    corpus: &Corpus,
    warmup: f64,
    seconds: f64,
    rate: Option<f64>,
    sensor: Option<&Sensor>,
) -> Result<Measurement> {
    let mut runner = Runner::new(corpus);
    let warm = Instant::now();
    let mut frames = 0;
    while warm.elapsed().as_secs_f64() < warmup
        || !runner.index.is_multiple_of(corpus.frames.len())
        || frames == 0
    {
        runner.pair(backend)?;
        frames += 1;
        pace(warm, frames, rate);
    }
    // Every backend starts the measured sequence at frame zero with identical cache/debt state.
    runner.index = 0;
    let before = backend.counts();
    let stages_before = backend.stages();
    let mut times = Vec::new();
    let mut bytes = 0;
    let mut misses = 0;
    let count = rate.map(|r| (r * seconds).round().max(1.0) as u64);
    let start = Instant::now();
    let sampler = sensor.cloned().map(|s| Sampler::start(s, start));
    let cpu_start = cpu_time();
    let mut pairs = 0;
    let execution = (|| -> Result<()> {
        loop {
            if count.map_or(start.elapsed().as_secs_f64() >= seconds, |n| pairs >= n) {
                break;
            }
            let t = Instant::now();
            bytes += runner.pair(backend)?;
            let elapsed = t.elapsed().as_secs_f64();
            times.push(elapsed * 1000.0);
            if elapsed > 1.0 / 60.0 {
                misses += 1;
            }
            pairs += 1;
            pace(start, pairs, rate);
        }
        Ok(())
    })();
    let duration = start.elapsed().as_secs_f64();
    let cpu_seconds = cpu_time() - cpu_start;
    let (samples, power_error) = match sampler.map(Sampler::finish) {
        Some(Ok(samples)) => (samples, None),
        Some(Err(error)) => (vec![], Some(error.to_string())),
        None => (
            vec![],
            Some("no matching readable AMD package power counter".into()),
        ),
    };
    execution?;
    ensure!(pairs > 0, "measurement completed no frames");
    times.sort_by(f64::total_cmp);
    let joules = integrate_power(&samples, duration);
    let after = backend.counts();
    Ok(Measurement {
        backend: backend.name().into(),
        cpu_threads: backend.threads(),
        frame_pairs: pairs,
        seconds: duration,
        frame_pairs_per_second: pairs as f64 / duration,
        cpu_seconds,
        average_cpu_cores: cpu_seconds / duration,
        p50_ms: quantile(&times, 0.5),
        p95_ms: quantile(&times, 0.95),
        p99_ms: quantile(&times, 0.99),
        deadline_misses: misses,
        encoded_bytes: bytes,
        encoded_strips: after.0 - before.0,
        gpu_dispatches: after.1 - before.1,
        gpu_stage_totals: backend
            .stages()
            .zip(stages_before)
            .map(|(after, before)| after.since(before)),
        package_joules: joules,
        joules_per_frame_pair: joules.map(|j| j / pairs as f64),
        average_package_watts: joules.map(|j| j / duration),
        power_error,
        power_samples: samples,
    })
}
/// Integrate only the measured window, extending the first sample back to t=0.
/// The final sampler read must cover the end of the window.
fn integrate_power(samples: &[PowerSample], seconds: f64) -> Option<f64> {
    if samples.len() < 2 || samples.last()?.seconds < seconds {
        return None;
    }
    let mut previous = PowerSample {
        seconds: 0.0,
        ..samples[0].clone()
    };
    let mut joules = 0.0;
    for sample in samples {
        if sample.seconds < previous.seconds {
            return None;
        }
        let end = sample.seconds.min(seconds);
        let span = sample.seconds - previous.seconds;
        let fraction = if span > 0.0 {
            (end - previous.seconds) / span
        } else {
            0.0
        };
        let watts = previous.watts + (sample.watts - previous.watts) * fraction;
        joules += (end - previous.seconds) * (previous.watts + watts) * 0.5;
        if sample.seconds >= seconds {
            return Some(joules);
        }
        previous = sample.clone();
    }
    None
}
fn thread_counts(explicit: Option<usize>) -> Vec<usize> {
    if let Some(n) = explicit {
        return vec![n];
    }
    let max = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let mut counts = vec![1];
    let mut n = 2;
    while n < max {
        counts.push(n);
        n *= 2;
    }
    if max > 1 {
        counts.push(max);
    }
    counts
}
/// Paired, deterministic percentile bootstrap of the geometric mean ratio.
pub fn ratio_ci95(ratios: &[f64]) -> Option<[f64; 2]> {
    if ratios.len() < 5 || ratios.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return None;
    }
    let logs: Vec<_> = ratios.iter().map(|v| v.ln()).collect();
    let mut rng = 0x7a6f5e4d3c2b1a09u64;
    let mut bootstrap = Vec::with_capacity(10000);
    for _ in 0..10000 {
        let mut total = 0.0;
        for _ in 0..logs.len() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            total += logs[rng as usize % logs.len()];
        }
        bootstrap.push((total / logs.len() as f64).exp());
    }
    bootstrap.sort_by(f64::total_cmp);
    Some([quantile(&bootstrap, 0.025), quantile(&bootstrap, 0.975)])
}
fn geometric_mean(values: &[f64]) -> f64 {
    (values.iter().map(|v| v.ln()).sum::<f64>() / values.len() as f64).exp()
}

pub fn run(device: Option<usize>, options: Options) -> Result<Report> {
    ensure!(
        !cfg!(debug_assertions),
        "bench requires a release build; use cargo build --release with RUSTFLAGS='-C target-cpu=native'"
    );
    ensure!(
        options.width > 0 && options.height > 0 && options.width <= 3840 && options.height <= 2160,
        "benchmark dimensions must fit within 3840x2160"
    );
    ensure!(
        options.seconds.is_finite()
            && options.seconds > 0.0
            && options.warmup.is_finite()
            && options.warmup >= 0.0
            && options.sweep_seconds.is_finite()
            && options.sweep_seconds > 0.0,
        "invalid measurement duration"
    );
    ensure!(
        options.pairs > 0 && options.cpu_threads != Some(0) && !options.cases.is_empty(),
        "empty benchmark configuration"
    );
    ensure!(
        options
            .cases
            .iter()
            .enumerate()
            .all(|(i, c)| !options.cases[..i].contains(c)),
        "duplicate benchmark case"
    );
    fs::create_dir_all(&options.output)?;
    let gpu = GpuEncoder::new(device, false)?;
    let info = gpu.info.clone();
    let sensor = Sensor::discover(&info);
    let mut gpu = Backend::Gpu(Box::new(gpu));
    let mut report = Report {
        schema_version: 1,
        unix_time: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        device: info,
        options: options.clone(),
        rust_flags: env!("VINO_BUILD_FLAGS").into(),
        cpu_features: [
            ("avx2", cfg!(target_feature = "avx2")),
            ("avx512f", cfg!(target_feature = "avx512f")),
        ]
        .into_iter()
        .filter_map(|(n, p)| p.then_some(n))
        .collect(),
        kernel: fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
            .into(),
        cpu_model: fs::read_to_string("/proc/cpuinfo")
            .unwrap_or_default()
            .lines()
            .find_map(|l| {
                l.strip_prefix("model name")
                    .and_then(|s| s.split_once(':'))
                    .map(|(_, s)| s.trim().to_owned())
            })
            .unwrap_or_default(),
        power_sensor: sensor.as_ref().map(|s| s.power.display().to_string()),
        energy_scope: "AMD APU/SoC package, including CPU and GPU; not whole-laptop or dock power",
        cases: vec![],
        acceptance_run: options.seconds >= 60.0
            && options.warmup >= 15.0
            && options.pairs >= 5
            && options.cpu_threads.is_none()
            && options.width == 1920
            && options.height == 1080
            && env!("VINO_BUILD_FLAGS").contains("target-cpu=native"),
        recommendation: "incomplete".into(),
    };
    for case in &options.cases {
        eprintln!("Preparing {} corpus", case.name());
        let corpus = Corpus::new(*case, &options);
        let mut sweep = Vec::new();
        for n in thread_counts(options.cpu_threads) {
            let mut cpu = Backend::Cpu(CpuEncoder::new(n)?);
            let row = measure(
                &mut cpu,
                &corpus,
                options.warmup.min(1.0),
                options.sweep_seconds,
                None,
                sensor.as_ref(),
            )?;
            eprintln!(
                "{} CPU {n} threads: {:.1} frame pairs/s",
                case.name(),
                row.frame_pairs_per_second
            );
            sweep.push(row);
        }
        let fastest = sweep
            .iter()
            .max_by(|a, b| {
                a.frame_pairs_per_second
                    .total_cmp(&b.frame_pairs_per_second)
            })
            .unwrap();
        let fastest_threads = fastest.cpu_threads.unwrap();
        let gpu_probe = measure(
            &mut gpu,
            &corpus,
            options.warmup.min(1.0),
            options.sweep_seconds,
            None,
            sensor.as_ref(),
        )?;
        let common = (fastest
            .frame_pairs_per_second
            .min(gpu_probe.frame_pairs_per_second)
            * 0.85)
            .min(60.0);
        eprintln!(
            "{} GPU: {:.1} frame pairs/s; common energy rate {:.2} Hz",
            case.name(),
            gpu_probe.frame_pairs_per_second,
            common
        );
        let mut energy_sweep = Vec::new();
        for n in thread_counts(options.cpu_threads) {
            let mut cpu = Backend::Cpu(CpuEncoder::new(n)?);
            energy_sweep.push(measure(
                &mut cpu,
                &corpus,
                options.warmup.min(1.0),
                options.sweep_seconds,
                Some(common),
                sensor.as_ref(),
            )?);
        }
        let energy_threads = energy_sweep
            .iter()
            .filter(|m| {
                m.joules_per_frame_pair.is_some() && m.frame_pairs_per_second >= common * 0.98
            })
            .min_by(|a, b| {
                a.joules_per_frame_pair
                    .unwrap()
                    .total_cmp(&b.joules_per_frame_pair.unwrap())
            })
            .and_then(|m| m.cpu_threads)
            .unwrap_or(fastest_threads);
        let mut cpu_fast = Backend::Cpu(CpuEncoder::new(fastest_threads)?);
        let mut cpu_energy = Backend::Cpu(CpuEncoder::new(energy_threads)?);
        let mut pairs = Vec::new();
        for index in 0..options.pairs {
            eprintln!(
                "{} pair {}/{} (CPU throughput {fastest_threads} threads, energy {energy_threads})",
                case.name(),
                index + 1,
                options.pairs
            );
            let (ct, gt, ce, ge) = if index % 2 == 0 {
                let ct = measure(
                    &mut cpu_fast,
                    &corpus,
                    options.warmup,
                    options.sweep_seconds,
                    None,
                    sensor.as_ref(),
                )?;
                let ce = measure(
                    &mut cpu_energy,
                    &corpus,
                    options.warmup,
                    options.seconds,
                    Some(common),
                    sensor.as_ref(),
                )?;
                let gt = measure(
                    &mut gpu,
                    &corpus,
                    options.warmup,
                    options.sweep_seconds,
                    None,
                    sensor.as_ref(),
                )?;
                let ge = measure(
                    &mut gpu,
                    &corpus,
                    options.warmup,
                    options.seconds,
                    Some(common),
                    sensor.as_ref(),
                )?;
                (ct, gt, ce, ge)
            } else {
                let gt = measure(
                    &mut gpu,
                    &corpus,
                    options.warmup,
                    options.sweep_seconds,
                    None,
                    sensor.as_ref(),
                )?;
                let ge = measure(
                    &mut gpu,
                    &corpus,
                    options.warmup,
                    options.seconds,
                    Some(common),
                    sensor.as_ref(),
                )?;
                let ct = measure(
                    &mut cpu_fast,
                    &corpus,
                    options.warmup,
                    options.sweep_seconds,
                    None,
                    sensor.as_ref(),
                )?;
                let ce = measure(
                    &mut cpu_energy,
                    &corpus,
                    options.warmup,
                    options.seconds,
                    Some(common),
                    sensor.as_ref(),
                )?;
                (ct, gt, ce, ge)
            };
            ensure!(
                ce.frame_pairs == ge.frame_pairs
                    && ce.encoded_bytes == ge.encoded_bytes
                    && ce.encoded_strips == ge.encoded_strips,
                "energy comparison did unequal work"
            );
            pairs.push(Pair {
                cpu_first: index % 2 == 0,
                cpu_throughput: ct,
                gpu_throughput: gt,
                cpu_energy: ce,
                gpu_energy: ge,
            });
            fs::write(
                options.output.join(format!("{}-pairs.json", case.name())),
                serde_json::to_vec_pretty(&pairs)?,
            )?;
        }
        let speeds: Vec<_> = pairs
            .iter()
            .map(|p| {
                p.gpu_throughput.frame_pairs_per_second / p.cpu_throughput.frame_pairs_per_second
            })
            .collect();
        let energies: Vec<_> = pairs
            .iter()
            .filter_map(|p| {
                Some(p.gpu_energy.joules_per_frame_pair? / p.cpu_energy.joules_per_frame_pair?)
            })
            .collect();
        let speed = geometric_mean(&speeds);
        let speed_ci = ratio_ci95(&speeds);
        let energy_ci = if energies.len() == pairs.len() {
            ratio_ci95(&energies)
        } else {
            None
        };
        let p99 = pairs
            .iter()
            .map(|p| p.gpu_throughput.p99_ms.max(p.gpu_energy.p99_ms))
            .fold(0.0, f64::max);
        let matched = pairs.iter().all(|p| {
            p.cpu_energy.frame_pairs_per_second >= common * 0.98
                && p.gpu_energy.frame_pairs_per_second >= common * 0.98
        });
        let capacity = pairs
            .iter()
            .all(|p| p.gpu_throughput.frame_pairs_per_second >= 60.0)
            && p99 <= 1000.0 / 60.0;
        report.cases.push(CaseReport {
            case: *case,
            cpu_sweep: sweep,
            energy_sweep,
            gpu_probe,
            fastest_cpu_threads: fastest_threads,
            energy_cpu_threads: energy_threads,
            common_rate_hz: common,
            pairs,
            throughput_speedup: speed,
            throughput_ratio_ci95: speed_ci,
            energy_ratio_ci95: energy_ci,
            gpu_p99_ms: p99,
            energy_rate_matched: matched,
            meets_speed: speed_ci.is_some_and(|ci| ci[0] >= 2.0),
            meets_energy: matched && energy_ci.is_some_and(|ci| ci[1] < 1.0),
            meets_60hz: capacity,
        });
        update_recommendation(&mut report);
        write_report(&report)?;
    }
    Ok(report)
}
fn update_recommendation(report: &mut Report) {
    let dense: Vec<_> = report
        .cases
        .iter()
        .filter(|r| matches!(r.case, Case::Motion | Case::Noise))
        .collect();
    report.recommendation = if !report.acceptance_run
        || !dense.iter().any(|r| r.case == Case::Motion)
        || !dense.iter().any(|r| r.case == Case::Noise)
    {
        "exploratory: full acceptance measurements are incomplete"
    } else if dense
        .iter()
        .all(|r| r.meets_speed && r.meets_energy && r.meets_60hz)
    {
        "go: measured speed, energy and 60 Hz thresholds met; live dock validation remains"
    } else {
        "no-go: the agreed speed, energy and 60 Hz thresholds were not all met"
    }
    .into();
}
fn write_report(report: &Report) -> Result<()> {
    fs::write(
        report.options.output.join("benchmark.json"),
        serde_json::to_vec_pretty(report)?,
    )?;
    let mut text=format!("# GPU encoder benchmark\n\n{}\n\nDevice: {}  \nBuild flags: {}  \nEnergy scope: {}\n\n| Case | GPU / CPU throughput | GPU p99 pair time | Energy ratio 95% CI | Matched energy rate | 60 Hz capacity |\n|---|---:|---:|---|---|---|\n",
        report.recommendation,report.device.name,report.rust_flags,report.energy_scope);
    for case in &report.cases {
        text.push_str(&format!(
            "| {} | {:.2}× | {:.2} ms | {} | {} | {} |\n",
            case.case.name(),
            case.throughput_speedup,
            case.gpu_p99_ms,
            case.energy_ratio_ci95
                .map(|ci| format!("{:.3}–{:.3}", ci[0], ci[1]))
                .unwrap_or_else(|| "insufficient samples".into()),
            case.energy_rate_matched,
            case.meets_60hz
        ));
    }
    text.push_str("\nEnergy ratios do not establish equal-rate savings when the matched-rate column is false. The CPU baseline is the native release Vino codec with a measured worker-count sweep. This does not compare against the proprietary DisplayLinkManager. Inputs are deterministic synthetic RGB888 sequences; timings include padding, damage detection, transfers, synchronisation, readback and framing. The benchmark does not send data to the dock.\n");
    fs::write(report.options.output.join("report.md"), text)?;
    Ok(())
}
/// One backend at a specified input rate, for comparison with live service CPU.
/// This remains an offline measurement and does not measure display delivery.
#[derive(clap::Args, Serialize)]
pub struct SampleOptions {
    #[arg(long, value_enum)]
    pub case: Case,
    #[arg(long, value_enum, default_value = "gpu")]
    pub backend: SampleBackend,
    #[arg(long, default_value_t = 30.0)]
    pub rate: f64,
    #[arg(long, default_value_t = 10.0)]
    pub seconds: f64,
    #[arg(long, default_value_t = 3.0)]
    pub warmup: f64,
    #[arg(long, default_value_t = 1920)]
    pub width: usize,
    #[arg(long, default_value_t = 1080)]
    pub height: usize,
    #[arg(long, default_value_t = workload::DEFAULT_SEED)]
    pub seed: u64,
    #[arg(long, default_value_t = 8)]
    pub cpu_threads: usize,
    #[arg(long)]
    pub output: PathBuf,
}
#[derive(Clone, Copy, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SampleBackend {
    Cpu,
    Gpu,
}

pub fn sample(device: Option<usize>, options: SampleOptions) -> Result<()> {
    ensure!(!cfg!(debug_assertions), "sample requires a release build");
    ensure!(
        options.rate.is_finite() && options.rate > 0.0 && options.rate <= 240.0,
        "sample rate must be in (0,240]"
    );
    ensure!(
        options.seconds.is_finite()
            && options.seconds > 0.0
            && options.seconds <= 3600.0
            && options.warmup.is_finite()
            && options.warmup >= 0.0
            && options.warmup <= 300.0,
        "invalid sample duration"
    );
    ensure!(
        options.width > 0 && options.width <= 3840 && options.height > 0 && options.height <= 2160,
        "sample dimensions must fit within 3840x2160"
    );
    ensure!(
        options.cpu_threads > 0 && options.cpu_threads <= 256,
        "invalid worker count"
    );
    ensure!(!options.output.exists(), "sample output already exists");
    let corpus = Corpus::new(
        options.case,
        &Options {
            width: options.width,
            height: options.height,
            seconds: options.seconds,
            warmup: options.warmup,
            pairs: 1,
            sweep_seconds: 1.0,
            cpu_threads: Some(options.cpu_threads),
            cases: vec![options.case],
            seed: options.seed,
            output: options.output.clone(),
        },
    );
    let (mut backend, sensor) = match options.backend {
        SampleBackend::Cpu => (Backend::Cpu(CpuEncoder::new(options.cpu_threads)?), None),
        SampleBackend::Gpu => {
            let gpu = GpuEncoder::new(device, false)?;
            let sensor = Sensor::discover(&gpu.info);
            (Backend::Gpu(Box::new(gpu)), sensor)
        }
    };
    let measurement = measure(
        &mut backend,
        &corpus,
        options.warmup,
        options.seconds,
        Some(options.rate),
        sensor.as_ref(),
    )?;
    let report = serde_json::json!({
        "schema_version": 1, "scope": "offline; no display delivery or USB",
        "options": options, "rust_flags": env!("VINO_BUILD_FLAGS"), "measurement": measurement,
    });
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.output)?;
    file.write_all(serde_json::to_string_pretty(&report)?.as_bytes())?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn power_integration_covers_exact_measurement_window() {
        let sample = |seconds, watts| PowerSample {
            seconds,
            watts,
            temperature_c: None,
        };
        assert_eq!(
            integrate_power(&[sample(0.1, 10.0), sample(2.0, 10.0)], 1.0),
            Some(10.0)
        );
        assert_eq!(
            integrate_power(&[sample(0.0, 10.0), sample(2.0, 30.0)], 1.0),
            Some(15.0)
        );
        assert_eq!(
            integrate_power(&[sample(0.0, 10.0), sample(0.9, 10.0)], 1.0),
            None
        );
    }
    #[test]
    fn confidence_requires_repeated_pairs() {
        assert!(ratio_ci95(&[0.8; 4]).is_none());
        assert_eq!(ratio_ci95(&[1.0; 5]), Some([1.0, 1.0]));
        let ci = ratio_ci95(&[0.5; 5]).unwrap();
        assert!((ci[0] - 0.5).abs() < 1e-12 && (ci[1] - 0.5).abs() < 1e-12);
    }
}
