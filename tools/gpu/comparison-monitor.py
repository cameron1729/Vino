#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Read-only benchmark telemetry and a scoped KDE screen-lock inhibitor."""
import json
import os
from pathlib import Path
import signal
import sys
import time

TICKS = os.sysconf("SC_CLK_TCK")
SCREENSAVER = "org.freedesktop.ScreenSaver"
SCREEN_PATH = "/ScreenSaver"


def optional_number(path, scale=1):
    try:
        return float(Path(path).read_text()) / scale
    except (OSError, ValueError):
        return None


def thermal_sensors(root=Path("/sys/class/hwmon")):
    sensors = []
    for hwmon in sorted(root.glob("hwmon*")):
        try:
            name = (hwmon / "name").read_text().strip()
        except OSError:
            continue
        # Keep each fan separate; ACPI may report a limit/firmware value rather
        # than the same tachometer as thinkpad. Never average different fans.
        files = list(hwmon.glob("fan*_input"))
        if name in ("k10temp", "coretemp", "thinkpad"):
            files += list(hwmon.glob("temp*_input"))
        for path in sorted(files):
            kind = "fan_rpm" if path.name.startswith("fan") else "temperature_c"
            scale = 1 if kind == "fan_rpm" else 1000
            if optional_number(path, scale) is None:
                continue
            label_path = path.with_name(path.name.replace("_input", "_label"))
            label = label_path.read_text().strip() if label_path.exists() else path.name
            sensors.append(dict(id=f"{hwmon.name}/{path.name}", device=name, label=label,
                                kind=kind, path=str(path), scale=scale))
    return sensors


def sampled_metric(samples, read):
    """Time-weighted mean without bridging missing readings."""
    values = [read(sample) for sample in samples]
    intervals = [(b["monotonic_s"] - a["monotonic_s"], x, y)
                 for a, b, x, y in zip(samples, samples[1:], values, values[1:])
                 if x is not None and y is not None]
    span = samples[-1]["monotonic_s"] - samples[0]["monotonic_s"]
    covered = sum(dt for dt, _, _ in intervals)
    present = [value for value in values if value is not None]
    return dict(mean=sum(dt * (x + y) / 2 for dt, x, y in intervals) / covered if covered else None,
                min=min(present) if present else None, max=max(present) if present else None,
                start=values[0], end=values[-1], samples=len(present),
                coverage_fraction=covered / span if span > 0 else 0)


def process_snapshot(root=Path("/proc")):
    started = time.monotonic()
    boot_time = time.clock_gettime(time.CLOCK_BOOTTIME)
    processes, unreadable = {}, 0
    for directory in root.iterdir():
        if not directory.name.isdigit():
            continue
        try:
            raw = (directory / "stat").read_text()
            end = raw.rfind(")")
            fields = raw[end + 2:].split()
            pid, start = int(directory.name), int(fields[19])
            cgroup = (directory / "cgroup").read_text().strip()
            processes[f"{pid}:{start}"] = dict(
                pid=pid, start_ticks=start, name=raw[raw.find("(") + 1:end],
                cpu_seconds=(int(fields[11]) + int(fields[12])) / TICKS, cgroup=cgroup)
        except (FileNotFoundError, ProcessLookupError):
            continue  # Exiting processes are expected; unobserved time stays unattributed.
        except (OSError, ValueError, IndexError):
            unreadable += 1
    ended = time.monotonic()
    return dict(monotonic_s=(started + ended) / 2, boottime_s=boot_time,
                scan_seconds=ended - started, unreadable=unreadable, processes=processes)


def process_attribution(samples, tracked, backend):
    """Estimate per-process CPU, retaining PID reuse and exit/scan limitations."""
    elapsed = samples[-1]["monotonic_s"] - samples[0]["monotonic_s"]
    if elapsed <= 0:
        raise RuntimeError("invalid process sampling interval")
    totals, new, exited, resets = {}, set(), set(), 0
    first_boot = samples[0]["boottime_s"]
    for before, after in zip(samples, samples[1:]):
        exited.update(before["processes"].keys() - after["processes"].keys())
        for identity, current in after["processes"].items():
            previous = before["processes"].get(identity)
            if previous is not None:
                delta = current["cpu_seconds"] - previous["cpu_seconds"]
            elif identity not in totals and current["start_ticks"] / TICKS >= first_boot:
                delta = current["cpu_seconds"]
                new.add(identity)
            else:
                # A pre-existing process missed by an earlier scan has no baseline.
                delta = 0
            if delta < 0:
                resets += 1
                continue
            role = next((name for name, counter in tracked.items()
                         if (current["pid"], current["start_ticks"]) ==
                         (counter["pid"], counter["start_ticks"])), "background")
            if backend == "official" and any(
                    line.partition("::")[2].split("/")[1:3] == ["system.slice", "displaylink-driver.service"]
                    for line in current["cgroup"].splitlines()):
                role = "driver"
            entry = totals.setdefault(identity, {**current, "role": role, "cpu_seconds_observed": 0})
            entry["cpu_seconds_observed"] += delta
    rows = sorted((dict(entry, cpu_cores=entry["cpu_seconds_observed"] / elapsed)
                   for entry in totals.values() if entry["cpu_seconds_observed"] > 0),
                  key=lambda row: row["cpu_cores"], reverse=True)
    return dict(seconds=elapsed, sample_count=len(samples),
                max_gap_seconds=max(b["monotonic_s"] - a["monotonic_s"] for a, b in zip(samples, samples[1:])),
                scan_seconds=sum(s["scan_seconds"] for s in samples),
                unreadable=sum(s["unreadable"] for s in samples), counter_resets=resets,
                new_processes=len(new), exited_processes=len(exited),
                background_cpu_cores_observed=sum(r["cpu_cores"] for r in rows if r["role"] == "background"),
                processes=rows,
                limitations="One-second /proc scans miss short-lived processes and final CPU after the last sighting; "
                            "names and cgroups only, no command arguments. Background includes deferred kernel work, "
                            "which may be display-related. These CPU figures cannot be subtracted from power.")


def cgroup_snapshot(root=Path("/sys/fs/cgroup")):
    """Read cumulative service CPU, including children that exited between scans."""
    started = time.monotonic()
    groups, unreadable = {}, []

    def walk_error(error):
        unreadable.append(str(error.filename))

    for directory, _, files in os.walk(root, onerror=walk_error):
        if "cpu.stat" not in files:
            continue
        path = Path(directory)
        relative = path.relative_to(root)
        name = "/" if relative == Path(".") else "/" + relative.as_posix()
        try:
            identity = path.stat()
            fields = dict(line.split() for line in (path / "cpu.stat").read_text().splitlines())
            current = path.stat()
            if (identity.st_dev, identity.st_ino) != (current.st_dev, current.st_ino):
                unreadable.append(name)
                continue
            groups[name] = dict(device=identity.st_dev, inode=identity.st_ino,
                                cpu_seconds=int(fields["usage_usec"]) / 1e6)
        except FileNotFoundError:
            continue  # An ancestor's counter can still account for a deleted group.
        except (OSError, ValueError, KeyError):
            unreadable.append(name)
    ended = time.monotonic()
    return dict(monotonic_s=(started + ended) / 2, scan_seconds=ended - started,
                unreadable=unreadable, groups=groups)


def cgroup_attribution(before, after):
    """Keep inclusive hierarchy counters separate: never add parents to children."""
    elapsed = after["monotonic_s"] - before["monotonic_s"]
    if elapsed <= 0:
        raise RuntimeError("invalid cgroup sampling interval")
    previous, current = before["groups"], after["groups"]
    rows, replaced, resets = [], [], []
    for name in previous.keys() & current.keys():
        a, b = previous[name], current[name]
        if (a["device"], a["inode"]) != (b["device"], b["inode"]):
            replaced.append(name)
            continue
        delta = b["cpu_seconds"] - a["cpu_seconds"]
        if delta < 0:
            resets.append(name)
            continue
        rows.append(dict(path=name, inclusive_cpu_seconds=delta,
                         inclusive_cpu_cores=delta / elapsed))
    rows.sort(key=lambda row: (-row["inclusive_cpu_cores"], row["path"]))
    return dict(seconds=elapsed, groups=rows, replaced=sorted(replaced), counter_resets=sorted(resets),
                appeared=sorted(current.keys() - previous.keys()),
                disappeared=sorted(previous.keys() - current.keys()),
                unreadable=before["unreadable"] + after["unreadable"],
                scan_seconds=before["scan_seconds"] + after["scan_seconds"],
                limitations="Inclusive CPU includes descendant groups and exited processes. "
                            "Rows overlap and must not be summed. New, deleted or recreated groups "
                            "have no direct delta; surviving ancestors retain their charged CPU. "
                            "Groups can mix display, benchmark and background work. "
                            "Diagnostic only; does not change power qualification or subtract watts.")


def screen_guard(action):
    """Own the inhibitor on a persistent user-session bus connection."""
    import dbus
    from dbus.mainloop.glib import DBusGMainLoop
    from gi.repository import GLib

    DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus(private=True)
    owner = str(bus.get_name_owner(SCREENSAVER))
    proxy = bus.get_object(owner, SCREEN_PATH)
    saver = dbus.Interface(proxy, SCREENSAVER)
    cookie = None
    failed = False
    loop = GLib.MainLoop()

    def emit(event, **fields):
        print(json.dumps(dict(event=event, monotonic_s=time.monotonic(), **fields)), flush=True)

    def invalid(reason):
        nonlocal failed
        if not failed:
            failed = True
            emit("invalid", reason=reason)
        loop.quit()

    def active_changed(active):
        if active:
            invalid("screen locked during comparison")

    def owner_changed(name, old, new):
        if str(name) == SCREENSAVER and str(new) != owner:
            invalid("screen-lock service changed; inhibitor no longer assured")

    def heartbeat():
        try:
            if str(bus.get_name_owner(SCREENSAVER)) != owner:
                raise RuntimeError("screen-lock service changed")
            if saver.GetActive(timeout=2):
                raise RuntimeError("screen is locked")
            if not failed:
                emit("heartbeat")
        except Exception as error:
            invalid(str(error))
        return not failed

    try:
        # Subscribe before checking/inhibiting, including KDE's acquisition
        # signal: GetActive becomes true only after the lock is established.
        bus.add_signal_receiver(active_changed, signal_name="ActiveChanged",
                                dbus_interface=SCREENSAVER, bus_name=owner, path=SCREEN_PATH)
        bus.add_signal_receiver(lambda: invalid("screen lock is starting"),
                                signal_name="AboutToLock", dbus_interface="org.kde.screensaver",
                                bus_name=owner, path=SCREEN_PATH)
        bus.add_signal_receiver(owner_changed, signal_name="NameOwnerChanged",
                                dbus_interface="org.freedesktop.DBus", bus_name="org.freedesktop.DBus",
                                path="/org/freedesktop/DBus", arg0=SCREENSAVER)
        if saver.GetActive(timeout=2):
            raise RuntimeError("unlock the desktop before comparing drivers")
        if action == "check":
            # Check availability without inhibiting, locking, or changing settings.
            xml = dbus.Interface(proxy, "org.freedesktop.DBus.Introspectable").Introspect(timeout=2)
            for member in ('name="Inhibit"', 'name="UnInhibit"', 'name="AboutToLock"'):
                if member not in xml:
                    raise RuntimeError(f"KDE screen-lock interface lacks {member}")
            if failed:
                raise RuntimeError("screen lock started during preflight")
            emit("checked", owner=owner, active=False)
            return
        cookie = saver.Inhibit("Vino driver comparison", "Bounded display power measurement", timeout=3)
        if saver.GetActive(timeout=2):
            raise RuntimeError("screen locked while acquiring inhibitor")
        if failed:
            raise RuntimeError("screen lock started while acquiring inhibitor")

        def parent_input(fd, condition):
            # EOF also releases the inhibitor if the coordinator dies.
            os.read(fd, 4096)
            loop.quit()
            return False

        GLib.io_add_watch(sys.stdin.fileno(), GLib.IO_IN | GLib.IO_HUP | GLib.IO_ERR, parent_input)
        GLib.timeout_add(1000, heartbeat)
        for signum in (signal.SIGTERM, signal.SIGINT):
            GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signum, lambda: (loop.quit(), False)[1])
        emit("ready", pid=os.getpid(), owner=owner, inhibited=True)
        loop.run()
    finally:
        if cookie is not None:
            try:
                saver.UnInhibit(cookie, timeout=2)
            except dbus.DBusException:
                pass  # KDE also releases the cookie when this bus owner disappears.
        bus.close()
    if failed:
        raise RuntimeError("screen-lock guard invalidated the comparison")


if __name__ == "__main__":
    try:
        if sys.argv[1:] not in (["guard"], ["check"]):
            raise RuntimeError("expected guard or check")
        screen_guard(sys.argv[1])
    except Exception as error:
        print(f"comparison-monitor: {error}", file=sys.stderr)
        sys.exit(1)
