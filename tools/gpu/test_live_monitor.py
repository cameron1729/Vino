"""Telemetry and isolated D-Bus tests; never lock or change the real desktop."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

import test_live_comparison as comparison_tests

comparison = comparison_tests.comparison
monitor = comparison.monitor


class AttributionTests(unittest.TestCase):
    def snapshot(self, seconds, entries):
        return dict(monotonic_s=seconds, boottime_s=seconds, scan_seconds=.001, unreadable=0,
                    processes={f"{pid}:{start}": dict(pid=pid, start_ticks=start, name=name,
                        cpu_seconds=cpu, cgroup=f"0::{cgroup}") for pid, start, name, cpu, cgroup in entries})

    def test_pid_reuse_new_and_exited_processes_are_separate(self):
        samples = [
            self.snapshot(10, [(7, 100, "old", 50, "/apps"), (8, 100, "stays", 1, "/apps")]),
            self.snapshot(11, [(7, 1050, "new", .2, "/apps"), (8, 100, "stays", 1.1, "/apps")]),
            self.snapshot(12, [(7, 1050, "new", .4, "/apps")])]
        result = monitor.process_attribution(samples, {}, "gpu")
        self.assertAlmostEqual(result["background_cpu_cores_observed"], .25)
        self.assertEqual(result["new_processes"], 1)
        self.assertEqual(result["exited_processes"], 2)
        self.assertEqual([r["name"] for r in result["processes"]], ["new", "stays"])
        self.assertEqual(result["counter_resets"], 0)

    def test_official_children_and_guard_are_not_background(self):
        entries = [(1, 100, "official", 1, "/system.slice/displaylink-driver.service"),
                   (2, 100, "child", 1, "/system.slice/displaylink-driver.service/child"),
                   (3, 100, "guard", 1, "/system.slice/vino-trial"),
                   (4, 100, "other", 1, "/system.slice/displaylink-driver.service-unrelated")]
        samples = [self.snapshot(10, entries), self.snapshot(11, [
            (pid, start, name, cpu + .1, group) for pid, start, name, cpu, group in entries])]
        result = monitor.process_attribution(samples, {"screen_guard": dict(pid=3, start_ticks=100)}, "official")
        roles = {r["name"]: r["role"] for r in result["processes"]}
        self.assertEqual(roles, dict(official="driver", child="driver", guard="screen_guard", other="background"))
        self.assertAlmostEqual(result["background_cpu_cores_observed"], .1)

    def test_counter_reset_is_reported_without_negative_cpu(self):
        samples = [self.snapshot(10, [(1, 100, "x", 10, "/")]),
                   self.snapshot(11, [(1, 100, "x", 1, "/")])]
        result = monitor.process_attribution(samples, {}, "gpu")
        self.assertEqual(result["counter_resets"], 1)
        self.assertEqual(result["background_cpu_cores_observed"], 0)

    def test_proc_names_and_disappearing_processes_do_not_read_command_arguments(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            path = root / "42"
            path.mkdir()
            fields = ["S"] + ["0"] * 49
            fields[11], fields[12], fields[19] = "20", "30", "123"
            (path / "stat").write_text("42 (name ) with space) " + " ".join(fields))
            (path / "cgroup").write_text("0::/apps\n")
            (path / "cmdline").write_text("SECRET ARGUMENT")
            (root / "43").mkdir()
            result = monitor.process_snapshot(root)
            self.assertEqual(result["processes"]["42:123"]["name"], "name ) with space")
            self.assertNotIn("SECRET", json.dumps(result))
            self.assertEqual(result["unreadable"], 0)

    def test_background_residual_excludes_player_sampler_and_screen_guard(self):
        result = dict(system_busy_cpu_cores=1, display_stack_cpu_cores=.5,
                      cpu_cores=dict(player=.2, sampler=.1, screen_guard=.02),
                      process_attribution=dict(unreadable=0, counter_resets=0, max_gap_seconds=1, processes=[]))
        assessment = comparison.background_assessment(result, {})
        self.assertAlmostEqual(assessment["cpu_cores"], .18)
        self.assertEqual(assessment["warnings"], [])
        result["system_busy_cpu_cores"] = 2
        self.assertIn("exceeds", comparison.background_assessment(result, {})["warnings"][0])


class CgroupTests(unittest.TestCase):
    def snapshot(self, seconds, groups):
        return dict(monotonic_s=seconds, scan_seconds=.001, unreadable=[],
                    groups={path: dict(device=1, inode=inode, cpu_seconds=cpu)
                            for path, inode, cpu in groups})

    def test_parent_retains_exited_child_cpu_without_summing_overlapping_rows(self):
        before = self.snapshot(10, [("/", 1, 100), ("/cron.service", 2, 20),
                                    ("/cron.service/child", 3, 5)])
        after = self.snapshot(12, [("/", 1, 103), ("/cron.service", 2, 22)])
        result = monitor.cgroup_attribution(before, after)
        self.assertEqual(result["disappeared"], ["/cron.service/child"])
        self.assertEqual(result["groups"], [
            dict(path="/", inclusive_cpu_seconds=3, inclusive_cpu_cores=1.5),
            dict(path="/cron.service", inclusive_cpu_seconds=2, inclusive_cpu_cores=1)])
        self.assertIn("must not be summed", result["limitations"])

    def test_recreated_reset_and_new_groups_do_not_count_lifetime_cpu(self):
        before = self.snapshot(10, [("/replaced", 1, 10), ("/reset", 2, 20)])
        after = self.snapshot(12, [("/replaced", 3, 100), ("/reset", 2, 1), ("/new", 4, 300)])
        result = monitor.cgroup_attribution(before, after)
        self.assertEqual(result["groups"], [])
        self.assertEqual(result["replaced"], ["/replaced"])
        self.assertEqual(result["counter_resets"], ["/reset"])
        self.assertEqual(result["appeared"], ["/new"])

    def test_snapshot_reads_cumulative_usage_and_records_bad_counters(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            (root / "cpu.stat").write_text("usage_usec 1500000\nuser_usec 1000000\nsystem_usec 500000\n")
            (root / "bad").mkdir()
            (root / "bad/cpu.stat").write_text("usage_usec invalid\n")
            result = monitor.cgroup_snapshot(root)
            self.assertEqual(result["groups"]["/"]["cpu_seconds"], 1.5)
            self.assertEqual(result["groups"]["/"]["inode"], root.stat().st_ino)
            self.assertEqual(result["unreadable"], ["/bad"])
            missing = monitor.cgroup_snapshot(root / "missing")
            self.assertEqual(missing["groups"], {})
            self.assertTrue(missing["unreadable"])


class SensorTests(unittest.TestCase):
    def test_fans_stay_separate_and_unreadable_values_are_not_zero(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            hw = root / "hwmon0"
            hw.mkdir()
            for field, value in dict(name="thinkpad", fan1_input="2200", fan2_input="2300",
                                     temp1_input="60000", temp1_label="CPU", temp2_input="unavailable").items():
                (hw / field).write_text(value)
            sensors = monitor.thermal_sensors(root)
            self.assertEqual(len(sensors), 3)
            self.assertEqual([s["kind"] for s in sensors], ["fan_rpm", "fan_rpm", "temperature_c"])
            self.assertEqual(sensors[-1]["label"], "CPU")
            (hw / "fan1_input").unlink()
            self.assertIsNone(monitor.optional_number(sensors[0]["path"]))

    def test_missing_sensor_intervals_are_not_interpolated(self):
        samples = [dict(monotonic_s=t, value=v) for t, v in ((0, 10), (1, 20), (3, None), (4, 100))]
        result = monitor.sampled_metric(samples, lambda s: s["value"])
        self.assertEqual(result["mean"], 15)
        self.assertEqual(result["coverage_fraction"], .25)
        self.assertEqual(result["samples"], 3)

    def test_activity_and_fans_are_in_power_summary(self):
        samples = [dict(monotonic_s=t, watts=10, temperature_c=50, gpu_busy_percent=busy,
                        gpu_clock_mhz=800, thermal={"fan": fan, "cpu": cpu})
                   for t, busy, fan, cpu in ((0, 0, 2000, 60), (1, 10, 2200, 62), (4, 10, 2200, 64))]
        result = comparison.power_summary(samples)
        self.assertEqual(result["activity"]["gpu_busy_percent"]["mean"], 8.75)
        self.assertEqual(result["thermal"]["fan"]["mean"], 2175)
        self.assertEqual(result["thermal"]["cpu"]["start"], 60)
        self.assertEqual(result["thermal"]["fan"]["coverage_fraction"], 1)


class PowerPairTests(unittest.TestCase):
    def rows(self):
        rows = []
        for index, block in enumerate(comparison.plan(2, gpu_evdi_rows=128)):
            for case in comparison.CASES:
                m = copy.deepcopy(comparison_tests.ReplayAssessmentTests.SCROLL)
                m.update(input_pacing_ok=True, input_deadline_miss_percent=0, measurement_errors=[],
                         screen_lock=dict(inhibited=True), background=dict(cpu_cores=.1, warnings=[]))
                m["power"]["temperature_start_c"] = 50
                if block["backend"] == "gpu":
                    m["vino_usb"] = dict(input_rate_warning=None, changed_updates_per_second=20, presentations=600)
                rows.append(dict(block=f"block-{index:02d}", **block, case=case, measurement=m))
        return rows

    def assess(self, rows):
        return comparison.power_comparison(rows, dict(pairs=2, gpu_evdi_rows=128))

    def test_current_vino_comparison_is_balanced_and_keeps_original_official_copy(self):
        blocks = comparison.plan(2, gpu_evdi_rows=128)
        self.assertEqual([b["condition"] for b in blocks],
                         ["official-evdi-0", "gpu-evdi-128", "gpu-evdi-128", "official-evdi-0"])
        self.assertTrue(self.assess(self.rows())["all_cases_repeated"])

    def test_one_noisy_sample_excludes_whole_pair_and_preserves_all_raw_observations(self):
        rows = self.rows()
        rows[0]["measurement"]["background"]["warnings"] = ["outside CPU too busy"]
        rows[0]["measurement"]["power"]["mean_watts"] = 999
        result = self.assess(rows)
        self.assertFalse(result["all_cases_repeated"])
        idle = result["cases"][0]
        self.assertEqual(idle["retained_pairs"], 1)
        self.assertFalse(idle["repeated_comparison_available"])
        self.assertTrue(all(m["repetitions"] == 1 for m in idle["matched_observations"]))
        self.assertEqual(len(rows), 12)
        self.assertEqual(rows[0]["measurement"]["power"]["mean_watts"], 999)

    def test_mismatched_background_or_start_temperature_is_excluded(self):
        for field, value, reason in (("background", .23, "background CPU spread"),
                                     ("power", 60, "starting AMD temperature spread")):
            rows = self.rows()
            key = "cpu_cores" if field == "background" else "temperature_start_c"
            rows[0]["measurement"][field][key] = value
            result = self.assess(rows)["cases"][0]["repetitions"][0]
            self.assertFalse(result["included"])
            self.assertIn(reason, "; ".join(result["reasons"]))

    def test_missing_lock_pacing_and_throughput_evidence_never_qualifies(self):
        for field, value in (("screen_lock", {}), ("input_pacing_ok", False),
                              ("vino_usb", dict(input_rate_warning="missing updates"))):
            rows = self.rows()
            row = next(r for r in rows if r["backend"] == "gpu" and r["case"] == "idle")
            row["measurement"][field] = value
            self.assertFalse(self.assess(rows)["cases"][0]["repetitions"][0]["included"])
        rows = self.rows()
        rows.pop(0)
        self.assertIn("missing or duplicate condition",
                      self.assess(rows)["cases"][0]["repetitions"][0]["reasons"])


class CoordinatorMonitorTests(unittest.TestCase):
    def test_failed_inhibitor_prevents_any_display_takeover(self):
        with tempfile.TemporaryDirectory() as name:
            c = comparison.Coordinator.__new__(comparison.Coordinator)
            c.directory, c.state, c.data = Path(name), {}, {}
            c.save, c.run_blocks = Mock(), Mock()
            guard = Mock()
            guard.__enter__ = Mock(side_effect=RuntimeError("inhibitor unavailable"))
            guard.__exit__ = Mock()
            with patch.object(comparison, "SessionGuard", return_value=guard), \
                 patch.object(comparison.trial, "notify"):
                with self.assertRaisesRegex(RuntimeError, "inhibitor unavailable"):
                    c.run()
            c.run_blocks.assert_not_called()

    def test_mid_replay_lock_preserves_partial_sensors_without_valid_measurement(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            c = comparison.Coordinator.__new__(comparison.Coordinator)
            c.directory = directory
            c.settings = dict(warmup=0, seconds=30, input_fps=20)
            c.state = dict(panel=dict(mode="1920x1200"), processes={}, sensor={})
            c.data = dict(runs=[], complete=False)
            c.guard = Mock(ready=dict(pid=42))
            c.tick = Mock()
            c.counters = Mock(return_value=dict(processes={}))
            process = Mock()
            process.poll.return_value = None
            reader = Mock()
            reader.event.side_effect = [dict(pid=99), {}, RuntimeError("screen lock is starting")]
            power = dict(monotonic_s=1, watts=12)
            procs = dict(monotonic_s=1, processes={})
            with patch.object(comparison, "verify_environment", return_value={}), \
                 patch.object(comparison.trial, "session_invocation", return_value=(["unused"], {})), \
                 patch.object(comparison.subprocess, "Popen", return_value=process), \
                 patch.object(comparison, "JsonReader", return_value=reader), \
                 patch.object(comparison, "power_sample", return_value=power), \
                 patch.object(monitor, "process_snapshot", return_value=procs):
                with self.assertRaisesRegex(RuntimeError, "screen lock is starting"):
                    c.replay(directory, dict(backend="gpu"), "scroll", 123)
            self.assertFalse(json.loads((directory / "scroll-interrupted.json").read_text())["valid"])
            self.assertEqual(json.loads((directory / "scroll-power.json").read_text()), [power])
            self.assertEqual(json.loads((directory / "scroll-processes.json").read_text()), [procs])
            self.assertEqual(c.data["runs"], [])
            process.terminate.assert_called_once()
            reader.close.assert_called_once()


def fake_screensaver():
    """Tiny service on a private test bus, including owner-lifetime cleanup."""
    import dbus
    import dbus.service
    from dbus.mainloop.glib import DBusGMainLoop
    from gi.repository import GLib
    DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus()
    name = dbus.service.BusName(monitor.SCREENSAVER, bus=bus)

    class Saver(dbus.service.Object):
        active = False
        cookies = {}
        next_cookie = 0

        @dbus.service.method(monitor.SCREENSAVER, in_signature="", out_signature="b")
        def GetActive(self):
            return self.active

        @dbus.service.method(monitor.SCREENSAVER, in_signature="ss", out_signature="u", sender_keyword="sender")
        def Inhibit(self, app, reason, sender=None):
            self.next_cookie += 1
            self.cookies[self.next_cookie] = sender
            return self.next_cookie

        @dbus.service.method(monitor.SCREENSAVER, in_signature="u", out_signature="")
        def UnInhibit(self, cookie):
            self.cookies.pop(cookie, None)

        @dbus.service.signal(monitor.SCREENSAVER, signature="b")
        def ActiveChanged(self, active):
            pass

        @dbus.service.signal("org.kde.screensaver", signature="")
        def AboutToLock(self):
            pass

        @dbus.service.method("org.vino.Test", in_signature="", out_signature="u")
        def Cookies(self):
            return len(self.cookies)

        @dbus.service.method("org.vino.Test", in_signature="s", out_signature="")
        def Trigger(self, event):
            if event == "acquiring":
                self.AboutToLock()
            elif event == "flash":
                self.ActiveChanged(True)
                self.ActiveChanged(False)
            elif event == "initially-locked":
                self.active = True
            elif event == "service-replaced":
                bus.release_name(monitor.SCREENSAVER)

        def disconnected(self, name, old, new):
            if not new:
                for cookie, owner in list(self.cookies.items()):
                    if owner == name:
                        self.cookies.pop(cookie)

    saver = Saver(bus, monitor.SCREEN_PATH)
    bus.add_signal_receiver(saver.disconnected, signal_name="NameOwnerChanged",
                            dbus_interface="org.freedesktop.DBus")
    print("ready", flush=True)
    GLib.MainLoop().run()


@unittest.skipUnless(shutil.which("dbus-daemon") and importlib.util.find_spec("dbus")
                     and importlib.util.find_spec("gi"), "D-Bus and GLib required")
class ScreenGuardTests(unittest.TestCase):
    def setUp(self):
        import dbus
        self.temporary = tempfile.TemporaryDirectory(prefix="vino-guard-test-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.bus_process = subprocess.Popen(["dbus-daemon", "--session", "--nofork", "--print-address=1"],
                                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.stop, self.bus_process)
        self.assertTrue(select.select([self.bus_process.stdout], [], [], 3)[0])
        address = self.bus_process.stdout.readline().strip()
        self.assertTrue(address)
        self.env = {**os.environ, "DBUS_SESSION_BUS_ADDRESS": address}
        self.server = subprocess.Popen(["/usr/bin/python3", __file__, "--fake-screensaver"], env=self.env,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.stop, self.server)
        self.assertTrue(select.select([self.server.stdout], [], [], 3)[0])
        self.assertEqual(self.server.stdout.readline().strip(), "ready")
        self.bus = dbus.bus.BusConnection(address)
        self.addCleanup(self.bus.close)
        self.control = dbus.Interface(self.bus.get_object(monitor.SCREENSAVER, monitor.SCREEN_PATH), "org.vino.Test")
        invocation = patch.object(comparison.trial, "session_invocation",
                                  side_effect=lambda state, args: (args, self.env))
        invocation.start()
        self.addCleanup(invocation.stop)

    @staticmethod
    def stop(process):
        comparison.stop_process(process)
        for stream in (process.stdout, process.stderr):
            if stream:
                stream.close()

    def await_release(self):
        deadline = time.monotonic() + 3
        while self.control.Cookies() and time.monotonic() < deadline:
            time.sleep(.02)
        self.assertEqual(self.control.Cookies(), 0)

    def test_scoped_inhibitor_heartbeat_and_release(self):
        with comparison.SessionGuard({}, self.directory) as guard:
            self.assertEqual(self.control.Cookies(), 1)
            result = guard.confirm(guard.check)
            self.assertTrue(result["inhibited"])
        self.await_release()

    def test_brief_lock_and_acquisition_are_detected_even_when_getactive_is_false(self):
        for event in ("flash", "acquiring"):
            with self.subTest(event=event), comparison.SessionGuard({}, self.directory) as guard:
                self.control.Trigger(event)
                deadline = time.monotonic() + 3
                with self.assertRaisesRegex(RuntimeError, "screen lock"):
                    while time.monotonic() < deadline:
                        guard.check()
                        time.sleep(.02)
            self.await_release()

    def test_helper_kill_and_parent_pipe_close_release_inhibitor(self):
        for abrupt in (True, False):
            with self.subTest(abrupt=abrupt), comparison.SessionGuard({}, self.directory) as guard:
                if abrupt:
                    guard.process.kill()
                else:
                    guard.process.stdin.close()
                guard.process.wait(timeout=3)
                self.await_release()
                with self.assertRaisesRegex(RuntimeError, "guard exited"):
                    guard.check()

    def test_service_replacement_invalidates_inhibition(self):
        with comparison.SessionGuard({}, self.directory) as guard:
            self.control.Trigger("service-replaced")
            with self.assertRaisesRegex(RuntimeError, "screen-lock service changed"):
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    guard.check()
                    time.sleep(.02)

    def test_already_locked_session_never_acquires_inhibitor(self):
        self.control.Trigger("initially-locked")
        with self.assertRaisesRegex(RuntimeError, "unlock the desktop"):
            with comparison.SessionGuard({}, self.directory):
                self.fail("locked session was accepted")
        self.await_release()

    def test_coordinator_failure_releases_guard_and_stale_heartbeat_is_rejected(self):
        c = comparison.Coordinator.__new__(comparison.Coordinator)
        c.directory, c.state, c.data = self.directory, {}, {}
        c.save = Mock()
        c.run_blocks = Mock(side_effect=RuntimeError("simulated driver failure"))
        with patch.object(comparison.trial, "notify"):
            with self.assertRaisesRegex(RuntimeError, "simulated driver failure"):
                c.run()
        self.await_release()
        self.assertIsNone(c.guard)
        with comparison.SessionGuard({}, self.directory) as guard:
            # No real delay or live desktop operation needed to exercise expiry.
            with patch.object(comparison.time, "monotonic", return_value=guard.last_heartbeat + 10):
                with self.assertRaisesRegex(RuntimeError, "stopped responding"):
                    guard.check()
        self.await_release()

    def test_check_is_read_only_and_missing_service_fails(self):
        command = ["/usr/bin/python3", comparison.HERE / "comparison-monitor.py", "check"]
        result = subprocess.run(command, env=self.env, capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["event"], "checked")
        self.assertEqual(self.control.Cookies(), 0)
        self.server.terminate()
        self.server.wait(timeout=3)
        result = subprocess.run(command, env=self.env, capture_output=True, text=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    if sys.argv[1:] == ["--fake-screensaver"]:
        fake_screensaver()
    else:
        unittest.main()
