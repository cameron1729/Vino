#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Bounded, single-monitor live DisplayLink/Vino comparison with systemd recovery."""
import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
spec = importlib.util.spec_from_file_location("live_trial", HERE / ("helper.py" if (HERE / "helper.py").exists() else "live-trial.py"))
trial = importlib.util.module_from_spec(spec)
spec.loader.exec_module(trial)
spec = importlib.util.spec_from_file_location("comparison_monitor", HERE / "comparison-monitor.py")
monitor = importlib.util.module_from_spec(spec)
spec.loader.exec_module(monitor)
CASES = ("idle", "scroll", "motion")
FRAME_BYTES = 1920 * 1080 * 4
TICKS = os.sysconf("SC_CLK_TCK")


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def plan(pairs, compare_evdi_rows=None, gpu_evdi_rows=None):
    variants = [dict(backend=b, condition=b, evdi_copy_rows=None) for b in ("official", "gpu")]
    if compare_evdi_rows is not None:
        variants = [dict(backend=b, condition=f"{b}-evdi-{rows}", evdi_copy_rows=rows)
                    for b, rows in (("official", 0), ("gpu", 0), ("gpu", compare_evdi_rows))]
    elif gpu_evdi_rows is not None:
        variants = [dict(backend=b, condition=f"{b}-evdi-{rows}", evdi_copy_rows=rows)
                    for b, rows in (("official", 0), ("gpu", gpu_evdi_rows))]
    return [dict(repeat=r, **variant, cases=list(CASES[r % 3:] + CASES[:r % 3]))
            for r in range(pairs) for variant in (variants if r % 2 == 0 else variants[::-1])]


def runtime_bound(settings):
    return len(plan(settings["pairs"], settings.get("compare_evdi_rows"), settings.get("gpu_evdi_rows"))) * (
        3 * (settings["warmup"] + settings["seconds"] + 10) + settings["settle"] + 65) + 10


def gpu_worker_arguments(settings):
    """Use the same capture configuration for the hardware check and live worker."""
    capture = settings.get("capture", "copied")
    if capture not in ("copied", "direct"):
        raise ValueError("capture must be copied or direct")
    if capture == "direct" and settings["head"] != 0:
        raise ValueError("direct capture requires --head 0")
    arguments = ["--encoder", "gpu", "--source", "desktop", "--dock", settings["dock"],
                 "--head", settings["head"], "--card", settings["card"], "--device", settings["device"]]
    # Historical workers default to copied capture and EVDI cursors. Omitting both
    # flags preserves --binary support for workers predating these options.
    if capture == "direct":
        arguments += ["--capture", "direct", "--cursor", "client"]
    return arguments


def evdi_state():
    return dict(srcversion=trial.EVDI_VERSION_PATH.read_text().strip(),
                copy_batch_rows=int(trial.EVDI_BATCH_PARAMETER.read_text())
                if trial.EVDI_BATCH_PARAMETER.exists() else None)


def verify_evdi(state):
    expected = dict(state["evdi_module"])
    if state.get("evdi_copy"):
        expected["copy_batch_rows"] = state["evdi_copy"]["rows"]
    current = evdi_state()
    if current != expected:
        raise RuntimeError(f"EVDI configuration changed: expected {expected}, got {current}")
    return current


def process_counter(pid):
    # The comm field may contain spaces and parentheses; fields after its final ')' are stable.
    raw = Path(f"/proc/{pid}/stat").read_text()
    fields = raw[raw.rfind(")") + 2:].split()
    return dict(pid=pid, start_ticks=int(fields[19]), cpu_seconds=(int(fields[11]) + int(fields[12])) / TICKS)


def cpu_delta(before, after, elapsed):
    if elapsed <= 0 or (before["pid"], before["start_ticks"]) != (after["pid"], after["start_ticks"]):
        raise RuntimeError("process identity changed during measurement")
    seconds = after["cpu_seconds"] - before["cpu_seconds"]
    if seconds < 0:
        raise RuntimeError("CPU counter moved backwards")
    return seconds / elapsed


def system_cpu():
    ticks = [int(v) for v in Path("/proc/stat").read_text().splitlines()[0].split()[1:]]
    # user/nice include guest time already; iowait is not busy CPU time.
    return sum(ticks[i] for i in (0, 1, 2, 5, 6, 7)) / TICKS


def service_cpu():
    fields = dict(line.split() for line in Path("/sys/fs/cgroup/system.slice/displaylink-driver.service/cpu.stat").read_text().splitlines())
    return int(fields["usage_usec"]) / 1e6


def one_pid(name):
    pids = trial.command(["pgrep", "-x", name]).stdout.split()
    if len(pids) != 1:
        raise RuntimeError(f"expected one {name} process")
    return int(pids[0])


def driver_pid():
    pid = int(trial.command(["systemctl", "show", trial.SERVICE, "-p", "MainPID", "--value"]).stdout)
    if pid <= 0:
        raise RuntimeError("official DisplayLink service is not running")
    return pid


def discover_sensor():
    found = []
    for hwmon in sorted(Path("/sys/class/drm").glob("card[0-9]*/device/hwmon/hwmon*")):
        if (hwmon / "name").read_text().strip() == "amdgpu" and (hwmon / "power1_average").is_file():
            found.append(dict(path=str(hwmon), power_label=(hwmon / "power1_label").read_text().strip(),
                              temperature_label=(hwmon / "temp1_label").read_text().strip(),
                              thermal_sensors=monitor.thermal_sensors(),
                              scope="AMD hwmon reported power; not whole-laptop or wall power"))
    if len(found) != 1:
        raise RuntimeError("expected one readable AMD power sensor")
    power_sample(found[0])
    return found[0]


def power_sample(sensor):
    path = Path(sensor["path"])
    return dict(monotonic_s=time.monotonic(), watts=int((path / "power1_average").read_text()) / 1e6,
                temperature_c=int((path / "temp1_input").read_text()) / 1000,
                gpu_clock_mhz=monitor.optional_number(path / "freq1_input", 1e6),
                gpu_busy_percent=monitor.optional_number(path.parent.parent / "gpu_busy_percent"),
                thermal={entry["id"]: monitor.optional_number(entry["path"], entry["scale"])
                         for entry in sensor.get("thermal_sensors", [])})


def power_summary(samples):
    if len(samples) < 2:
        raise RuntimeError("insufficient power samples")
    span = samples[-1]["monotonic_s"] - samples[0]["monotonic_s"]
    if span <= 0:
        raise RuntimeError("invalid power sample interval")
    gaps = [b["monotonic_s"] - a["monotonic_s"] for a, b in zip(samples, samples[1:])]
    if min(gaps) <= 0:
        raise RuntimeError("power samples are not ordered")
    energy = sum(dt * (a["watts"] + b["watts"]) / 2 for a, b, dt in zip(samples, samples[1:], gaps))
    activity = {key: monitor.sampled_metric(samples, lambda s, k=key: s.get(k))
                for key in ("gpu_clock_mhz", "gpu_busy_percent")}
    thermal = {key: monitor.sampled_metric(samples, lambda s, k=key: s.get("thermal", {}).get(k))
               for key in sorted({key for sample in samples for key in sample.get("thermal", {})})}
    return dict(sample_count=len(samples), sampled_seconds=span, max_gap_seconds=max(gaps),
                mean_watts=energy / span, estimated_joules=energy,
                activity=activity, thermal=thermal,
                temperature_start_c=samples[0]["temperature_c"], temperature_end_c=samples[-1]["temperature_c"],
                temperature_max_c=max(s["temperature_c"] for s in samples))


def power_source(root=Path("/sys/class/power_supply")):
    # This class also contains mouse/keyboard batteries. Their online flag means
    # the peripheral is reachable, not that the laptop's charger is connected.
    result = {}
    for supply in sorted(root.glob("*")):
        try:
            kind = (supply / "type").read_text().strip()
        except FileNotFoundError:
            continue  # A peripheral may disappear while enumerating the class.
        if kind == "Battery":
            continue
        try:
            scope = (supply / "scope").read_text().strip()
        except FileNotFoundError:
            scope = None  # ACPI Mains supplies commonly omit scope.
        if scope == "Device":
            continue
        if kind != "Mains" and not (scope == "System" and (kind.startswith("USB") or kind == "Wireless")):
            continue
        path = supply / "online"
        try:
            result[str(path)] = path.read_text().strip()
        except FileNotFoundError:
            continue
    return result


def verify_power_source(expected):
    current = power_source()
    if current != expected:
        changes = [f"{Path(path).parent.name}/online: {expected.get(path, 'missing')} -> {current.get(path, 'missing')}"
                   for path in sorted(expected.keys() | current.keys()) if expected.get(path) != current.get(path)]
        raise RuntimeError("system external-power readings changed: " + "; ".join(changes))
    return current


def target_layout(state, current):
    saved = []
    for output, mode, x, primary in ((state["panel"], state["panel"]["mode"], 0, True),
                                      (state["target"], "1920x1080", int(state["panel"]["mode"].split("x")[0]), False)):
        saved.append({**output, "mode": mode, "x": x, "y": 0, "primary": primary,
                      "rate": state["panel"]["rate"] if primary else "60"})
    return saved, trial.layout_arguments(saved, current)


def verify_layout(state):
    current = trial.outputs(state)
    wanted, _ = target_layout(state, current)
    if len([o for o in current if o["mode"]]) != 2:
        raise RuntimeError("comparison requires only the laptop and Dell enabled")
    for old in wanted:
        new = trial.match_output(old, current)
        if any(new[k] != old[k] for k in ("mode", "x", "y", "primary")) or abs(float(new["rate"] or 0) - float(old["rate"])) > .1:
            raise RuntimeError("comparison display geometry or refresh changed")


def verify_environment(state):
    verify_evdi(state)
    verify_layout(state)
    for name, saved in state["processes"].items():
        now = process_counter(saved["pid"])
        if now["start_ticks"] != saved["start_ticks"]:
            raise RuntimeError(f"{name} restarted")
    if trial.compositor_active(state) != state["compositor_active"]:
        raise RuntimeError("KWin compositor state changed")
    return verify_power_source(state["power_source"])


def prepare_inputs(args):
    args.inputs.mkdir(parents=True, exist_ok=True)
    # Export deliberately refuses overwrites. Build a fresh set, then replace our
    # generated inputs; the manifest is last so interrupted preparation fails closed.
    with tempfile.TemporaryDirectory(prefix=".prepare-", dir=args.inputs) as name:
        stage = Path(name)
        trial.command(["cc", "-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", HERE / "live-replay.c",
                       "-o", stage / "live-replay", "-lX11", "-lXext", "-lm"], timeout=30)
        for case in CASES:
            trial.command([args.exporter, "export", "--case", case, "--output", stage / f"{case}.bgrx"], timeout=60)
        files = [stage / "live-replay", *(stage / f"{case}.bgrx" for case in CASES)]
        manifest = dict(schema=1, dimensions=[1920, 1080], frames_per_head=16, interleaved_heads=2,
                        files={p.name: digest(p) for p in files}, gpu_binary_sha256=digest(args.binary),
                        exporter_sha256=digest(args.exporter), player_source_sha256=digest(HERE / "live-replay.c"))
        for path in files:
            path.replace(args.inputs / path.name)
        trial.save(args.inputs / "manifest.json", manifest)
    print(f"Prepared replay and identical source pixels: {args.inputs}", flush=True)


def preflight(args):
    worker_arguments = gpu_worker_arguments(vars(args))
    manifest = json.loads((args.inputs / "manifest.json").read_text())
    for name in ("live-replay", *(f"{case}.bgrx" for case in CASES)):
        if digest(args.inputs / name) != manifest["files"].get(name):
            raise RuntimeError(f"replay input changed: {name}; rerun prepare")
    if digest(args.binary) != manifest["gpu_binary_sha256"] or digest(HERE / "live-replay.c") != manifest["player_source_sha256"]:
        raise RuntimeError("binary or player source changed; rerun prepare")
    state = trial.snapshot(args)
    state["screen_lock_check"] = json.loads(trial.session_command(
        state, ["/usr/bin/python3", HERE / "comparison-monitor.py", "check"]).stdout)
    state["evdi_module"] = evdi_state()
    if state["evdi_module"]["copy_batch_rows"] not in (None, 0):
        raise RuntimeError("Start the comparison with EVDI batching disabled")
    targets = [o for o in state["outputs"] if o["connected"] and o["edid"].startswith("00ffffffffffff0010ac")]
    if len(targets) != 1 or not state["panel"]["rate"]:
        raise RuntimeError("expected one Dell and an active laptop panel with a known refresh rate")
    state.update(target=targets[0], expected_monitor_edid=targets[0]["edid"],
                 processes={n: process_counter(one_pid(n)) for n in ("Xorg", "kwin_x11")},
                 compositor_active=trial.compositor_active(state), power_source=power_source(),
                 sensor=discover_sensor(), inputs=manifest,
                 settings={k: getattr(args, k) for k in (
                     "pairs", "warmup", "seconds", "settle", "dock", "head", "card", "device",
                     "input_fps", "capture", "compare_evdi_rows", "gpu_evdi_rows",
                     "max_background_cpu", "max_background_delta", "max_temperature_delta")})
    state["settings"]["cursor"] = "client" if args.capture == "direct" else "evdi"
    if not state["power_source"]:
        raise RuntimeError("no system external-power inputs found; power consistency cannot be checked")
    trial.command([args.binary, "--check", *worker_arguments], timeout=25)
    driver_pid()
    service_cpu()
    state["versions"] = trial.command(["dpkg-query", "-W", "displaylink-driver", "evdi"]).stdout
    state["kernel"] = trial.command(["uname", "-r"]).stdout.strip()
    state["official_sha256"] = digest("/usr/lib/displaylink-driver/DisplayLinkManager")
    state["harness_sha256"] = {p.name: digest(p) for p in (
        Path(__file__), HERE / "live-trial.py", HERE / "comparison-monitor.py")}
    return state


def unit_command(directory, duration, *, user=False, mock=None):
    worker = ["/usr/bin/python3", directory / "comparison.py", "worker", directory]
    if mock:
        worker = ["/usr/bin/python3", directory / "comparison.py", "mock-worker", mock]
    command = trial.unit_command(directory, worker, duration, user=user,
                                 startup=2 if mock else 40, watchdog=1 if mock else 30)
    # Recovery is armed before the coordinator changes either layout or service.
    # The official block must start without ExecStartPre taking the dock away.
    index = next(i for i, value in enumerate(command) if str(value).startswith("ExecStartPre="))
    del command[index - 1:index + 1]
    return command


def assess_measurement(result, seconds, *, static, input_fps=60):
    """Keep scheduling variation as evidence; reject incomplete/invalid measurements."""
    warnings, sampling_errors, errors = [], [], []
    iterations = seconds * input_fps
    expected_frames = 0 if static else iterations
    if result["frames_submitted"] != expected_frames:
        errors.append(f"replay submitted {result['frames_submitted']} images; expected {expected_frames}")
    if result["static_hold"] != static:
        errors.append("replay static/active mode did not match the requested workload")
    drift = abs(result["seconds"] - seconds)
    if drift >= .25:
        warnings.append(f"replay took {result['seconds']:.3f}s for the requested {seconds}s (duration drift {drift:.3f}s)")
    missed = result["input_deadline_misses"]
    missed_percent = missed / iterations * 100
    if missed > max(1, iterations * .01):
        warnings.append(f"{missed}/{iterations} replay deadlines missed ({missed_percent:.1f}%; warning threshold 1%)")
    if result["max_input_gap_ms"] >= 100:
        warnings.append(f"largest replay gap {result['max_input_gap_ms']:.1f}ms (warning threshold 100ms)")
    boundary_drift = abs(result["cpu_interval_seconds"] - result["seconds"])
    if boundary_drift >= .1:
        sampling_errors.append(f"CPU sampling interval differs from replay by {boundary_drift:.3f}s (limit 0.100s)")
    if result["power"]["max_gap_seconds"] >= 1:
        sampling_errors.append(f"power sample gap {result['power']['max_gap_seconds']:.3f}s (limit 1.000s)")
    return dict(input_pacing_ok=not warnings, input_pacing_warnings=warnings,
                input_deadline_miss_percent=missed_percent, sampling_ok=not sampling_errors,
                measurement_errors=errors + sampling_errors)


def vino_throughput(events, measured):
    frames = [event for event in events if event.get("event") == "frame"
              and measured["start_monotonic_s"] <= event["monotonic_ns"] / 1e9 <= measured["end_monotonic_s"]]
    changed = sum(event["changed_strips"] > 0 for event in frames)
    expected = measured["frames_submitted"]
    warning = None
    if expected and changed < expected * .95:
        warning = (f"Vino encoded {changed} changed updates for {expected} replay images "
                   "(below 95% of input count); resource savings are not qualified at equal output")
    return dict(bytes=sum(event["bytes"] for event in frames),
                presentations=sum(event["presentations"] for event in frames),
                changed_updates=changed, changed_updates_per_second=changed / measured["seconds"],
                input_rate_warning=warning,
                note="USB submissions and changed-update counts are throughput diagnostics, not monitor FPS or proof that every input image was displayed")


def progress_messages(path, offset):
    """Read every completed status line, retaining a partial line for the next poll."""
    messages = []
    if path.exists():
        with path.open("rb") as stream:
            stream.seek(offset)
            while True:
                line = stream.readline()
                if not line.endswith(b"\n"):
                    break
                messages.append(json.loads(line)["message"])
                offset = stream.tell()
    return messages, offset


class JsonReader:
    def __init__(self, process):
        self.process = process
        self.buffer = b""
        self.selector = selectors.DefaultSelector()
        self.selector.register(process.stdout, selectors.EVENT_READ)

    def close(self):
        self.selector.close()
        self.process.stdout.close()
        self.process.stdin.close()

    def event(self, expected, timeout, tick):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            tick()
            if b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                event = json.loads(line)
                if event.get("event") != expected:
                    raise RuntimeError(f"replay expected {expected}, got {event}")
                return event
            if self.selector.select(.1):
                chunk = os.read(self.process.stdout.fileno(), 8192)
                if not chunk:
                    raise RuntimeError(f"replay exited before {expected}")
                self.buffer += chunk
        raise RuntimeError(f"replay timed out before {expected}")


def stop_process(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=2)


class SessionGuard:
    """Supervise a session-user helper in the comparison's systemd cgroup."""
    def __init__(self, state, directory):
        self.directory = directory
        self.failure = None
        self.last_heartbeat = 0
        args, env = trial.session_invocation(
            state, ["/usr/bin/python3", HERE / "comparison-monitor.py", "guard"])
        with (directory / "screen-lock.stderr").open("w") as errors:
            self.process = subprocess.Popen(args, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                            stderr=errors, bufsize=0)
        self.reader = JsonReader(self.process)

    def record(self, event):
        with (self.directory / "screen-lock.jsonl").open("a") as stream:
            stream.write(json.dumps(event) + "\n")
        if event["event"] in ("ready", "heartbeat"):
            self.last_heartbeat = event["monotonic_s"]
        else:
            self.failure = event.get("reason", f"unexpected screen guard event: {event}")

    def __enter__(self):
        try:
            self.ready = self.reader.event("ready", 10, lambda: None)
            self.record(self.ready)
            if not self.ready.get("inhibited"):
                raise RuntimeError("screen-lock inhibitor was not acquired")
            self.check()
            return self
        except Exception as error:
            self.close()
            detail = (self.directory / "screen-lock.stderr").read_text().strip()
            raise RuntimeError(f"screen-lock guard startup failed: {detail or error}") from error

    def check(self):
        while self.reader.selector.select(0):
            chunk = os.read(self.process.stdout.fileno(), 8192)
            if not chunk:
                self.failure = self.failure or "screen-lock guard exited"
                break
            self.reader.buffer += chunk
        while b"\n" in self.reader.buffer:
            line, self.reader.buffer = self.reader.buffer.split(b"\n", 1)
            self.record(json.loads(line))
        if self.process.poll() is not None:
            self.failure = self.failure or "screen-lock guard exited"
        if time.monotonic() - self.last_heartbeat > 4:
            self.failure = self.failure or "screen-lock guard stopped responding"
        if self.failure:
            raise RuntimeError(self.failure)

    def confirm(self, tick):
        # Require a successful state check after the measured interval, not just
        # a stale heartbeat from before its end. Signal failures are latched.
        boundary = time.monotonic()
        while self.last_heartbeat <= boundary:
            tick()
            time.sleep(.05)
        return dict(inhibited=True, checked_through_monotonic_s=self.last_heartbeat,
                    events_file="screen-lock.jsonl")

    def close(self):
        if self.process.poll() is None:
            try:
                self.process.stdin.write(b"q")
                self.process.wait(timeout=3)
            except (BrokenPipeError, subprocess.TimeoutExpired):
                stop_process(self.process)
        self.reader.close()

    def __exit__(self, *args):
        self.close()


def background_assessment(result, settings):
    # Residual CPU includes interrupts and kernel work outside the tracked
    # processes. Attribution explains it imperfectly; never subtract it from W.
    background = (result["system_busy_cpu_cores"] - result["display_stack_cpu_cores"]
                  - sum(result["cpu_cores"].get(name, 0) for name in ("player", "sampler", "screen_guard")))
    limit = settings.get("max_background_cpu", .25)
    warnings = []
    if background > limit:
        warnings.append(f"outside CPU {background:.2f} cores exceeds {limit:.2f}")
    if background < -.05:
        warnings.append("system/process CPU accounting is inconsistent")
    attribution = result["process_attribution"]
    if attribution["unreadable"] or attribution["counter_resets"] or attribution["max_gap_seconds"] > 1.5:
        warnings.append("process attribution was incomplete or sampling was delayed")
    return dict(cpu_cores=background, warning_limit_cores=limit, warnings=warnings,
                top_processes=[r for r in attribution["processes"] if r["role"] == "background"][:10])


def power_comparison(rows, settings):
    """Retain/exclude complete matched repetitions using declared noise limits."""
    blocks = plan(settings["pairs"], settings.get("compare_evdi_rows"), settings.get("gpu_evdi_rows"))
    conditions = list(dict.fromkeys(b["condition"] for b in blocks))
    cases = []
    for case in CASES:
        pairs = []
        for repeat in range(settings["pairs"]):
            selected = [r for r in rows if r["case"] == case and r["repeat"] == repeat]
            reasons = []
            if sorted(r["condition"] for r in selected) != sorted(conditions):
                reasons.append("missing or duplicate condition")
            for row in selected:
                m = row["measurement"]
                if m.get("measurement_errors") or not m.get("input_pacing_ok"):
                    reasons.append(f"{row['condition']}: sampling/input pacing failed")
                if not m.get("screen_lock", {}).get("inhibited"):
                    reasons.append(f"{row['condition']}: screen-lock monitoring missing")
                reasons += [f"{row['condition']}: {warning}" for warning in m.get("background", {}).get(
                    "warnings", ["background monitoring missing"])]
                if row["backend"] == "gpu":
                    if "vino_usb" not in m or m["vino_usb"].get("input_rate_warning"):
                        reasons.append(f"{row['condition']}: Vino changed-update throughput unqualified")
            backgrounds = [r["measurement"].get("background", {}).get("cpu_cores") for r in selected]
            temperatures = [r["measurement"]["power"].get("temperature_start_c") for r in selected]
            for values, limit, label in (
                    (backgrounds, settings.get("max_background_delta", .10), "background CPU spread (cores)"),
                    (temperatures, settings.get("max_temperature_delta", 5), "starting AMD temperature spread (C)")):
                if not values or any(value is None for value in values):
                    reasons.append(f"{label}: readings missing")
                elif max(values) - min(values) > limit:
                    reasons.append(f"{label} {max(values) - min(values):.3f} exceeds {limit:.3f}")
            pairs.append(dict(repeat=repeat, included=not reasons, reasons=reasons,
                              samples=[dict(block=r["block"], condition=r["condition"]) for r in selected]))
        retained = [p["repeat"] for p in pairs if p["included"]]
        comparable = [r for r in rows if r["case"] == case and r["repeat"] in retained]
        cases.append(dict(case=case, repetitions=pairs, retained_pairs=len(retained),
                          repeated_comparison_available=len(retained) >= 2,
                          matched_observations=summarize(comparable)))
    return dict(criteria=dict(max_background_cpu_cores=settings.get("max_background_cpu", .25),
                              max_background_spread_cores=settings.get("max_background_delta", .10),
                              max_starting_temperature_spread_c=settings.get("max_temperature_delta", 5),
                              minimum_matched_repetitions=2),
                cases=cases, all_cases_repeated=all(c["repeated_comparison_available"] for c in cases),
                note="Predeclared diagnostic screens, not proof of equal output, thermal equilibrium or isolated "
                     "driver power. All observations remain in the raw summary. No background watts are subtracted.")


class Coordinator:
    def __init__(self, directory):
        self.directory = directory
        self.state = json.loads((directory / "state.json").read_text())
        self.settings = self.state["settings"]
        self.gpu = None
        self.channel = None
        self.gpu_ready = False
        self.gpu_watchdog = 0
        self.last_notify = 0
        self.guard = None
        self.data = dict(schema=4, complete=False, started_unix=time.time(), runs=[],
                         blocks=plan(self.settings["pairs"], self.settings.get("compare_evdi_rows"),
                                     self.settings.get("gpu_evdi_rows")),
                         method="same Dell 1920x1080/60, laptop primary, other outputs off; both drivers capture and send USB",
                         vulkan_validation=False, monitor_fps_verified=False, quality_equivalence_verified=False,
                         sensor=self.state["sensor"], settings=self.settings,
                         external_power_inputs=self.state["power_source"],
                         limitations=["Input submissions and Vino USB presentations do not measure delivered monitor FPS or latency",
                                      "Visual smoothness and quality need user observation; codecs and adaptive quality may differ",
                                      "Synthetic full-window damage; not browser decoding, window dragging, or arbitrary desktop workloads",
                                      "AMD reported PPT is not wall or whole-laptop power; normal background apps and automatic clocks remain active",
                                      "Fan speed and temperatures respond slowly; short samples do not establish steady-state fan noise",
                                      "Periodic process sampling misses short-lived work; background CPU cannot be subtracted from power"],
                         inputs=self.state["inputs"], official_sha256=self.state["official_sha256"],
                         harness_sha256=self.state["harness_sha256"],
                         versions=self.state["versions"], kernel=self.state["kernel"],
                         evdi_module=self.state["evdi_module"], original_outputs=self.state["outputs"])

    def save(self):
        trial.save(self.directory / "measurements.json", self.data)

    def status(self, message):
        print(message, flush=True)
        event = dict(message=message, unix=time.time())
        with (self.directory / "progress.jsonl").open("a") as stream:
            stream.write(json.dumps(event) + "\n")
        trial.save(self.directory / "progress.json", event)

    def tick(self):
        now = time.monotonic()
        if self.guard is not None:
            self.guard.check()
        if self.gpu:
            if self.gpu.poll() is not None:
                raise RuntimeError(f"Vino exited unexpectedly ({self.gpu.returncode})")
            while True:
                try:
                    packet = self.channel.recv(4096)
                except BlockingIOError:
                    break
                if b"READY=1" in packet:
                    self.gpu_ready = True
                if b"WATCHDOG=1" in packet:
                    self.gpu_watchdog = now
            if self.gpu_ready and now - self.gpu_watchdog > 5:
                raise RuntimeError("Vino capture/USB watchdog stopped")
        if now - self.last_notify >= 1:
            trial.notify("WATCHDOG=1")
            self.last_notify = now

    def pause(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.tick()
            time.sleep(min(.1, max(0, end - time.monotonic())))

    def stop_gpu(self):
        if not self.gpu:
            return
        process, self.gpu = self.gpu, None
        stop_process(process)
        self.channel.close()
        self.channel = None
        if process.returncode != 0:
            raise RuntimeError(f"Vino did not shut down cleanly ({process.returncode})")

    def official(self):
        self.stop_gpu()
        if self.state.get("evdi_copy"):
            # Keep the official baseline on the original row-copy path. Record
            # reset intent before writing; the independent recovery also resets it.
            self.state["evdi_copy"]["rows"] = self.state["evdi_copy"]["previous"]
            self.state["evdi_copy_restore_pending"] = True
            trial.save(self.directory / "state.json", self.state)
            trial.set_evdi_copy_rows(self.state["evdi_copy"], self.state["evdi_copy"]["previous"])
        verify_evdi(self.state)
        # Write ahead, including the very first official-only layout change.
        self.state["takeover"] = True
        trial.save(self.directory / "state.json", self.state)
        mask = Path("/run/systemd/system") / trial.SERVICE
        if mask.is_symlink() and os.readlink(mask) == "/dev/null" and self.state["mask_added"]:
            trial.command(["systemctl", "unmask", "--runtime", trial.SERVICE])
        elif mask.exists() or mask.is_symlink():
            raise RuntimeError("unexpected service override")
        trial.command(["systemctl", "start", trial.SERVICE], timeout=15)
        deadline = time.monotonic() + 18
        while True:
            self.tick()
            try:
                _, arguments = target_layout(self.state, trial.outputs(self.state))
                trial.desktop_command(self.state, arguments)
                verify_layout(self.state)
                return driver_pid()
            except RuntimeError:
                if time.monotonic() >= deadline:
                    raise
                self.pause(.5)

    def start_gpu(self, block_dir, evdi_copy_rows=None):
        worker_arguments = gpu_worker_arguments(self.settings)
        self.stop_gpu()
        verify_evdi(self.state)
        if self.state.get("evdi_copy"):
            if evdi_copy_rows not in (0, 32, 128):
                raise RuntimeError("A prepared EVDI comparison requires an explicit supported batch size")
            self.state["evdi_copy"]["rows"] = evdi_copy_rows
            trial.save(self.directory / "state.json", self.state)
        trial.prepare(self.directory)
        self.state = json.loads((self.directory / "state.json").read_text())
        trial.save(block_dir / "state.json", self.state)
        os.chmod(block_dir / "state.json", 0o600)
        shutil.copyfile(self.directory / "helper.py", block_dir / "helper.py")
        os.chmod(block_dir / "helper.py", 0o700)
        self.channel = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.channel.bind(str(block_dir / "notify.sock"))
        self.channel.setblocking(False)
        self.gpu_ready = False
        duration = int(3 * (self.settings["warmup"] + self.settings["seconds"] + 10) + self.settings["settle"] + 50)
        arguments = [self.directory / "payload/worker", *worker_arguments, "--fps", "60",
                     "--duration", duration, "--log", block_dir / "events.jsonl"]
        env = {**os.environ, "NOTIFY_SOCKET": str(block_dir / "notify.sock"), "VINO_TRIAL_DIR": str(block_dir), "VINO_SUPERVISED": "1"}
        with (block_dir / "worker.stderr").open("w") as errors:
            self.gpu = subprocess.Popen(list(map(str, arguments)), env=env, stdin=subprocess.DEVNULL, stdout=errors, stderr=errors)
        deadline = time.monotonic() + 35
        while not self.gpu_ready:
            self.pause(.1)
            if time.monotonic() > deadline:
                raise RuntimeError("Vino did not produce its first frame before the startup deadline")
        verify_layout(self.state)
        return self.gpu.pid

    def counters(self, pids, backend):
        cgroups = monitor.cgroup_snapshot()
        begin = time.monotonic()
        result = dict(processes={name: process_counter(pid) for name, pid in pids.items()}, system_cpu_seconds=system_cpu())
        if backend == "official":
            result["official_service_cpu_seconds"] = service_cpu()
        result["monotonic_s"] = (begin + time.monotonic()) / 2
        result["cgroups"] = cgroups
        return result

    def replay(self, block_dir, block, case, pid):
        power_before = verify_environment(self.state)
        if block["backend"] == "official" and driver_pid() != pid:
            raise RuntimeError("official driver restarted")
        label = f"{block_dir.name}-{case}-{block.get('condition', block['backend'])}"
        self.status(f"{label}: {self.settings['warmup']}s warmup, {self.settings['seconds']}s measurement")
        arguments = [self.directory / "payload/live-replay", self.directory / f"payload/{case}.bgrx",
                     "1920", "1080", str(self.settings.get("input_fps", 60)),
                     str(self.settings["warmup"]), str(self.settings["seconds"]), self.state["panel"]["mode"].split("x")[0],
                     "0", str(int(case == "idle")), label, "checked-layout"]
        invocation, env = trial.session_invocation(self.state, list(map(str, arguments)))
        with (block_dir / f"{case}.stderr").open("w") as errors:
            process = subprocess.Popen(invocation, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, bufsize=0)
        reader = JsonReader(process)
        samples, process_samples = [], []
        try:
            ready = reader.event("ready", 10, self.tick)
            pids = {name: counter["pid"] for name, counter in self.state["processes"].items()}
            pids.update(driver=pid, player=ready["pid"], sampler=os.getpid(),
                        screen_guard=self.guard.ready["pid"])
            process.stdin.write(b"g")
            reader.event("warm", self.settings["warmup"] + 8, self.tick)
            process_samples.append(monitor.process_snapshot())
            samples.append(power_sample(self.state["sensor"]))
            before = self.counters(pids, block["backend"])
            process.stdin.write(b"g")

            def sample():
                self.tick()
                if time.monotonic() - samples[-1]["monotonic_s"] >= .2:
                    samples.append(power_sample(self.state["sensor"]))
                if time.monotonic() - process_samples[-1]["monotonic_s"] >= 1:
                    process_samples.append(monitor.process_snapshot())

            result = reader.event("done", self.settings["seconds"] + 8, sample)
            after = self.counters(pids, block["backend"])
            samples.append(power_sample(self.state["sensor"]))
            process_samples.append(monitor.process_snapshot())
            elapsed = after["monotonic_s"] - before["monotonic_s"]
            result.update(cpu_interval_seconds=elapsed,
                          evdi=verify_evdi(self.state),
                          requested_input_fps=self.settings.get("input_fps", 60),
                          capture_storage=self.settings.get("capture", "copied") if block["backend"] == "gpu" else None,
                          cursor_mode=("client" if self.settings.get("capture") == "direct" else "evdi")
                          if block["backend"] == "gpu" else None,
                          external_power_before=power_before,
                          cpu_cores={name: cpu_delta(before["processes"][name], after["processes"][name], elapsed) for name in pids},
                          system_busy_cpu_cores=(after["system_cpu_seconds"] - before["system_cpu_seconds"]) / elapsed,
                          power=power_summary(samples), pixel_checks_passed=ready["pixel_checks_passed"],
                          counters_before=before, counters_after=after)
            result["process_attribution"] = monitor.process_attribution(
                process_samples, before["processes"], block["backend"])
            result["cgroup_attribution"] = monitor.cgroup_attribution(before["cgroups"], after["cgroups"])
            trial.save(block_dir / f"{case}-processes.json", process_samples)
            if block["backend"] == "official":
                result["official_service_cpu_cores"] = (after["official_service_cpu_seconds"] - before["official_service_cpu_seconds"]) / elapsed
                if result["official_service_cpu_cores"] < 0:
                    raise RuntimeError("official service CPU counter reset")
            result["driver_cpu_cores"] = result.get("official_service_cpu_cores", result["cpu_cores"]["driver"])
            result["display_stack_cpu_cores"] = result["driver_cpu_cores"] + sum(result["cpu_cores"][name] for name in ("Xorg", "kwin_x11"))
            result["background"] = background_assessment(result, self.settings)
            process.stdin.write(b"g")
            if process.wait(timeout=5) != 0:
                raise RuntimeError("replay did not finish cleanly")
            result["screen_lock"] = self.guard.confirm(self.tick)
            self.check_replay_environment(block_dir, block, case, pid, result, samples)
            self.record_measurement(block_dir, block, case, result, samples)
        except Exception as error:
            # Even a mid-replay lock/failure leaves diagnostics. This interrupted
            # interval is never silently added to the completed sample summary.
            trial.save(block_dir / f"{case}-interrupted.json", dict(
                error=str(error), monotonic_s=time.monotonic(), valid=False,
                screen_lock_events="screen-lock.jsonl"))
            trial.save(block_dir / f"{case}-power.json", samples)
            trial.save(block_dir / f"{case}-processes.json", process_samples)
            raise
        finally:
            stop_process(process)
            reader.close()

    def check_replay_environment(self, block_dir, block, case, pid, result, samples):
        # Preserve completed measurements even if a later environment check fails.
        # The ordinary report only includes rows that passed these checks.
        path = block_dir / f"{case}-raw-measurement.json"
        raw = dict(environment_checked=False, measurement=result)
        trial.save(path, raw)
        trial.save(block_dir / f"{case}-power.json", samples)
        try:
            result["external_power_after"] = verify_environment(self.state)
            if block["backend"] == "official" and driver_pid() != pid:
                raise RuntimeError("official service restarted during replay")
        except Exception as error:
            raw["environment_error"] = str(error)
            trial.save(path, raw)
            raise RuntimeError(f"{block_dir.name}-{case}-{block.get('condition', block['backend'])}: {error}") from error
        raw["environment_checked"] = True
        trial.save(path, raw)

    def record_measurement(self, block_dir, block, case, result, samples):
        result.update(assess_measurement(result, self.settings["seconds"], static=case == "idle",
                                         input_fps=self.settings.get("input_fps", 60)))
        trial.save(block_dir / f"{case}-power.json", samples)
        self.data["runs"].append(dict(block=block_dir.name, **block, case=case, measurement=result))
        self.save()
        label = f"{block_dir.name}-{case}-{block.get('condition', block['backend'])}"
        if result["measurement_errors"]:
            raise RuntimeError(f"{label}: invalid measurement: {'; '.join(result['measurement_errors'])}")
        self.status(f"{label}: driver {result['driver_cpu_cores'] * 100:.1f}% of one CPU core; AMD PPT {result['power']['mean_watts']:.2f} W")
        for warning in result["input_pacing_warnings"]:
            self.status(f"{label}: TIMING WARNING: {warning}; continuing and retaining the timing data")
        for warning in result.get("background", {}).get("warnings", []):
            self.status(f"{label}: BACKGROUND WARNING: {warning}; retaining raw data, excluding this power pair")
        if result.get("background", {}).get("top_processes"):
            top = result["background"]["top_processes"][:3]
            self.status(f"{label}: outside CPU leaders: " + ", ".join(
                f"{r['name']} (PID {r['pid']}) {100 * r['cpu_cores']:.1f}% of one core" for r in top))

    def run(self):
        self.save()
        trial.notify("READY=1\nWATCHDOG=1")
        with SessionGuard(self.state, self.directory) as guard:
            self.guard = guard
            self.data["screen_lock_guard"] = guard.ready
            self.save()
            try:
                self.run_blocks()
            finally:
                self.guard = None

    def run_blocks(self):
        for index, block in enumerate(self.data["blocks"]):
            block_dir = self.directory / f"block-{index:02d}"
            block_dir.mkdir()
            self.status(f"Block {index + 1}/{len(self.data['blocks'])}: switching to {block['condition']}")
            pid = self.official() if block["backend"] == "official" else self.start_gpu(block_dir, block["evdi_copy_rows"])
            self.pause(self.settings["settle"])
            for case in block["cases"]:
                self.replay(block_dir, block, case, pid)
            self.stop_gpu()
            if block["backend"] == "gpu":
                events = [json.loads(line) for line in (block_dir / "events.jsonl").read_text().splitlines()]
                if not events or events[-1].get("event") != "exit" or not events[-1].get("ok"):
                    raise RuntimeError("Vino reported a failed trial")
                for row in self.data["runs"]:
                    if row["block"] == block_dir.name:
                        measured = row["measurement"]
                        measured["vino_usb"] = vino_throughput(events, measured)
                        if row["case"] != "idle" and not measured["vino_usb"]["presentations"]:
                            raise RuntimeError("Vino submitted no USB presentations during an active sample")
                        if measured["vino_usb"]["input_rate_warning"]:
                            self.status(f"{block_dir.name}-{row['case']}-{block['condition']}: "
                                        f"THROUGHPUT WARNING: {measured['vino_usb']['input_rate_warning']}")
                # Every GPU takeover starts from the same working official-driver state,
                # including the adjacent GPU blocks in the ABBA order.
                self.official()
        warning_runs = sum(not row["measurement"]["input_pacing_ok"] for row in self.data["runs"])
        throughput_warnings = sum(bool(row["measurement"].get("vino_usb", {}).get("input_rate_warning"))
                                  for row in self.data["runs"])
        self.data.update(complete=True, finished_unix=time.time(), summary=summarize(self.data["runs"]),
                         input_pacing_ok=not warning_runs, input_pacing_warning_runs=warning_runs,
                         vino_throughput_warning_runs=throughput_warnings,
                         power_comparison=power_comparison(self.data["runs"], self.settings))
        self.save()
        warning = f" with timing warnings in {warning_runs} samples" if warning_runs else ""
        self.status(f"Measurements complete{warning}; restoring your original display layout")
        for case in self.data["power_comparison"]["cases"]:
            self.status(f"{case['case']}: {case['retained_pairs']}/{self.settings['pairs']} matched power pairs; " +
                        ("repeated observations available" if case["repeated_comparison_available"]
                         else "power comparison inconclusive"))
            for pair in case["repetitions"]:
                if not pair["included"]:
                    self.status(f"{case['case']} pair {pair['repeat'] + 1} excluded: " + "; ".join(pair["reasons"]))


def summarize(rows):
    summary = []
    conditions = list(dict.fromkeys(r.get("condition", r["backend"]) for r in rows))
    for case in CASES:
        for condition in conditions:
            matching = [r for r in rows if r["case"] == case and r.get("condition", r["backend"]) == condition]
            measurements = [r["measurement"] for r in matching]
            if not measurements:
                continue
            values = dict(driver_cpu_cores=[m["driver_cpu_cores"] for m in measurements],
                          display_stack_cpu_cores=[m["display_stack_cpu_cores"] for m in measurements],
                          system_busy_cpu_cores=[m["system_busy_cpu_cores"] for m in measurements],
                          amd_reported_watts=[m["power"]["mean_watts"] for m in measurements],
                          input_frames_per_second=[m["input_frames_per_second"] for m in measurements],
                          input_deadline_miss_percent=[m["input_deadline_miss_percent"] for m in measurements],
                          x_submission_p99_ms=[m["x_submission_p99_ms"] for m in measurements],
                          max_input_gap_ms=[m["max_input_gap_ms"] for m in measurements])
            if all("vino_usb" in m for m in measurements):
                values["vino_changed_updates_per_second"] = [m["vino_usb"]["changed_updates_per_second"] for m in measurements]
                values["vino_usb_presentations_per_second"] = [m["vino_usb"]["presentations"] / m["seconds"] for m in measurements]
            if all("background" in m for m in measurements):
                values["background_cpu_cores"] = [m["background"]["cpu_cores"] for m in measurements]
            for category in ("activity", "thermal"):
                keys = {key for m in measurements for key in m["power"].get(category, {})}
                for key in sorted(keys):
                    readings = [m["power"].get(category, {}).get(key, {}) for m in measurements]
                    # Missing sensor coverage remains in raw records, never
                    # converted to zero or silently averaged over fewer repeats.
                    if all(r.get("coverage_fraction") == 1 and r.get("mean") is not None for r in readings):
                        values[f"{category}/{key}"] = [r["mean"] for r in readings]
            summary.append(dict(case=case, backend=matching[0]["backend"], condition=condition,
                                evdi_copy_rows=matching[0].get("evdi_copy_rows"), repetitions=len(measurements),
                                vino_throughput_warning_runs=sum(bool(m.get("vino_usb", {}).get("input_rate_warning")) for m in measurements),
                                input_pacing_ok=all(m["input_pacing_ok"] for m in measurements),
                                input_pacing_warning_runs=sum(not m["input_pacing_ok"] for m in measurements),
                                metrics={k: dict(mean=statistics.mean(v), min=min(v), max=max(v)) for k, v in values.items()}))
    return summary


def launch(args):
    if os.geteuid() != 0:
        raise RuntimeError("run needs sudo with DISPLAY and XAUTHORITY preserved")
    lock = open("/run/lock/vino-live-trial.lock", "w")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    if trial.command(["systemctl", "list-units", "--all", "--no-legend", "vino-trial-*", "vino-recover-*", "vino-module-*"], check=False).stdout.strip():
        raise RuntimeError("an earlier trial/recovery is still active")
    state = preflight(args)
    if os.statvfs("/var/tmp").f_flag & os.ST_NOEXEC:
        raise RuntimeError("/var/tmp is noexec; no changes made")
    directory = Path(tempfile.mkdtemp(prefix="vino-compare-", dir="/var/tmp"))
    os.chmod(directory, 0o755)
    payload = directory / "payload"
    payload.mkdir()
    shutil.copyfile(HERE / "live-trial.py", directory / "helper.py")
    shutil.copyfile(__file__, directory / "comparison.py")
    shutil.copyfile(HERE / "comparison-monitor.py", directory / "comparison-monitor.py")
    shutil.copyfile(args.binary, payload / "worker")
    os.chmod(directory / "helper.py", 0o700)
    os.chmod(directory / "comparison.py", 0o700)
    # Readable by the desktop user, but only root can modify the staged helper.
    os.chmod(directory / "comparison-monitor.py", 0o644)
    os.chmod(payload / "worker", 0o700)
    for name in ("live-replay", *(f"{case}.bgrx" for case in CASES)):
        checksum = state["inputs"]["files"][name]
        shutil.copyfile(args.inputs / name, payload / name)
        os.chmod(payload / name, 0o755 if name == "live-replay" else 0o644)
        if digest(payload / name) != checksum:
            raise RuntimeError("staged input checksum mismatch")
    if digest(payload / "worker") != state["inputs"]["gpu_binary_sha256"]:
        raise RuntimeError("staged GPU binary checksum mismatch")
    trial.save(directory / "state.json", state)
    os.chmod(directory / "state.json", 0o600)
    print(f"Comparison logs: {directory}\nDell only, BenQ temporarily off; Ctrl+C restores the original layout", flush=True)
    bound = runtime_bound(state["settings"])
    interrupted = False
    progress_offset = 0
    try:
        trial.command(unit_command(directory, bound))
        end = time.monotonic() + bound + 60
        while not (directory / "recovery.json").exists():
            if time.monotonic() > end:
                raise RuntimeError("comparison exceeded its recovery deadline")
            messages, progress_offset = progress_messages(directory / "progress.jsonl", progress_offset)
            for message in messages:
                print(message, flush=True)
            time.sleep(.25)
    except KeyboardInterrupt:
        interrupted = True
        trial.command(["systemctl", "stop", "--no-block", f"vino-trial-{directory.name}"])
    outcome = trial.wait_for_recovery(directory, 50)
    messages, progress_offset = progress_messages(directory / "progress.jsonl", progress_offset)
    for message in messages:
        print(message, flush=True)
    print(json.dumps(outcome, indent=2), flush=True)
    if not outcome["restored"]:
        raise RuntimeError(f"restoration needs attention; inspect {directory}/recovery.json")
    report = json.loads((directory / "measurements.json").read_text()) if (directory / "measurements.json").exists() else {}
    if interrupted:
        print("Stopped and restored; partial results are not a completed comparison")
    elif outcome["service_result"] != "success" or not report.get("complete"):
        raise RuntimeError(f"comparison incomplete: {report.get('error', outcome['service_result'])}; displays restored")
    else:
        print("Observed averages, including flagged samples (CPU is % of one core):")
        print(f"{'Workload':<10} {'Condition':<20} {'Driver CPU':>11} {'AMD PPT':>10} {'Outside CPU':>12}")
        for row in report["summary"]:
            metrics = row["metrics"]
            print(f"{row['case']:<10} {row['condition']:<20} "
                  f"{metrics['driver_cpu_cores']['mean'] * 100:>10.1f}% "
                  f"{metrics['amd_reported_watts']['mean']:>8.2f} W "
                  f"{metrics['background_cpu_cores']['mean'] * 100:>11.1f}%")
        if not report["input_pacing_ok"]:
            print(f"Timing warnings in {report['input_pacing_warning_runs']} samples: compare pacing metrics before interpreting CPU/power as equal-throughput savings")
        if report.get("vino_throughput_warning_runs"):
            print(f"Vino fell below the changed-update count threshold in {report['vino_throughput_warning_runs']} samples; lower CPU/power is not qualified at equal output")
        if not report["power_comparison"]["all_cases_repeated"]:
            print("POWER COMPARISON INCONCLUSIVE for one or more cases; see power_comparison for matched pairs and exclusion reasons")
        print(f"Results: {directory}/measurements.json\nPhysical smoothness/quality still require your visual check")


def self_test():
    for case in ("normal", "early-exit", "startup-hang", "crash", "hang", "runtime-limit", "interrupt"):
        directory = Path(tempfile.mkdtemp(prefix="vino-compare-test-"))
        shutil.copyfile(__file__, directory / "comparison.py")
        shutil.copyfile(HERE / "live-trial.py", directory / "helper.py")
        shutil.copyfile(HERE / "comparison-monitor.py", directory / "comparison-monitor.py")
        trial.save(directory / "state.json", dict(mock=True, takeover=False, recovery_delay=3))
        trial.command(unit_command(directory, 3, user=True, mock=case))
        outcome = trial.wait_for_recovery(directory, 15)
        if not outcome["restored"] or ((outcome["service_result"] == "success") != (case in ("normal", "interrupt"))):
            raise RuntimeError(f"supervision failed: {case}: {outcome}")
        print(json.dumps(dict(case=case, **outcome)), flush=True)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "worker":
        coordinator = Coordinator(Path(sys.argv[2]))
        try:
            coordinator.run()
        except Exception as error:
            coordinator.data["error"] = str(error)
            coordinator.save()
            raise
        return
    if len(sys.argv) > 1 and sys.argv[1] == "mock-worker":
        trial.mock_worker(sys.argv[2])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("prepare", "preflight", "run", "self-test"))
    parser.add_argument("--inputs", type=Path, default=ROOT / "gpu-artifacts/live-comparison-inputs")
    parser.add_argument("--binary", type=Path, default=ROOT / "revdi/target/release/vino-gpu-live")
    parser.add_argument("--exporter", type=Path, default=ROOT / "revdi/target/release/vino-gpu")
    parser.add_argument("--pairs", type=int, default=2)
    parser.add_argument("--warmup", type=int, default=15)
    parser.add_argument("--seconds", type=int, default=30)
    parser.add_argument("--settle", type=int, default=15)
    parser.add_argument("--input-fps", type=int, default=60,
                        help="synthetic image submissions per second; both display modes and the Vino cap remain 60 Hz")
    parser.add_argument("--capture", choices=("copied", "direct"), default="copied",
                        help="Vino capture storage; direct also selects separate client cursor composition")
    evdi = parser.add_mutually_exclusive_group()
    evdi.add_argument("--compare-evdi-rows", type=int, choices=(32, 128),
                        help="compare official/normal-copy, Vino/normal-copy and Vino/batched-copy on the prepared EVDI candidate")
    evdi.add_argument("--gpu-evdi-rows", type=int, choices=(32, 128),
                      help="two-condition comparison: official/normal-copy versus Vino/batched-copy")
    parser.add_argument("--max-background-cpu", type=float, default=.25,
                        help="exclude matched power pairs if outside CPU exceeds this many cores (default .25)")
    parser.add_argument("--max-background-delta", type=float, default=.10,
                        help="maximum outside-CPU spread across a matched power pair (default .10 cores)")
    parser.add_argument("--max-temperature-delta", type=float, default=5,
                        help="maximum starting AMD-temperature spread across a power pair (default 5 C)")
    parser.add_argument("--dock", default="8-1.2")
    parser.add_argument("--head", type=int, choices=(0, 1), default=0)
    parser.add_argument("--card", type=int, default=0)
    parser.add_argument("--device", type=int, default=0)
    args = parser.parse_args()
    try:
        gpu_worker_arguments(vars(args))
    except ValueError as error:
        parser.error(str(error))
    if not (1 <= args.pairs <= 3 and 1 <= args.seconds <= 60 and 0 <= args.warmup <= 30 and 0 <= args.settle <= 30):
        parser.error("pairs 1..3, seconds 1..60, warmup/settle 0..30")
    if not 1 <= args.input_fps <= 60:
        parser.error("input-fps must be 1..60")
    if not (0 <= args.max_background_cpu <= 4 and 0 <= args.max_background_delta <= 4
            and 0 <= args.max_temperature_delta <= 30):
        parser.error("background CPU/delta must be 0..4 cores; temperature delta 0..30 C")
    args.evdi_copy_rows = 0 if args.compare_evdi_rows is not None or args.gpu_evdi_rows is not None else None
    args.trace_motion = args.no_compositor = False
    args.duration = runtime_bound(vars(args))
    if args.action == "prepare":
        prepare_inputs(args)
    elif args.action == "preflight":
        state = preflight(args)
        print(json.dumps(dict(target=state["target"]["name"], sensor=state["sensor"],
                              blocks=plan(args.pairs, args.compare_evdi_rows, args.gpu_evdi_rows), input_fps=args.input_fps,
                              capture_storage=args.capture, cursor_mode="client" if args.capture == "direct" else "evdi",
                              screen_lock_check=state["screen_lock_check"],
                              evdi_module=state["evdi_module"],
                              external_power_inputs=state["power_source"],
                              runtime_limit_seconds=args.duration, hardware_changed=False), indent=2))
    elif args.action == "self-test":
        self_test()
    else:
        launch(args)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"compare-live: {error}", file=sys.stderr)
        sys.exit(1)
