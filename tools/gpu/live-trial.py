#!/usr/bin/env python3
"""A bounded Ella trial with systemd-owned recovery. Python standard library only."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

SERVICE = "displaylink-driver.service"
ROOT = Path(__file__).resolve().parents[2]
PROFILE_HZ = 199
EVDI_BATCH_PARAMETER = Path('/sys/module/evdi/parameters/copy_batch_rows')
EVDI_MODULE_STATE = Path('/run/vino-evdi-module-trial.json')
EVDI_VERSION_PATH = Path('/sys/module/evdi/srcversion')
EVDI_SHARED_ABI = Path('/sys/module/evdi/parameters/shared_capture_abi')


def evdi_copy_configuration(rows):
    if rows is None:
        return None
    active = json.loads(EVDI_MODULE_STATE.read_text())
    state = json.loads((Path(active['directory']) / 'state.json').read_text())
    current = EVDI_VERSION_PATH.read_text().strip()
    if state['kernel'] != os.uname().release or current != state['versions']['candidate']:
        raise RuntimeError('The prepared EVDI candidate is not loaded for this kernel')
    if state['goal'] != 'candidate' or state['phase'] != 'complete':
        raise RuntimeError('The EVDI module switch has not completed successfully')
    before = int(EVDI_BATCH_PARAMETER.read_text())
    if before != 0:
        raise RuntimeError('Start with EVDI copy batching disabled outside the trial')
    return dict(rows=rows, previous=before, srcversion=current)


def set_evdi_copy_rows(configuration, rows):
    if EVDI_VERSION_PATH.read_text().strip() != configuration['srcversion']:
        raise RuntimeError('EVDI module changed during the trial; refusing to write its parameter')
    EVDI_BATCH_PARAMETER.write_text(f'{rows}\n')
    if int(EVDI_BATCH_PARAMETER.read_text()) != rows:
        raise RuntimeError('EVDI copy batch parameter did not retain the requested value')


def command(args, *, check=True, timeout=15, env=None):
    result = subprocess.run([str(a) for a in args], capture_output=True, text=True,
                            timeout=timeout, env=env)
    if check and result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr.strip() or result.stdout.strip()}")
    return result


def save(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def notify(message):
    address = os.environ.get("NOTIFY_SOCKET")
    if address:
        if address.startswith("@"):
            address = "\0" + address[1:]
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as channel:
            channel.sendto(message.encode(), address)


def parse_outputs(text):
    outputs = []
    current = None
    edid = False
    for line in text.splitlines():
        match = re.match(r"^(\S+) (connected|disconnected)(?: (primary))?(?: (\d+x\d+)([+-]\d+)([+-]\d+))?", line)
        if match:
            name, status, primary, mode, x, y = match.groups()
            current = dict(name=name, connected=status == "connected", primary=bool(primary),
                           mode=mode, x=int(x or 0), y=int(y or 0), rate=None, edid="")
            if mode and re.search(r"\d+x\d+[+-]\d+[+-]\d+ (left|right|inverted|x axis|y axis)", line):
                raise RuntimeError("first trial supports unrotated, unreflected outputs only")
            outputs.append(current)
            edid = False
        elif current is not None:
            if line.strip() == "EDID:":
                edid = True
            elif edid and re.fullmatch(r"[0-9a-fA-F]{32}", line.strip()):
                current["edid"] += line.strip().lower()
            else:
                edid = False
                if current["mode"] and line.lstrip().startswith(current["mode"] + " "):
                    rate = re.search(r"(\d+(?:\.\d+)?)\*", line)
                    if rate:
                        current["rate"] = rate[1]
    return outputs


def session_invocation(state, arguments):
    env = {"PATH": "/usr/bin:/bin", "DISPLAY": state["display"],
           "XAUTHORITY": state["xauthority"], "XDG_RUNTIME_DIR": state["runtime_dir"],
           "DBUS_SESSION_BUS_ADDRESS": f"unix:path={state['runtime_dir']}/bus"}
    args = list(arguments)
    if os.geteuid() == 0:
        args = ["/usr/sbin/runuser", "-u", state["user"], "--", "/usr/bin/env",
                *(f"{k}={v}" for k, v in env.items()), *args]
        return args, None
    return args, env


def session_command(state, arguments, *, timeout=10):
    args, env = session_invocation(state, arguments)
    return command(args, env=env, timeout=timeout)


def desktop_command(state, arguments):
    return session_command(state, ["/usr/bin/xrandr", *arguments])


def compositor_command(state, method):
    return session_command(state, ["/usr/bin/qdbus", "org.kde.KWin", "/Compositor",
                                  "org.kde.kwin.Compositing." + method], timeout=3).stdout.strip()


def compositor_active(state):
    result = compositor_command(state, "active")
    if result not in ("true", "false"):
        raise RuntimeError("KWin did not report its compositor state")
    return result == "true"


def set_compositor_active(state, active):
    compositor_command(state, "resume" if active else "suspend")
    deadline = time.monotonic() + 2
    while compositor_active(state) != active:
        if time.monotonic() >= deadline:
            raise RuntimeError("KWin did not reach the requested compositor state")
        time.sleep(0.1)


def outputs(state):
    return parse_outputs(desktop_command(state, ["--props"]).stdout)


def match_output(saved, current):
    candidates = [o for o in current if o["connected"] and o["edid"] == saved["edid"]]
    if len(candidates) == 1:
        return candidates[0]
    exact = [o for o in candidates if o["name"] == saved["name"]]
    if len(exact) == 1:
        return exact[0]
    raise RuntimeError(f"cannot uniquely locate saved monitor {saved['name']}")


def layout_arguments(saved, current):
    # --noprimary applies to the whole screen, unlike the per-output --primary.
    args = [] if any(o["primary"] and o["mode"] for o in saved) else ["--noprimary"]
    restored = set()
    for old in saved:
        if not old["mode"]:
            continue
        new = match_output(old, current)
        restored.add(new["name"])
        args += ["--output", new["name"], "--mode", old["mode"], "--pos", f"{old['x']}x{old['y']}",
                 "--rotate", "normal", "--reflect", "normal"]
        if old["primary"]:
            args.append("--primary")
        if old["rate"]:
            args += ["--rate", old["rate"]]
    for output in current:
        if output["name"] not in restored:
            args += ["--output", output["name"], "--off"]
    return args


def interface_bindings(dock):
    result = {}
    for path in sorted(Path("/sys/bus/usb/devices").glob(f"{dock}:1.*")):
        number = int((path / "bInterfaceNumber").read_text(), 16)
        if number >= 2:
            driver = path / "driver"
            result[str(number)] = driver.resolve().name if driver.is_symlink() else None
    return result


def snapshot(args):
    uid = int(os.environ.get("SUDO_UID", os.getuid()))
    if uid == 0:
        raise RuntimeError("run from the desktop account using sudo, not a root login")
    account = pwd.getpwuid(uid)
    state = dict(user=account.pw_name, uid=uid, gid=account.pw_gid,
                 display=os.environ.get("DISPLAY", ""), xauthority=os.environ.get("XAUTHORITY", ""),
                 runtime_dir=f"/run/user/{uid}", dock=args.dock, mask_added=False, takeover=False)
    if not state["display"] or not Path(state["xauthority"]).is_file():
        raise RuntimeError("DISPLAY/XAUTHORITY missing; preserve them when invoking sudo")
    state["outputs"] = outputs(state)
    panels = [o for o in state["outputs"] if o["mode"] and o["name"].startswith(("eDP", "LVDS"))]
    if len(panels) != 1:
        raise RuntimeError("expected one active laptop panel")
    state["panel"] = panels[0]
    state["bindings"] = interface_bindings(args.dock)
    props = command(["systemctl", "show", SERVICE, "-p", "ActiveState", "-p", "LoadState", "-p", "UnitFileState"]).stdout
    state["service"] = dict(line.split("=", 1) for line in props.splitlines() if "=" in line)
    if state["service"].get("ActiveState") != "active" or state["service"].get("LoadState") != "loaded":
        raise RuntimeError("first trial requires the official DisplayLink service running and unmasked")
    mask = Path("/run/systemd/system") / SERVICE
    if mask.exists() or mask.is_symlink():
        raise RuntimeError("an existing runtime service override must be left alone")
    state["compositor_restore_pending"] = False
    state["trace_motion"] = args.trace_motion
    state["duration"] = args.duration
    if args.trace_motion:
        session_command(state, ["/usr/bin/python3", Path(__file__).with_name("trace-desktop.py"), "--check"])
    if args.no_compositor:
        if compositor_command(state, "platformRequiresCompositing") != "false":
            raise RuntimeError("compositor comparison requires a KWin X11 session")
        state["compositor_was_active"] = compositor_active(state)
    state['evdi_copy'] = evdi_copy_configuration(getattr(args, 'evdi_copy_rows', None))
    if getattr(args, 'capture', 'copied') == 'shared':
        # Validate the completed temporary-module switch before stopping the official driver.
        state['evdi_shared'] = evdi_copy_configuration(0)
        try:
            abi = EVDI_SHARED_ABI.read_text().strip()
        except OSError:
            abi = None
        if abi != '1':
            raise RuntimeError('Shared capture requires the prepared experimental EVDI module (ABI 1)')
    state['evdi_copy_restore_pending'] = False
    return state


def prepare(directory):
    state = json.loads((directory / "state.json").read_text())
    if state.get("mock"):
        state["takeover"] = True
        save(directory / "state.json", state)
        return
    # The service's ExecStopPost is armed before this command is invoked.
    if state.get("compositor_was_active"):
        # Record restoration before changing desktop state, including an interrupted D-Bus call.
        state["compositor_restore_pending"] = True
        save(directory / "state.json", state)
        set_compositor_active(state, False)
    state["mask_added"] = True  # write-ahead, so a kill just after masking is recoverable
    save(directory / "state.json", state)
    command(["systemctl", "mask", "--runtime", SERVICE])
    state["takeover"] = True
    save(directory / "state.json", state)
    command(["systemctl", "stop", SERVICE], timeout=12)
    if command(["systemctl", "show", SERVICE, "-p", "MainPID", "--value"]).stdout.strip() != "0":
        raise RuntimeError("DisplayLinkManager did not stop")
    if command(["pgrep", "-x", "DisplayLinkMana"], check=False).returncode == 0:
        raise RuntimeError("another DisplayLinkManager process is still running")
    if state.get('evdi_copy'):
        state['evdi_copy_restore_pending'] = True
        save(directory / 'state.json', state)
        set_evdi_copy_rows(state['evdi_copy'], state['evdi_copy']['rows'])
    panel = state["panel"]
    desktop_command(state, ["--output", panel["name"], "--mode", panel["mode"], "--pos", "0x0", "--primary"])


def configure_layout(directory):
    state = json.loads((directory / "state.json").read_text())
    monitor = next(json.loads(line)["edid"] for line in (directory / "events.jsonl").read_text().splitlines()
                   if json.loads(line).get("event") == "monitor")
    if state.get("expected_monitor_edid") and monitor != state["expected_monitor_edid"]:
        raise RuntimeError("worker selected a different monitor than the comparison target")
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        current = outputs(state)
        targets = [o for o in current if o["connected"] and o["edid"] == monitor]
        if len(targets) == 1:
            panel = match_output(state["panel"], current)
            target = targets[0]
            width = int(state["panel"]["mode"].split("x")[0])
            args = ["--output", panel["name"], "--mode", state["panel"]["mode"], "--pos", "0x0", "--primary",
                    "--output", target["name"], "--mode", "1920x1080", "--rate", "60", "--pos", f"{width}x0"]
            for output in current:
                if output["name"] not in (panel["name"], target["name"]):
                    args += ["--output", output["name"], "--off"]
            desktop_command(state, args)
            return
        time.sleep(0.2)
    raise RuntimeError("selected monitor did not appear in X11 before the layout deadline")


def start_diagnostics(directory):
    state = json.loads((directory / "state.json").read_text())
    if state.get("trace_motion"):
        start_motion_trace(directory, state)


def start_motion_trace(directory, state):
    # Called by the running worker before it connects EVDI, while X11 can still answer.
    # Waiting for X11 after enabling the output can hold up the capture that releases its
    # pending page flip. Keep this out of the synchronous layout hook and ExecStartPre
    # (systemd kills ExecStartPre children). The trace shares the worker's cgroup instead.
    args, env = session_invocation(state, ["/usr/bin/python3", directory / "trace-desktop.py",
                                          "--duration", str(state["duration"] + 35)])
    with open(directory / "motion.jsonl", "w") as log, open(directory / "motion.stderr", "w") as errors:
        os.chmod(log.name, 0o600)
        os.chmod(errors.name, 0o600)
        process = subprocess.Popen(args, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=errors)
    deadline = time.monotonic() + 3
    last_step = "process launch"
    while process.poll() is None and time.monotonic() < deadline:
        for line in (directory / "motion.jsonl").read_text().splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue  # The child's latest line may still be in progress.
            if event.get("event") == "start":
                return
            if event.get("event") == "initializing":
                last_step = event.get("step", last_step)
        time.sleep(0.02)
    raise RuntimeError(f"movement trace did not start (last step: {last_step}); inspect {directory}/motion.stderr")


def trial_error(directory):
    try:
        lines = (directory / "events.jsonl").read_text().splitlines()
    except OSError:
        return None
    for line in reversed(lines):
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("event") == "exit":
            return event.get("error")
    return None


def recover(directory):
    # This runs in a separate oneshot service, even when the worker never sent READY.
    state = json.loads((directory / "state.json").read_text())
    outcome = dict(service_result=os.environ.get("VINO_SERVICE_RESULT", os.environ.get("SERVICE_RESULT")), errors=[], restored=False)
    if state.get("mock"):
        time.sleep(state.get("recovery_delay", 0))
        outcome["restored"] = True
        save(directory / "recovery.json", outcome)
        return
    if state.get('evdi_copy_restore_pending'):
        try:
            set_evdi_copy_rows(state['evdi_copy'], state['evdi_copy']['previous'])
            outcome['evdi_copy_rows_restored'] = state['evdi_copy']['previous']
        except Exception as error:
            outcome['errors'].append(f'EVDI batch restoration: {error}')
    if state.get("compositor_restore_pending"):
        try:
            set_compositor_active(state, True)
        except Exception as error:
            outcome["errors"].append(f"compositor restoration: {error}")
    if state["mask_added"]:
        try:
            mask = Path("/run/systemd/system") / SERVICE
            if mask.is_symlink() and os.readlink(mask) == "/dev/null":
                command(["systemctl", "unmask", "--runtime", SERVICE])
            elif mask.exists() or mask.is_symlink():
                raise RuntimeError("runtime mask changed unexpectedly; refusing to remove another override")
        except Exception as error:
            outcome["errors"].append(str(error))
    if state["takeover"]:
        try:
            command(["systemctl", "start", SERVICE], timeout=15)
        except Exception as error:
            outcome["errors"].append(str(error))
        deadline = time.monotonic() + 18
        last_error = None
        while time.monotonic() < deadline:
            try:
                desktop_command(state, layout_arguments(state["outputs"], outputs(state)))
                restored = outputs(state)
                for old in state["outputs"]:
                    if old["mode"]:
                        new = match_output(old, restored)
                        if any(new[k] != old[k] for k in ("mode", "x", "y", "primary")):
                            raise RuntimeError("output layout has not settled")
                last_error = None
                break
            except Exception as error:
                last_error = str(error)
                time.sleep(0.5)
        if last_error:
            outcome["errors"].append(last_error)
    if interface_bindings(state["dock"]) != state["bindings"]:
        outcome["errors"].append("dock audio/network bindings changed")
    outcome["restored"] = not outcome["errors"]
    save(directory / "recovery.json", outcome)
    if state.get("profile_cpu"):
        # The worker has exited. Preserve its exact, root-owned symbols for perf report
        # without allowing the desktop account to modify the staged executable.
        os.chmod(directory / "worker", 0o744)
    # Leave logs available to the desktop account without exposing Xauthority contents.
    for path in directory.iterdir():
        if path.is_file() and path.name not in ("helper.py", "worker", "trace-desktop.py", "comparison.py"):
            os.chmod(path, 0o600)
            os.chown(path, state["uid"], state["gid"])
    if outcome["errors"]:
        raise RuntimeError("; ".join(outcome["errors"]))


def start_recovery(directory):
    state = json.loads((directory / "state.json").read_text())
    args = ["systemd-run", *(["--user"] if state.get("mock") else []),
            "--quiet", "--no-block", "--collect", "--unit", f"vino-recover-{directory.name}",
            "--property", "Type=oneshot", "--property", "TimeoutStartSec=45s",
            "--property", "TimeoutStopSec=2s", "--property", "KillMode=control-group",
            "--setenv", f"VINO_SERVICE_RESULT={os.environ.get('SERVICE_RESULT', 'unknown')}",
            "--", "/usr/bin/python3", str(directory / "helper.py"), "recover", str(directory)]
    command(args, timeout=1.5)


def unit_command(directory, worker, duration, *, user=False, startup=30, watchdog=5, stop_timeout=2):
    helper = f"/usr/bin/python3 {directory}/helper.py"
    args = ["systemd-run", *( ["--user"] if user else []), "--quiet", "--no-block", "--collect",
            "--unit", f"vino-trial-{directory.name}"]
    properties = ["Type=notify", "NotifyAccess=all", "Restart=no", "KillMode=control-group", "LimitCORE=0",
                  f"TimeoutStartSec={startup}s", f"TimeoutStopSec={stop_timeout}s", f"TimeoutAbortSec={stop_timeout}s",
                  f"WatchdogSec={watchdog}s", "WatchdogSignal=SIGTERM", f"RuntimeMaxSec={duration}s",
                  f"ExecStartPre={helper} prepare {directory}", f"ExecStopPost={helper} recover-start {directory}",
                  f"Environment=VINO_SUPERVISED=1 VINO_TRIAL_DIR={directory}"]
    for prop in properties:
        args += ["--property", prop]
    return args + ["--", *map(str, worker)]


def wait_for_recovery(directory, maximum):
    deadline = time.monotonic() + maximum
    while time.monotonic() < deadline:
        path = directory / "recovery.json"
        if path.is_file():
            return json.loads(path.read_text())
        time.sleep(0.2)
    raise RuntimeError(f"recovery did not finish; inspect {directory} and journalctl -u vino-trial-{directory.name}")


def worker_arguments(args, directory=None):
    result = [str(args.binary), "--dock", args.dock, "--head", str(args.head), "--card", str(args.card),
              "--device", str(args.device), "--encoder", args.encoder, "--source", args.source,
              "--duration", str(args.duration), "--fps", str(args.fps), "--cursor", args.cursor]
    # Copied is also the historical worker default, so --binary can still select an older
    # known-good worker during recovery/comparison without requiring it to know the new flag.
    if getattr(args, "capture", "copied") != "copied":
        result += ["--capture", args.capture]
    if getattr(args, "comparison", "cpu") != "cpu":
        result += ["--comparison", args.comparison]
    if directory:
        result[0] = str(directory / "worker")
        result += ["--log", str(directory / "events.jsonl")]
    if args.validation:
        result.append("--validation")
    return result


def validate_capture(args):
    if getattr(args, "capture", "copied") in ("direct", "shared") and (
            args.encoder != "gpu" or args.source != "desktop" or args.cursor != "client"
            or args.head != 0):
        raise ValueError("native capture requires --encoder gpu --source desktop --cursor client --head 0")
    if getattr(args, "comparison", "cpu") == "gpu" and getattr(args, "capture", "copied") == "copied":
        raise ValueError("GPU comparison requires --capture direct or shared")
    if getattr(args, "capture", "copied") == "shared" and getattr(args, "evdi_copy_rows", None) not in (None, 0):
        raise ValueError("shared capture bypasses the EVDI CPU copy; omit --evdi-copy-rows")


def cpu_profile_command(worker, output):
    # Inherit into the worker's threads. CPU-clock samples CPU execution, including
    # kernel work charged to the capture thread, rather than time waiting for USB.
    # DWARF unwinding works with the existing optimized binary's .eh_frame tables.
    return ["/usr/bin/perf", "record", "--event", "cpu-clock", "--freq", str(PROFILE_HZ),
            "--call-graph", "dwarf,8192", "--clockid", "mono", "--timestamp", "--sample-cpu",
            "--no-buildid-cache", "--output", str(output), "--", *map(str, worker)]


def prepare_cpu_profile(directory):
    # This must succeed before systemd runs prepare() or changes either display driver.
    # Keep symbols from this boot: unprivileged offline analysis cannot read their
    # addresses from /proc/kallsyms after the root-owned recording has finished.
    symbols = Path("/proc/kallsyms").read_text()
    if not any(line.split()[0].strip("0") for line in symbols.splitlines() if line.split()):
        raise RuntimeError("kernel symbol addresses are unavailable; CPU profile not started")
    symbol_file = directory / "profile-kallsyms"
    symbol_file.touch(mode=0o600, exist_ok=False)
    symbol_file.write_text(symbols)
    probe = directory / "profile-check.data"
    workload = ["/usr/bin/python3", "-c",
                "import time\nend = time.monotonic() + 0.35\nwhile time.monotonic() < end: pass"]
    result = command(cpu_profile_command(workload, probe), timeout=12)
    report = command(["/usr/bin/perf", "script", "--input", probe,
                      "--kallsyms", symbol_file, "--fields", "comm,pid,tid,time,event,ip,sym,dso"], timeout=12)
    if not report.stdout.strip():
        raise RuntimeError("CPU profiler recorded no preflight samples; displays were not changed")
    (directory / "profile-check.txt").write_text(result.stderr + report.stdout + report.stderr)
    save(directory / "profile.json", {
        "event": "cpu-clock", "frequency_hz": PROFILE_HZ, "call_graph": "dwarf,8192",
        "clock": "CLOCK_MONOTONIC", "scope": "worker and inherited threads/children; user and kernel CPU",
        "perf_version": command(["/usr/bin/perf", "--version"]).stdout.strip(),
        "kernel": os.uname().release,
        "worker_sha256": hashlib.sha256((directory / "worker").read_bytes()).hexdigest(),
        "record_command": cpu_profile_command([], directory / "profile.data")[:-1],
        "preflight_passed": True,
        "note": "Sampling adds overhead; this run diagnoses CPU costs, not an unprofiled CPU/power comparison",
    })


def launch(args):
    validate_capture(args)
    if os.geteuid() != 0:
        raise RuntimeError("run requires sudo with DISPLAY and XAUTHORITY preserved; preflight and self-test do not")
    lock = open("/run/lock/vino-live-trial.lock", "w")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    # A launcher killed earlier may have released the lock while its unit is still recovering.
    if command(["systemctl", "list-units", "--all", "--no-legend", "vino-trial-*", "vino-recover-*", "vino-module-*"], check=False).stdout.strip():
        raise RuntimeError("an earlier trial unit still exists; wait for its recovery")
    state = snapshot(args)
    state["profile_cpu"] = args.profile_cpu
    # /run is noexec on this laptop. Keep the root-owned executable snapshot on /var/tmp.
    if os.statvfs("/var/tmp").f_flag & os.ST_NOEXEC:
        raise RuntimeError("/var/tmp is noexec; no driver changes were made")
    directory = Path(tempfile.mkdtemp(prefix="vino-live-", dir="/var/tmp"))
    os.chmod(directory, 0o755)
    shutil.copyfile(__file__, directory / "helper.py")
    shutil.copyfile(args.binary, directory / "worker")
    if args.trace_motion:
        shutil.copyfile(Path(__file__).with_name("trace-desktop.py"), directory / "trace-desktop.py")
        os.chmod(directory / "trace-desktop.py", 0o644)
    os.chmod(directory / "helper.py", 0o700)
    os.chmod(directory / "worker", 0o700)
    save(directory / "state.json", state)
    os.chmod(directory / "state.json", 0o600)
    print(f"Trial logs: {directory}", flush=True)
    # Exercise the actual staged executable before arming any device changes.
    command([*worker_arguments(args, directory), "--check"], timeout=25)
    worker = worker_arguments(args, directory)
    if args.profile_cpu:
        prepare_cpu_profile(directory)
        worker = cpu_profile_command(worker, directory / "profile.data")
        print(f"CPU profiling ready: use the Dell for {args.duration} seconds after its desktop appears; "
              "the original layout will then be restored", flush=True)
    command(unit_command(directory, worker, args.duration + 2,
                         stop_timeout=5 if args.profile_cpu else 2))
    try:
        outcome = wait_for_recovery(directory, args.duration + 80)
    except KeyboardInterrupt:
        command(["systemctl", "stop", "--no-block", f"vino-trial-{directory.name}"])
        outcome = wait_for_recovery(directory, 50)
    print(json.dumps(outcome, indent=2))
    if not outcome["restored"]:
        raise RuntimeError("automatic restoration needs attention; recovery.json records the failures")
    if outcome["service_result"] != "success":
        detail = trial_error(directory)
        reason = f": {detail}" if detail else ""
        raise RuntimeError(f"trial stopped with {outcome['service_result']}{reason}; DisplayLink restoration completed")


def mock_worker(case):
    if case == "early-exit":
        sys.exit(7)
    if case == "startup-hang":
        time.sleep(20)
    notify("READY=1\nWATCHDOG=1")
    if case == "normal":
        time.sleep(0.3)
    elif case == "crash":
        os.kill(os.getpid(), signal.SIGKILL)
    elif case == "hang":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        time.sleep(20)
    elif case == "runtime-limit":
        while True:
            notify("WATCHDOG=1")
            time.sleep(0.2)
    elif case == "interrupt":
        os.kill(os.getpid(), signal.SIGINT)


def self_test():
    report = []
    for case in ("normal", "early-exit", "startup-hang", "crash", "hang", "runtime-limit", "interrupt"):
        directory = Path(tempfile.mkdtemp(prefix="vino-test-"))
        shutil.copyfile(__file__, directory / "helper.py")
        save(directory / "state.json", {"mock": True, "takeover": False,
                                       "recovery_delay": 3 if case in ("hang", "early-exit", "startup-hang") else 0})
        worker = [sys.executable, directory / "helper.py", "mock-worker", case]
        started = time.monotonic()
        command(unit_command(directory, worker, 3, user=True, startup=2, watchdog=1))
        outcome = wait_for_recovery(directory, 15)
        elapsed = time.monotonic() - started
        assert outcome["restored"], outcome
        assert (outcome["service_result"] == "success") == (case in ("normal", "interrupt")), outcome
        report.append(dict(case=case, elapsed_s=round(elapsed, 2), **outcome))
        print(json.dumps(report[-1]), flush=True)
    print("All supervision cases restored their simulated driver state; no display devices were opened")


def main():
    if len(sys.argv) > 1 and sys.argv[1] in ("prepare", "recover", "recover-start", "layout", "trace-start", "mock-worker"):
        action = sys.argv[1]
        if action == "mock-worker":
            mock_worker(sys.argv[2])
        else:
            {"prepare": prepare, "recover": recover, "recover-start": start_recovery,
             "layout": configure_layout, "trace-start": start_diagnostics}[action](Path(sys.argv[2]))
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("preflight", "run", "self-test"))
    parser.add_argument("--binary", type=Path, default=ROOT / "revdi/target/release/vino-gpu-live")
    parser.add_argument("--dock", default="8-1.2")
    parser.add_argument("--head", type=int, choices=(0, 1), default=0)
    parser.add_argument("--card", type=int, default=0)
    parser.add_argument("--device", type=int, default=0)
    parser.add_argument("--encoder", choices=("cpu", "gpu"), default="gpu")
    parser.add_argument("--source", choices=("pattern", "desktop"), default="desktop")
    parser.add_argument("--capture", choices=("copied", "direct", "shared"), default="copied",
                        help="RGB snapshots, native CPU copy, or experimental shared DMA-buf GPU copy")
    parser.add_argument("--comparison", choices=("cpu", "gpu"), default="cpu",
                        help="exact strip change detection for native capture")
    parser.add_argument("--cursor", choices=("client", "evdi"), default="evdi",
                        help="compose cursor events in the client, or use EVDI's combined capture for comparison")
    parser.add_argument("--no-compositor", action="store_true",
                        help="temporarily suspend KWin X11 compositing and restore it during recovery")
    parser.add_argument("--trace-motion", action="store_true",
                        help="record X11 window geometry and pointer positions during the supervised trial")
    parser.add_argument("--profile-cpu", action="store_true",
                        help="record worker user/kernel CPU call stacks with perf during a supervised run")
    parser.add_argument('--evdi-copy-rows', type=int, choices=(0, 32, 128),
                        help='temporarily set the prepared EVDI candidate batch size; recovery resets it to zero')
    parser.add_argument("--duration", type=int)
    parser.add_argument("--fps", type=int)
    parser.add_argument("--validation", action="store_true")
    args = parser.parse_args()
    try:
        validate_capture(args)
    except ValueError as error:
        parser.error(str(error))
    if args.no_compositor and args.source != "desktop":
        parser.error("--no-compositor requires --source desktop")
    if args.trace_motion and args.source != "desktop":
        parser.error("--trace-motion requires --source desktop")
    if args.profile_cpu and args.action != "run":
        parser.error("--profile-cpu requires run; profiler access is checked before display takeover")
    args.duration = args.duration if args.duration is not None else (60 if args.source == "pattern" else 900)
    args.fps = args.fps if args.fps is not None else (10 if args.source == "pattern" else 60)
    if not 1 <= args.duration <= 900 or not 1 <= args.fps <= 60 or not 0 <= args.card < 64:
        parser.error("duration must be 1..900, fps 1..60, and card 0..63")
    if args.action == "self-test":
        self_test()
    elif args.action == "preflight":
        state = snapshot(args)
        print(command([*worker_arguments(args), "--check"], timeout=25).stdout.strip())
        print(json.dumps({"panel": state["panel"]["name"], "dock_bindings": state["bindings"],
                          "service": state["service"], "hardware_changed": False}, indent=2))
    else:
        launch(args)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"live-trial: {error}", file=sys.stderr)
        sys.exit(1)
