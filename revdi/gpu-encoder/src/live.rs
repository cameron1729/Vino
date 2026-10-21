// SPDX-License-Identifier: GPL-2.0-or-later
use anyhow::{bail, ensure, Context, Result};
use clap::{Parser, ValueEnum};
use serde_json::json;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use vino_chimera::{
    session::PresentStats,
    trial::{
        self, CaptureDiagnostics, CaptureMode, CaptureStats, ComparisonMode, CursorMode, Observer,
        Source,
    },
};
use vino_gpu_encoder::{gpu::GpuEncoder, CpuEncoder};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Cpu,
    Gpu,
}
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Input {
    Pattern,
    Desktop,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CursorHandling {
    Client,
    Evdi,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CaptureHandling {
    Copied,
    Direct,
    Shared,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ComparisonHandling {
    Cpu,
    Gpu,
}

#[derive(Parser)]
#[command(
    about = "Bounded Ella GPU display trial; use tools/gpu/live-trial.py for supervised takeover"
)]
struct Cli {
    #[arg(long)]
    dock: String,
    #[arg(long, default_value_t=0, value_parser=clap::value_parser!(u8).range(0..=1))]
    head: u8,
    /// Existing /dev/dri/cardN index; never creates EVDI cards.
    #[arg(long, value_parser=clap::value_parser!(u8).range(0..64))]
    card: u8,
    #[arg(long, default_value_t = 0)]
    device: usize,
    #[arg(long, value_enum, default_value = "gpu")]
    encoder: Backend,
    #[arg(long, value_enum, default_value = "desktop")]
    source: Input,
    /// Direct capture shares native mapped buffers with the GPU and requires client cursor events.
    #[arg(long, value_enum, default_value = "copied")]
    capture: CaptureHandling,
    #[arg(long, value_enum, default_value = "cpu")]
    comparison: ComparisonHandling,
    /// Compose cursor events in the client, or retain EVDI's combined capture for comparison.
    #[arg(long, value_enum, default_value = "evdi")]
    cursor: CursorHandling,
    #[arg(long, value_parser=clap::value_parser!(u64).range(1..=900))]
    duration: Option<u64>,
    #[arg(long, value_parser=clap::value_parser!(u32).range(1..=60))]
    fps: Option<u32>,
    #[arg(long)]
    log: Option<PathBuf>,
    /// Validate arguments, EVDI/USB sysfs identity and GPU creation without claiming devices.
    #[arg(long)]
    check: bool,
    #[arg(long)]
    validation: bool,
}

struct Log {
    file: BufWriter<File>,
    started: Instant,
    health_at: Instant,
    notify_at: Instant,
    ready: bool,
    frames: u64,
    captures: u64,
    superseded_captures: u64,
    cursor_events: u64,
    bytes: u64,
    cpu_start: f64,
    capture_diagnostics: CaptureDiagnostics,
    loop_iterations: u64,
    sensor: Option<PathBuf>,
}
impl Log {
    fn emit(&mut self, mut value: serde_json::Value) -> Result<()> {
        value["elapsed_s"] = json!(self.started.elapsed().as_secs_f64());
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: now is a valid writable timespec; this clock matches Python monotonic_ns.
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } == 0,
            "cannot read the monotonic clock"
        );
        value["monotonic_ns"] = json!(now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64);
        serde_json::to_writer(&mut self.file, &value)?;
        self.file.write_all(b"\n")?;
        Ok(())
    }
}
impl Observer for Log {
    fn phase(&mut self, phase: &str) -> Result<(), String> {
        self.emit(json!({"event":"phase","phase":phase}))
            .map_err(|e| e.to_string())?;
        self.file.flush().map_err(|e| e.to_string())?;
        let action = match phase {
            // Start X11 diagnostics before connecting EVDI. After a modeset, X11 can
            // wait for our first capture, so waiting for a new X client there deadlocks.
            "engaging" => Some("trace-start"),
            "waiting_for_mode" => Some("layout"),
            _ => None,
        };
        if let Some(action) = action {
            let directory =
                std::env::var("VINO_TRIAL_DIR").map_err(|_| "missing trial directory")?;
            let output = std::process::Command::new("/usr/bin/python3")
                .arg(format!("{directory}/helper.py"))
                .arg(action)
                .arg(&directory)
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(format!(
                    "X11 trial {action} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        Ok(())
    }
    fn connected(&mut self, edid: &[u8]) -> Result<(), String> {
        let hex: String = edid.iter().map(|b| format!("{b:02x}")).collect();
        self.emit(json!({"event":"monitor","edid":hex}))
            .map_err(|e| e.to_string())
    }
    fn frame(&mut self, capture: CaptureStats, stats: PresentStats) -> Result<(), String> {
        self.captures = self.captures.max(capture.sequence);
        self.cursor_events += capture.cursor_events;
        self.frames += u64::from(stats.presentations > 0);
        self.bytes += stats.bytes as u64;
        self.emit(json!({"event":"frame","captured":capture.captured,"capture_ms":capture.elapsed.as_secs_f64()*1000.0,
            "content_changed":capture.content_changed,
            "capture_sequence":capture.sequence,"capture_age_ms":capture.age.as_secs_f64()*1000.0,
            "damage_rects":capture.damage_rects,"damage_pixels":capture.damage_pixels,
            "cursor_events":capture.cursor_events,"cursor_composite_ms":capture.cursor_composite.as_secs_f64()*1000.0,
            "changed_strips":stats.changed_strips,"changed_bounds":stats.changed_bounds,
            "encode_ms":stats.encode_time.as_secs_f64()*1000.0,"usb_ms":stats.usb_time.as_secs_f64()*1000.0,
            "bytes":stats.bytes,"presentations":stats.presentations})).map_err(|e|e.to_string())
    }
    fn capture_totals(&mut self, captures: u64, superseded: u64) {
        self.captures = captures;
        self.superseded_captures = superseded;
    }
    fn capture_diagnostics(&mut self, diagnostics: CaptureDiagnostics) {
        self.capture_diagnostics = diagnostics;
    }
    fn progress(&mut self, ready: bool) -> Result<(), String> {
        self.loop_iterations += 1;
        if ready && !self.ready {
            notify(b"READY=1\nWATCHDOG=1").map_err(|e| e.to_string())?;
            self.ready = true;
        }
        if self.notify_at.elapsed() >= Duration::from_secs(1) {
            if self.ready {
                notify(b"WATCHDOG=1").map_err(|e| e.to_string())?;
            }
            self.notify_at = Instant::now();
        }
        if self.health_at.elapsed() >= Duration::from_secs(1) {
            self.emit(
                json!({"event":"health","frames_submitted":self.frames,"captures":self.captures,
                "cursor_events":self.cursor_events,"superseded_captures":self.superseded_captures,
                "bytes":self.bytes,"process_cpu_seconds":cpu_seconds()-self.cpu_start,
                "unchanged_captures":self.capture_diagnostics.unchanged,
                "capture_event_waits":self.capture_diagnostics.event_waits,
                "rgb_conversion_ms":self.capture_diagnostics.conversion_time.as_secs_f64()*1000.0,
                "rgb_comparison_ms":self.capture_diagnostics.comparison_time.as_secs_f64()*1000.0,
                "snapshot_bytes":self.capture_diagnostics.snapshot_bytes,
                "repair_bytes":self.capture_diagnostics.repair_bytes,
                "repair_ms":self.capture_diagnostics.repair_time.as_secs_f64()*1000.0,
                "grab_ms":self.capture_diagnostics.grab_time.as_secs_f64()*1000.0,
                "pool_peak_leases":self.capture_diagnostics.pool_peak_leases,
                "pool_exhaustions":self.capture_diagnostics.pool_exhaustions,
                "shared_capture_copies":self.capture_diagnostics.shared_capture_copies,
                "shared_capture_bytes":self.capture_diagnostics.shared_capture_bytes,
                "shared_capture_imports":self.capture_diagnostics.shared_capture_imports,
                "shared_capture_ms":self.capture_diagnostics.shared_capture_time.as_secs_f64()*1000.0,
                "presentation_loop_iterations":self.loop_iterations,
                "thread_cpu":thread_cpu(),"gpu_sensor":self.sensor.as_deref().map(sensor_sample),
                "queue_capacity":1}),
            )
            .map_err(|e| e.to_string())?;
            self.file.flush().map_err(|e| e.to_string())?;
            self.health_at = Instant::now();
        }
        Ok(())
    }
    fn stopping(&self) -> bool {
        STOP.load(Ordering::Relaxed)
    }
}

// These once-per-second diagnostics are optional. A missing counter must not stop display
// recovery or be presented as zero activity. GPU readings cover the device, not just Vino.
fn thread_cpu() -> Vec<serde_json::Value> {
    // SAFETY: sysconf has no pointer arguments and only reads the clock-tick scale.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks <= 0 {
        return Vec::new();
    }
    let Ok(tasks) = fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|task| {
            let text = fs::read_to_string(task.path().join("stat")).ok()?;
            let (left, rest) = text.rsplit_once(')')?;
            let (tid, name) = left.split_once('(')?;
            let fields: Vec<_> = rest.split_whitespace().collect();
            let user = fields.get(11)?.parse::<u64>().ok()?;
            let system = fields.get(12)?.parse::<u64>().ok()?;
            Some(json!({"tid":tid.trim().parse::<u32>().ok()?,"name":name,
            "user_seconds":user as f64 / ticks as f64,
            "system_seconds":system as f64 / ticks as f64}))
        })
        .collect()
}

fn sensor_for(info: &vino_gpu_encoder::gpu::DeviceInfo) -> Option<PathBuf> {
    let mut matches = Vec::new();
    for entry in fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_str()?;
        if !name
            .strip_prefix("card")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        let device = entry.path().join("device");
        if fs::read_to_string(device.join("vendor"))
            .ok()
            .as_deref()
            .map(str::trim)
            != Some(format!("0x{:04x}", info.vendor_id).as_str())
            || fs::read_to_string(device.join("device"))
                .ok()
                .as_deref()
                .map(str::trim)
                != Some(format!("0x{:04x}", info.device_id).as_str())
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
                matches.push(hw.path());
            }
        }
    }
    (matches.len() == 1).then(|| matches.remove(0))
}

fn sensor_sample(hwmon: &std::path::Path) -> serde_json::Value {
    let read = |path: PathBuf| {
        fs::read_to_string(path)
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    };
    json!({"scope":"whole AMD device; not Vino alone or whole-laptop power",
        "reported_watts":read(hwmon.join("power1_average")).map(|v| v / 1e6),
        "temperature_c":read(hwmon.join("temp1_input")).map(|v| v / 1e3),
        "gpu_clock_mhz":read(hwmon.join("freq1_input")).map(|v| v / 1e6),
        "gpu_busy_percent":hwmon.parent().and_then(std::path::Path::parent).and_then(|p| read(p.join("gpu_busy_percent")))})
}

fn cpu_seconds() -> f64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid writable timespec and a process CPU clock identifier.
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t);
    }
    t.tv_sec as f64 + t.tv_nsec as f64 / 1e9
}

fn notify(message: &[u8]) -> Result<()> {
    let path = std::env::var("NOTIFY_SOCKET").context("missing systemd notification socket")?;
    let addr = if let Some(name) = path.strip_prefix('@') {
        SocketAddr::from_abstract_name(name)?
    } else {
        SocketAddr::from_pathname(path)?
    };
    UnixDatagram::unbound()?.send_to_addr(message, &addr)?;
    Ok(())
}

fn validate_capture_arguments(args: &Cli) -> Result<()> {
    ensure!(
        matches!(args.capture, CaptureHandling::Copied)
            || (matches!(args.encoder, Backend::Gpu)
                && matches!(args.source, Input::Desktop)
                && matches!(args.cursor, CursorHandling::Client)
                && args.head == 0),
        "direct capture requires --encoder gpu --source desktop --cursor client --head 0"
    );
    ensure!(
        !matches!(args.comparison, ComparisonHandling::Gpu)
            || !matches!(args.capture, CaptureHandling::Copied),
        "GPU comparison requires --capture direct or shared"
    );
    Ok(())
}

fn shared_copier(
    mode: CaptureHandling,
    encoder: &GpuEncoder,
) -> Result<Option<Box<dyn vino_chimera::shared_capture::SharedCaptureCopy>>> {
    if matches!(mode, CaptureHandling::Shared) {
        Ok(Some(Box::new(encoder.create_shared_capture_copier()?)))
    } else {
        Ok(None)
    }
}

fn direct_pool(
    args: CaptureHandling,
    encoder: &mut GpuEncoder,
) -> Result<Option<std::sync::Arc<vino_chimera::capture_pool::CapturePool>>> {
    if matches!(args, CaptureHandling::Copied) {
        return Ok(None);
    }
    ensure!(
        encoder.info.vendor_id == 0x1002 && encoder.info.device_type == "INTEGRATED_GPU",
        "the direct desktop trial requires an integrated AMD GPU"
    );
    Ok(Some(encoder.create_capture_pool(1920, 1080, 1088)?))
}

fn preflight(args: &Cli) -> Result<()> {
    validate_capture_arguments(args)?;
    if matches!(args.capture, CaptureHandling::Shared) {
        ensure!(
            fs::read_to_string("/sys/module/evdi/parameters/shared_capture_abi")
                .map(|value| value.trim() == "1")
                .unwrap_or(false),
            "shared capture requires the prepared experimental EVDI module (ABI 1)"
        );
    }
    // Accept only a sysfs leaf name, never a user-supplied path traversal.
    ensure!(
        !args.dock.is_empty()
            && args
                .dock
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'.'),
        "invalid dock path"
    );
    let usb = PathBuf::from("/sys/bus/usb/devices").join(&args.dock);
    ensure!(
        fs::read_to_string(usb.join("idVendor"))?.trim() == "17e9"
            && fs::read_to_string(usb.join("idProduct"))?.trim() == "4307",
        "expected Ella 17e9:4307"
    );
    let driver = fs::canonicalize(format!("/sys/class/drm/card{}/device/driver", args.card))?;
    ensure!(
        driver.file_name().is_some_and(|n| n == "evdi"),
        "selected DRM card is not EVDI"
    );
    let version = fs::read_to_string("/sys/devices/evdi/version")?;
    ensure!(
        version.trim() == "1.15.1",
        "first trial requires installed EVDI 1.15.1; found {}",
        version.trim()
    );
    Ok(())
}

fn main() -> Result<()> {
    let args = Cli::parse();
    preflight(&args)?;
    if args.check {
        if matches!(args.encoder, Backend::Gpu) {
            let mut gpu = GpuEncoder::new(Some(args.device), args.validation)?;
            let pool = direct_pool(args.capture, &mut gpu)?;
            let _copier = shared_copier(args.capture, &gpu)?;
            println!(
                "{}",
                serde_json::to_string(&json!({"preflight":"ok","gpu":gpu.info,
                    "capture":format!("{:?}",args.capture).to_lowercase(),
                    "comparison":format!("{:?}",args.comparison).to_lowercase(),
                    "capture_slots":if pool.is_some() {4} else {0}}))?
            );
        } else {
            println!("{{\"preflight\":\"ok\",\"encoder\":\"cpu\"}}");
        }
        return Ok(());
    }
    ensure!(
        std::env::var("VINO_SUPERVISED").as_deref() == Ok("1")
            && std::env::var_os("NOTIFY_SOCKET").is_some(),
        "launch with tools/gpu/live-trial.py; supervision is required"
    );
    // SAFETY: the handlers only store to a lock-free AtomicBool.
    unsafe {
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
    }
    let source = match args.source {
        Input::Pattern => Source::Pattern,
        Input::Desktop => Source::Desktop,
    };
    let config = trial::Config {
        dock: args.dock,
        head: args.head,
        card: i32::from(args.card),
        source,
        cursor: match args.cursor {
            CursorHandling::Client => CursorMode::Client,
            CursorHandling::Evdi => CursorMode::Evdi,
        },
        capture: match args.capture {
            CaptureHandling::Copied => CaptureMode::Copied,
            CaptureHandling::Direct => CaptureMode::Direct,
            CaptureHandling::Shared => CaptureMode::Shared,
        },
        comparison: match args.comparison {
            ComparisonHandling::Cpu => ComparisonMode::Cpu,
            ComparisonHandling::Gpu => ComparisonMode::Gpu,
        },
        duration: Duration::from_secs(args.duration.unwrap_or(if source == Source::Pattern {
            60
        } else {
            900
        })),
        fps: args
            .fps
            .unwrap_or(if source == Source::Pattern { 10 } else { 60 }),
    };
    let now = Instant::now();
    let mut log = Log {
        file: BufWriter::new(File::create(args.log.context("--log is required")?)?),
        started: now,
        health_at: now,
        notify_at: now,
        ready: false,
        frames: 0,
        captures: 0,
        superseded_captures: 0,
        cursor_events: 0,
        bytes: 0,
        cpu_start: cpu_seconds(),
        capture_diagnostics: CaptureDiagnostics::default(),
        loop_iterations: 0,
        sensor: None,
    };
    log.emit(
        json!({"event":"start","encoder":format!("{:?}",args.encoder).to_lowercase(),
        "source":format!("{:?}",args.source).to_lowercase(),"dock":config.dock,"head":config.head,
        "card":config.card,"fps_cap":config.fps,"duration_s":config.duration.as_secs(),
        "capture_mode":if source == Source::Desktop {"independent"} else {"pattern"},
        "capture_storage":format!("{:?}",args.capture).to_lowercase(),
        "comparison":format!("{:?}",args.comparison).to_lowercase(),
        "cursor_mode":format!("{:?}",args.cursor).to_lowercase()}),
    )?;
    let result = match args.encoder {
        Backend::Gpu => match GpuEncoder::new(Some(args.device), args.validation) {
            Ok(mut encoder) => {
                log.sensor = sensor_for(&encoder.info);
                log.emit(json!({"event":"gpu","device":encoder.info}))?;
                log.emit(json!({"event":"diagnostics","sensor":log.sensor,
                    "scope":"cumulative capture/CPU counters; device-wide AMD sensors"}))?;
                let pool = direct_pool(args.capture, &mut encoder)?;
                let copier = shared_copier(args.capture, &encoder)?;
                let result =
                    trial::run_with_capture_sources(&config, &mut encoder, &mut log, pool, copier);
                log.emit(json!({"event":"gpu_totals","dispatches":encoder.dispatches,
                    "encoded_strips":encoder.encoded_strips,"stages":encoder.stage_times}))?;
                result
            }
            Err(e) => Err(format!("GPU initialization: {e:#}")),
        },
        Backend::Cpu => match CpuEncoder::new(1) {
            Ok(mut encoder) => trial::run(&config, &mut encoder, &mut log),
            Err(e) => Err(format!("CPU initialization: {e:#}")),
        },
    };
    log.emit(
        json!({"event":"exit","ok":result.is_ok(),"error":result.as_ref().err(),
        "frames_submitted":log.frames,"captures":log.captures,"superseded_captures":log.superseded_captures,
        "process_cpu_seconds":cpu_seconds()-log.cpu_start}),
    )?;
    log.file.flush()?;
    if let Err(error) = result {
        bail!("{error}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_is_default_and_direct_rejects_unsupported_combinations() {
        let base = ["worker", "--dock", "8-1.2", "--card", "0"];
        let default = Cli::try_parse_from(base).unwrap();
        assert!(matches!(default.capture, CaptureHandling::Copied));
        assert!(validate_capture_arguments(&default).is_ok());
        let mut direct = base.to_vec();
        direct.extend(["--capture", "direct", "--cursor", "client"]);
        assert!(validate_capture_arguments(&Cli::try_parse_from(&direct).unwrap()).is_ok());
        for extra in [
            ["--encoder", "cpu"],
            ["--source", "pattern"],
            ["--head", "1"],
        ] {
            let mut args = direct.clone();
            args.extend(extra);
            assert!(validate_capture_arguments(&Cli::try_parse_from(args).unwrap()).is_err());
        }
        let mut kernel_cursor = base.to_vec();
        kernel_cursor.extend(["--capture", "direct"]);
        assert!(validate_capture_arguments(&Cli::try_parse_from(kernel_cursor).unwrap()).is_err());
    }
    #[test]
    fn gpu_comparison_and_shared_capture_require_supported_native_source() {
        let base = [
            "worker",
            "--dock",
            "8-1.2",
            "--card",
            "0",
            "--comparison",
            "gpu",
        ];
        assert!(validate_capture_arguments(&Cli::try_parse_from(base).unwrap()).is_err());
        for capture in ["direct", "shared"] {
            let mut args = base.to_vec();
            args.extend(["--capture", capture, "--cursor", "client"]);
            assert!(validate_capture_arguments(&Cli::try_parse_from(&args).unwrap()).is_ok());
            for extra in [
                ["--encoder", "cpu"],
                ["--source", "pattern"],
                ["--head", "1"],
            ] {
                let mut invalid = args.clone();
                invalid.extend(extra);
                assert!(
                    validate_capture_arguments(&Cli::try_parse_from(invalid).unwrap()).is_err()
                );
            }
        }
    }
}
