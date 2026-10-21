"""Comparison policy and replay integration tests; never touch real outputs or drivers."""
import importlib.util
import copy
from contextlib import ExitStack
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("comparison", HERE / "compare-live.py")
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)
trial = comparison.trial


class MeasurementTests(unittest.TestCase):
    def test_power_samples_include_optional_device_activity_and_clock(self):
        with tempfile.TemporaryDirectory() as directory:
            device = Path(directory)
            hwmon = device / "hwmon/hwmon0"
            hwmon.mkdir(parents=True)
            (hwmon / "power1_average").write_text("18000000\n")
            (hwmon / "temp1_input").write_text("55000\n")
            sample = comparison.power_sample(dict(path=str(hwmon)))
            self.assertEqual(sample["watts"], 18)
            self.assertEqual(sample["temperature_c"], 55)
            self.assertIsNone(sample["gpu_clock_mhz"])
            self.assertIsNone(sample["gpu_busy_percent"])
            (hwmon / "freq1_input").write_text("800000000\n")
            (device / "gpu_busy_percent").write_text("12\n")
            sample = comparison.power_sample(dict(path=str(hwmon)))
            self.assertEqual(sample["gpu_clock_mhz"], 800)
            self.assertEqual(sample["gpu_busy_percent"], 12)
            (hwmon / "freq1_input").write_text("unavailable\n")
            self.assertIsNone(comparison.power_sample(dict(path=str(hwmon)))["gpu_clock_mhz"])

    def test_driver_order_is_balanced(self):
        blocks = comparison.plan(2)
        self.assertEqual([b["backend"] for b in blocks], ["official", "gpu", "gpu", "official"])
        for index in (0, 2):
            self.assertEqual(blocks[index]["cases"], blocks[index + 1]["cases"])
        self.assertNotEqual(blocks[0]["cases"], blocks[2]["cases"])

    def test_evdi_comparison_separates_encoder_and_copy_effects_in_reverse_order(self):
        blocks = comparison.plan(2, 128)
        self.assertEqual([b["condition"] for b in blocks], [
            "official-evdi-0", "gpu-evdi-0", "gpu-evdi-128",
            "gpu-evdi-128", "gpu-evdi-0", "official-evdi-0"])
        self.assertEqual([b["evdi_copy_rows"] for b in blocks], [0, 0, 128, 128, 0, 0])
        self.assertEqual([b["cases"] for b in blocks[:3]], [list(comparison.CASES)] * 3)
        self.assertEqual([b["cases"] for b in blocks[3:]], [list(comparison.CASES[1:] + comparison.CASES[:1])] * 3)
        settings = dict(pairs=2, warmup=15, seconds=30, settle=15)
        self.assertEqual(comparison.runtime_bound(settings), 990)
        self.assertEqual(comparison.runtime_bound({**settings, "compare_evdi_rows": 128}), 1480)

    def test_cpu_identity_and_counter_reset_are_rejected(self):
        before = dict(pid=1, start_ticks=10, cpu_seconds=2)
        for after in (dict(pid=2, start_ticks=10, cpu_seconds=3),
                      dict(pid=1, start_ticks=11, cpu_seconds=3),
                      dict(pid=1, start_ticks=10, cpu_seconds=1)):
            with self.assertRaises(RuntimeError):
                comparison.cpu_delta(before, after, 1)
        self.assertAlmostEqual(comparison.cpu_delta(before, {**before, "cpu_seconds": 3}, 2), .5)

    def test_proc_parser_accepts_spaces_and_parentheses_in_name(self):
        fields = ["S"] + ["0"] * 49
        fields[11], fields[12], fields[19] = "20", "30", "123"
        with patch.object(Path, "read_text", return_value="42 (a ) name) " + " ".join(fields)):
            result = comparison.process_counter(42)
        self.assertEqual(result["start_ticks"], 123)
        self.assertEqual(result["cpu_seconds"], 50 / comparison.TICKS)

    def test_power_integrates_time_instead_of_averaging_irregular_samples(self):
        samples = [dict(monotonic_s=t, watts=w, temperature_c=c)
                   for t, w, c in ((0, 10, 50), (1, 20, 52), (4, 20, 53))]
        result = comparison.power_summary(samples)
        self.assertAlmostEqual(result["estimated_joules"], 75)
        self.assertAlmostEqual(result["mean_watts"], 18.75)
        self.assertEqual(result["max_gap_seconds"], 3)
        with self.assertRaises(RuntimeError):
            comparison.power_summary(list(reversed(samples)))

    def test_single_monitor_geometry_uses_edid_and_disables_benq(self):
        panel = dict(name="eDP", connected=True, mode="1920x1200", edid="panel", x=960, y=1080, primary=True, rate="60.00")
        dell = dict(name="DVI-old", connected=True, mode="1920x1080", edid="dell", x=1920, y=0, primary=False, rate="60.00")
        benq = dict(dell, name="DVI-benq", edid="benq", x=0)
        state = dict(panel=panel, target=dell)
        wanted, args = comparison.target_layout(state, [panel, dict(dell, name="DVI-new"), benq])
        self.assertEqual([(o["x"], o["y"]) for o in wanted], [(0, 0), (1920, 0)])
        self.assertIn("DVI-new", args)
        self.assertEqual(args[-3:], ["--output", "DVI-benq", "--off"])
        with patch.object(trial, "outputs", return_value=[panel, dell, benq]):
            with self.assertRaisesRegex(RuntimeError, "only the laptop and Dell"):
                comparison.verify_layout(state)

    def test_worker_cannot_choose_a_different_physical_monitor(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            trial.save(directory / "state.json", dict(expected_monitor_edid="dell"))
            (directory / "events.jsonl").write_text('{"event":"monitor","edid":"benq"}\n')
            with patch.object(trial, "outputs") as outputs:
                with self.assertRaisesRegex(RuntimeError, "different monitor"):
                    trial.configure_layout(directory)
                outputs.assert_not_called()

    def test_entire_comparison_has_recovery_and_group_kill(self):
        args = comparison.unit_command(Path("/var/tmp/vino-example"), 990)
        self.assertTrue(any(str(a).startswith("ExecStopPost=") for a in args))
        self.assertFalse(any(str(a).startswith("ExecStartPre=") for a in args))
        for value in ("RuntimeMaxSec=990s", "WatchdogSec=30s", "KillMode=control-group", "TimeoutStopSec=2s"):
            self.assertIn(value, args)

    def test_recovery_preserves_searchable_comparison_directories_and_root_code(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            (directory / "payload").mkdir()
            (directory / "comparison.py").touch(mode=0o700)
            state = dict(mock=False, takeover=False, mask_added=False, dock="mock", bindings={}, uid=os.getuid(), gid=os.getgid())
            trial.save(directory / "state.json", state)
            with patch.object(trial, "interface_bindings", return_value={}), patch.object(trial.os, "chown"):
                trial.recover(directory)
            self.assertTrue((directory / "payload").stat().st_mode & 0o111)
            self.assertEqual((directory / "comparison.py").stat().st_mode & 0o777, 0o700)


class EvdiConditionTests(unittest.TestCase):
    def test_wrong_module_or_wrong_batch_size_invalidates_the_measurement(self):
        state = dict(evdi_module=dict(srcversion="candidate", copy_batch_rows=0),
                     evdi_copy=dict(srcversion="candidate", previous=0, rows=128))
        for version, rows, valid in (("candidate", 128, True), ("candidate", 0, False), ("other", 128, False)):
            with self.subTest(version=version, rows=rows), \
                 patch.object(comparison, "evdi_state", return_value=dict(srcversion=version, copy_batch_rows=rows)):
                if valid:
                    self.assertEqual(comparison.verify_evdi(state)["copy_batch_rows"], 128)
                else:
                    with self.assertRaisesRegex(RuntimeError, "EVDI configuration changed"):
                        comparison.verify_evdi(state)

    def test_original_module_without_experimental_parameter_is_supported(self):
        identity = dict(srcversion="original", copy_batch_rows=None)
        with patch.object(comparison, "evdi_state", return_value=identity):
            self.assertEqual(comparison.verify_evdi(dict(evdi_module=identity)), identity)

    def coordinator(self, directory):
        coordinator = comparison.Coordinator.__new__(comparison.Coordinator)
        coordinator.directory = directory
        coordinator.state = dict(evdi_module=dict(srcversion="candidate", copy_batch_rows=0),
                                 evdi_copy=dict(srcversion="candidate", previous=0, rows=128),
                                 evdi_copy_restore_pending=True, takeover=True, mask_added=False)
        coordinator.settings = dict(dock="mock", head=0, card=0, device=0)
        coordinator.stop_gpu = Mock()
        coordinator.tick = Mock()
        coordinator.pause = Mock()
        return coordinator

    def test_official_service_starts_only_after_recorded_batch_reset(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            coordinator = self.coordinator(directory)
            operations = []

            def set_rows(configuration, rows):
                saved = json.loads((directory / "state.json").read_text())
                self.assertTrue(saved["evdi_copy_restore_pending"])
                self.assertEqual(saved["evdi_copy"]["rows"], 0)
                self.assertEqual(rows, 0)
                operations.append("reset")

            def command(arguments, **kwargs):
                if arguments[:2] == ["systemctl", "start"]:
                    self.assertEqual(operations, ["reset", "verified"])
                    operations.append("official-start")
                return Mock(stdout="")

            with patch.object(trial, "set_evdi_copy_rows", side_effect=set_rows), \
                 patch.object(comparison, "verify_evdi", side_effect=lambda state: operations.append("verified")), \
                 patch.object(Path, "exists", return_value=False), patch.object(Path, "is_symlink", return_value=False), \
                 patch.object(trial, "command", side_effect=command), patch.object(trial, "outputs", return_value=[]), \
                 patch.object(trial, "desktop_command"), patch.object(comparison, "target_layout", return_value=([], [])), \
                 patch.object(comparison, "verify_layout"), patch.object(comparison, "driver_pid", return_value=123):
                self.assertEqual(coordinator.official(), 123)
            self.assertEqual(operations, ["reset", "verified", "official-start"])

    def test_failed_reset_cannot_start_an_official_measurement(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            coordinator = self.coordinator(directory)
            with patch.object(trial, "set_evdi_copy_rows", side_effect=RuntimeError("reset failed")), \
                 patch.object(trial, "command") as command:
                with self.assertRaisesRegex(RuntimeError, "reset failed"):
                    coordinator.official()
            command.assert_not_called()
            saved = json.loads((directory / "state.json").read_text())
            self.assertTrue(saved["evdi_copy_restore_pending"])
            self.assertEqual(saved["evdi_copy"]["previous"], 0)

    def test_each_gpu_condition_reaches_the_supervised_prepare_path(self):
        for rows in (0, 128):
            with self.subTest(rows=rows), tempfile.TemporaryDirectory() as name:
                directory = Path(name)
                coordinator = self.coordinator(directory)
                coordinator.state["evdi_copy"]["rows"] = 0

                def prepare(path):
                    self.assertEqual(path, directory)
                    saved = json.loads((path / "state.json").read_text())
                    self.assertEqual(saved["evdi_copy"]["rows"], rows)
                    self.assertEqual(saved["evdi_copy"]["previous"], 0)
                    raise RuntimeError("stop before any real worker or device access")

                with patch.object(comparison, "verify_evdi"), patch.object(trial, "prepare", side_effect=prepare):
                    with self.assertRaisesRegex(RuntimeError, "stop before"):
                        coordinator.start_gpu(directory / "block", rows)


class CaptureSelectionTests(unittest.TestCase):
    def arguments(self, *extra):
        with patch.object(comparison.sys, "argv", ["compare-live.py", "prepare", *extra]), \
             patch.object(comparison, "prepare_inputs") as prepare:
            comparison.main()
        return prepare.call_args.args[0]

    def checked_state(self, directory, capture):
        args = self.arguments("--capture", capture, "--gpu-evdi-rows", "128")
        args.inputs = directory
        args.binary = directory / "worker"
        manifest = dict(files={name: "digest" for name in (
            "live-replay", "idle.bgrx", "scroll.bgrx", "motion.bgrx")},
            gpu_binary_sha256="digest", player_source_sha256="digest")
        trial.save(directory / "manifest.json", manifest)
        state = dict(panel=dict(name="eDP", mode="1920x1200", rate="60"),
                     outputs=[dict(name="Dell", connected=True, edid="00ffffffffffff0010ac")],
                     evdi_copy=dict(rows=0, previous=0, srcversion="candidate"))
        with ExitStack() as stack:
            for owner, name, value in (
                    (comparison, "digest", "digest"),
                    (trial, "snapshot", state),
                    (trial, "session_command", Mock(stdout='{"checked":true}')),
                    (comparison, "evdi_state", dict(srcversion="candidate", copy_batch_rows=0)),
                    (comparison, "one_pid", 123),
                    (comparison, "process_counter", dict(pid=123, start_ticks=1, cpu_seconds=0)),
                    (trial, "compositor_active", True),
                    (comparison, "power_source", {"AC": "1"}),
                    (comparison, "discover_sensor", {"path": "mock"}),
                    (comparison, "driver_pid", 123),
                    (comparison, "service_cpu", 0)):
                stack.enter_context(patch.object(owner, name, return_value=value))
            command = stack.enter_context(patch.object(trial, "command", return_value=Mock(stdout="mock")))
            state = comparison.preflight(args)
        check = next(call.args[0] for call in command.call_args_list if "--check" in call.args[0])
        return state, list(map(str, check[2:]))

    def start_arguments(self, directory, state):
        block = directory / "block"
        block.mkdir()
        trial.save(directory / "state.json", state)
        (directory / "helper.py").write_text("# test helper\n")
        coordinator = comparison.Coordinator.__new__(comparison.Coordinator)
        coordinator.directory = directory
        coordinator.state = state
        coordinator.settings = state["settings"]
        coordinator.stop_gpu = Mock()
        coordinator.pause = Mock(side_effect=lambda _: setattr(coordinator, "gpu_ready", True))

        def prepare(path):
            self.assertEqual(path, directory)
            saved = json.loads((path / "state.json").read_text())
            self.assertEqual(saved["settings"], state["settings"])
            self.assertEqual(saved["evdi_copy"]["rows"], 128)

        with patch.object(comparison, "verify_evdi"), patch.object(trial, "prepare", side_effect=prepare), \
             patch.object(comparison.socket, "socket"), patch.object(comparison, "verify_layout"), \
             patch.object(comparison.subprocess, "Popen", return_value=Mock(pid=456)) as popen:
            self.assertEqual(coordinator.start_gpu(block, 128), 456)
        return popen.call_args.args[0], json.loads((block / "state.json").read_text())

    def test_capture_selection_reaches_hardware_check_and_worker_after_state_reload(self):
        for capture, cursor in (("copied", "evdi"), ("direct", "client")):
            with self.subTest(capture=capture), tempfile.TemporaryDirectory() as name:
                directory = Path(name)
                state, checked = self.checked_state(directory, capture)
                self.assertEqual(state["settings"]["capture"], capture)
                self.assertEqual(state["settings"]["cursor"], cursor)
                arguments, saved = self.start_arguments(directory, state)
                self.assertEqual(arguments[1:1 + len(checked)], checked)
                self.assertEqual(saved["settings"]["capture"], capture)
                self.assertEqual(saved["settings"]["cursor"], cursor)
                if capture == "direct":
                    self.assertEqual(checked[checked.index("--capture") + 1], "direct")
                    self.assertEqual(checked[checked.index("--cursor") + 1], "client")
                else:
                    for flag in ("--capture", "--cursor"):
                        self.assertNotIn(flag, checked)
                        self.assertNotIn(flag, arguments)

    def test_default_and_archived_settings_keep_legacy_worker_arguments(self):
        self.assertEqual(self.arguments().capture, "copied")
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            state, _ = self.checked_state(directory, "copied")
            del state["settings"]["capture"]
            del state["settings"]["cursor"]
            arguments, _ = self.start_arguments(directory, state)
            self.assertNotIn("--capture", arguments)
            self.assertNotIn("--cursor", arguments)

    def test_direct_head_one_is_rejected_before_checks_or_takeover(self):
        args = self.arguments("--capture", "direct")
        args.head = 1
        with patch.object(trial, "snapshot") as snapshot:
            with self.assertRaisesRegex(ValueError, "requires --head 0"):
                comparison.preflight(args)
            snapshot.assert_not_called()
        coordinator = comparison.Coordinator.__new__(comparison.Coordinator)
        coordinator.settings = vars(args)
        coordinator.stop_gpu = Mock()
        with patch.object(trial, "prepare") as prepare:
            with self.assertRaisesRegex(ValueError, "requires --head 0"):
                coordinator.start_gpu(Path("/unused"))
            prepare.assert_not_called()
            coordinator.stop_gpu.assert_not_called()
        with patch.object(comparison.sys, "argv", [
                "compare-live.py", "run", "--capture", "direct", "--head", "1"]), \
             patch.object(comparison, "launch") as launch, patch.object(comparison.sys, "stderr", io.StringIO()):
            with self.assertRaises(SystemExit) as error:
                comparison.main()
            self.assertEqual(error.exception.code, 2)
            launch.assert_not_called()


class PowerSupplyTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="vino-power-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.supply("AC", "Mains", "1")
        self.supply("BAT0", "Battery", "1", "System")
        self.supply("ucsi-source-psy-USBC000:001", "USB", "0", "System")
        self.supply("ucsi-source-psy-USBC000:002", "USB", "0", "System")
        self.supply("hidpp_battery_0", "Battery", "0", "Device")
        self.supply("hidpp_battery_1", "Battery", "1", "Device")

    def supply(self, name, kind, online, scope=None):
        path = self.root / name
        path.mkdir(exist_ok=True)
        for field, value in (("type", kind), ("online", online), ("scope", scope)):
            if value is not None:
                (path / field).write_text(value + "\n")
        return path

    def test_laptop_inputs_exclude_internal_and_peripheral_batteries(self):
        sources = comparison.power_source(self.root)
        self.assertEqual(sources, {str(self.root / "AC/online"): "1",
                                  str(self.root / "ucsi-source-psy-USBC000:001/online"): "0",
                                  str(self.root / "ucsi-source-psy-USBC000:002/online"): "0"})

    def test_mouse_sleep_wake_and_disappearance_do_not_change_laptop_power(self):
        expected = comparison.power_source(self.root)
        for online in ("0", "1"):
            (self.root / "hidpp_battery_1/online").write_text(online)
            actual = comparison.power_source(self.root)
            with patch.object(comparison, "power_source", return_value=actual):
                self.assertEqual(comparison.verify_power_source(expected), expected)
        shutil.rmtree(self.root / "hidpp_battery_1")
        self.assertEqual(comparison.power_source(self.root), expected)

    def test_batteries_without_scope_and_device_chargers_are_not_laptop_inputs(self):
        expected = comparison.power_source(self.root)
        self.supply("legacy-mouse", "Battery", "1")
        self.supply("peripheral-charger", "USB", "1", "Device")
        self.supply("peripheral-mains", "Mains", "1", "Device")
        self.assertEqual(comparison.power_source(self.root), expected)

    def test_real_ac_disconnect_identifies_the_exact_changed_reading(self):
        expected = comparison.power_source(self.root)
        (self.root / "AC/online").write_text("0")
        actual = comparison.power_source(self.root)
        with patch.object(comparison, "power_source", return_value=actual):
            with self.assertRaisesRegex(RuntimeError, "AC/online: 1 -> 0"):
                comparison.verify_power_source(expected)

    def test_system_usb_input_changes_and_missing_inputs_are_detected(self):
        expected = comparison.power_source(self.root)
        path = self.root / "ucsi-source-psy-USBC000:001/online"
        path.write_text("1")
        with patch.object(comparison, "power_source", return_value=comparison.power_source(self.root)):
            with self.assertRaisesRegex(RuntimeError, "USBC000:001/online: 0 -> 1"):
                comparison.verify_power_source(expected)
        path.unlink()
        with patch.object(comparison, "power_source", return_value=comparison.power_source(self.root)):
            with self.assertRaisesRegex(RuntimeError, "USBC000:001/online: 0 -> missing"):
                comparison.verify_power_source(expected)


class ReplayAssessmentTests(unittest.TestCase):
    # Reduced timing/CPU record from the official scrolling run that originally
    # aborted: /var/tmp/vino-compare-q7x1wcbn. No live machine paths are read here.
    SCROLL = dict(seconds=30.000065244, frames_submitted=1800, static_hold=False,
                  input_frames_per_second=59.99987, input_deadline_misses=117,
                  x_submission_p99_ms=21.739839, max_input_gap_ms=43.096527,
                  cpu_interval_seconds=30.00123154814355, driver_cpu_cores=.871328763889273,
                  display_stack_cpu_cores=1.0786535862059772, system_busy_cpu_cores=1.9452534775675414,
                  power=dict(max_gap_seconds=.2019719099625945, mean_watts=22.865007692302623))

    def coordinator(self, directory):
        coordinator = comparison.Coordinator.__new__(comparison.Coordinator)
        coordinator.directory = directory
        coordinator.settings = dict(seconds=30)
        coordinator.data = dict(complete=False, runs=[])
        return coordinator

    def test_observed_deadline_misses_continue_and_remain_visible_in_summary(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            coordinator = self.coordinator(directory)
            for backend in ("official", "gpu"):
                # The policy applies identically to either backend; these are the
                # same captured timings, not a claimed GPU measurement.
                result = copy.deepcopy(self.SCROLL)
                coordinator.record_measurement(directory, dict(backend=backend, repeat=0), "scroll", result, [])
                self.assertFalse(result["input_pacing_ok"])
                self.assertTrue(result["sampling_ok"])
                self.assertEqual(result["measurement_errors"], [])
                self.assertEqual(result["input_deadline_miss_percent"], 6.5)
                self.assertIn("117/1800", result["input_pacing_warnings"][0])
            rows = json.loads((directory / "measurements.json").read_text())["runs"]
            summary = comparison.summarize(rows)
            self.assertEqual(len(rows), 2)
            self.assertEqual(len(summary), 2)
            for entry in summary:
                self.assertFalse(entry["input_pacing_ok"])
                self.assertEqual(entry["input_pacing_warning_runs"], 1)
                self.assertAlmostEqual(entry["metrics"]["input_deadline_miss_percent"]["mean"], 6.5)
                self.assertAlmostEqual(entry["metrics"]["input_frames_per_second"]["mean"], 59.99987)

    def test_sampling_failures_still_abort_with_the_specific_reason(self):
        for field, value, reason in (("cpu_interval_seconds", 30.2, "CPU sampling interval differs"),
                                     ("power", dict(max_gap_seconds=1.2, mean_watts=22), "power sample gap 1.200s")):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as name:
                directory = Path(name)
                coordinator = self.coordinator(directory)
                result = {**copy.deepcopy(self.SCROLL), field: value}
                with self.assertRaisesRegex(RuntimeError, f"scroll-official: invalid measurement: {reason}"):
                    coordinator.record_measurement(directory, dict(backend="official"), "scroll", result, [])
                report = json.loads((directory / "measurements.json").read_text())
                self.assertFalse(report["complete"])
                self.assertFalse(report["runs"][0]["measurement"]["sampling_ok"])

    def test_missing_images_and_wrong_workload_still_abort(self):
        for field, value, reason in (("frames_submitted", 1799, "submitted 1799 images; expected 1800"),
                                     ("static_hold", True, "static/active mode did not match")):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as name:
                directory = Path(name)
                coordinator = self.coordinator(directory)
                result = {**copy.deepcopy(self.SCROLL), field: value}
                with self.assertRaisesRegex(RuntimeError, reason):
                    coordinator.record_measurement(directory, dict(backend="gpu"), "scroll", result, [])

    def test_slow_replay_is_recorded_with_warnings_instead_of_claiming_matched_pacing(self):
        result = {**copy.deepcopy(self.SCROLL), "seconds": 31, "cpu_interval_seconds": 31.001,
                  "max_input_gap_ms": 120, "input_frames_per_second": 1800 / 31}
        assessment = comparison.assess_measurement(result, 30, static=False)
        self.assertFalse(assessment["input_pacing_ok"])
        self.assertTrue(assessment["sampling_ok"])
        self.assertEqual(assessment["measurement_errors"], [])
        self.assertEqual(len(assessment["input_pacing_warnings"]), 3)

    def test_static_hold_has_no_submissions_and_no_pacing_warnings(self):
        result = {**copy.deepcopy(self.SCROLL), "frames_submitted": 0, "static_hold": True,
                  "input_frames_per_second": 0, "input_deadline_misses": 0}
        assessment = comparison.assess_measurement(result, 30, static=True)
        self.assertTrue(assessment["input_pacing_ok"])
        self.assertTrue(assessment["sampling_ok"])
        self.assertEqual(assessment["measurement_errors"], [])

    def test_twenty_fps_uses_six_hundred_inputs_and_its_own_warning_denominator(self):
        result = {**copy.deepcopy(self.SCROLL), "frames_submitted": 600,
                  "input_frames_per_second": 20, "input_deadline_misses": 12, "max_input_gap_ms": 55}
        assessment = comparison.assess_measurement(result, 30, static=False, input_fps=20)
        self.assertEqual(assessment["measurement_errors"], [])
        self.assertEqual(assessment["input_deadline_miss_percent"], 2)
        self.assertIn("12/600", assessment["input_pacing_warnings"][0])
        result["frames_submitted"] = 599
        self.assertIn("expected 600", comparison.assess_measurement(
            result, 30, static=False, input_fps=20)["measurement_errors"][0])

    def test_normal_and_batched_gpu_results_are_never_averaged_together(self):
        measurements = []
        for condition, rows, cpu in (("official-evdi-0", 0, .8), ("gpu-evdi-0", 0, .4), ("gpu-evdi-128", 128, .3)):
            result = copy.deepcopy(self.SCROLL)
            result["driver_cpu_cores"] = cpu
            result.update(comparison.assess_measurement(result, 30, static=False))
            measurements.append(dict(case="scroll", condition=condition,
                                     backend="official" if condition.startswith("official") else "gpu",
                                     evdi_copy_rows=rows, measurement=result))
        summary = comparison.summarize(measurements)
        self.assertEqual([row["repetitions"] for row in summary], [1, 1, 1])
        self.assertEqual([row["metrics"]["driver_cpu_cores"]["mean"] for row in summary], [.8, .4, .3])
        self.assertEqual([row["evdi_copy_rows"] for row in summary], [0, 0, 128])

    def test_retransmissions_do_not_hide_insufficient_changed_update_throughput(self):
        events = [dict(event="frame", monotonic_ns=1_000_000_000 + i * 10_000_000,
                       changed_strips=1 if i < 10 else 0, presentations=1, bytes=100)
                  for i in range(60)]
        measured = dict(start_monotonic_s=1, end_monotonic_s=2, seconds=1, frames_submitted=20)
        result = comparison.vino_throughput(events, measured)
        self.assertEqual(result["presentations"], 60)
        self.assertEqual(result["changed_updates"], 10)
        self.assertIn("10 changed updates for 20", result["input_rate_warning"])
        self.assertIn("not monitor FPS", result["note"])
        self.assertIsNone(comparison.vino_throughput(events, {**measured, "frames_submitted": 10})["input_rate_warning"])
        self.assertIsNone(comparison.vino_throughput([], {**measured, "frames_submitted": 0})["input_rate_warning"])

    def test_failed_post_replay_check_preserves_raw_measurements_without_qualifying_them(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            coordinator = self.coordinator(directory)
            coordinator.state = {}
            result = copy.deepcopy(self.SCROLL)
            result["external_power_before"] = {"/sys/class/power_supply/AC/online": "1"}
            samples = [dict(monotonic_s=1, watts=20)]
            error = RuntimeError("system external-power readings changed: AC/online: 1 -> 0")
            with patch.object(comparison, "verify_environment", side_effect=error):
                with self.assertRaisesRegex(RuntimeError, "scroll-gpu: system external-power readings changed"):
                    coordinator.check_replay_environment(directory, dict(backend="gpu"), "scroll", 123, result, samples)
            saved = json.loads((directory / "scroll-raw-measurement.json").read_text())
            self.assertFalse(saved["environment_checked"])
            self.assertIn("AC/online: 1 -> 0", saved["environment_error"])
            self.assertEqual(saved["measurement"]["frames_submitted"], 1800)
            self.assertEqual(json.loads((directory / "scroll-power.json").read_text()), samples)
            self.assertEqual(coordinator.data["runs"], [])

    def test_passed_post_replay_check_records_the_external_power_readings(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            coordinator = self.coordinator(directory)
            coordinator.state = {}
            result = copy.deepcopy(self.SCROLL)
            source = {"/sys/class/power_supply/AC/online": "1"}
            with patch.object(comparison, "verify_environment", return_value=source), \
                 patch.object(comparison, "driver_pid", return_value=123):
                coordinator.check_replay_environment(directory, dict(backend="official"), "scroll", 123, result, [])
            saved = json.loads((directory / "scroll-raw-measurement.json").read_text())
            self.assertTrue(saved["environment_checked"])
            self.assertEqual(saved["measurement"]["external_power_after"], source)

    def test_terminal_receives_every_status_even_if_updates_arrive_between_polls(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            coordinator = self.coordinator(directory)
            path = directory / "progress.jsonl"
            self.assertEqual(comparison.progress_messages(path, 0), ([], 0))
            expected = ["result", "TIMING WARNING: 117/1800", "next sample"]
            for message in expected:
                coordinator.status(message)
            messages, offset = comparison.progress_messages(path, 0)
            self.assertEqual(messages, expected)
            self.assertEqual(comparison.progress_messages(path, offset), ([], offset))
            with path.open("ab") as stream:
                stream.write(b'{"message":"partial')
            self.assertEqual(comparison.progress_messages(path, offset), ([], offset))
            with path.open("ab") as stream:
                stream.write(b' update"}\n')
            messages, new_offset = comparison.progress_messages(path, offset)
            self.assertEqual(messages, ["partial update"])
            self.assertGreater(new_offset, offset)


@unittest.skipUnless(shutil.which("Xvfb") and shutil.which("cc"), "Xvfb and C compiler required")
class ReplayTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="vino-replay-test-")
        cls.directory = Path(cls.temporary.name)
        cls.binary = cls.directory / "live-replay"
        subprocess.run(["cc", "-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", HERE / "live-replay.c",
                        "-o", cls.binary, "-lX11", "-lXext", "-lm"], check=True, capture_output=True)
        cls.xvfb = subprocess.Popen(["Xvfb", "-displayfd", "1", "-screen", "0", "128x96x24", "-nolisten", "tcp"],
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        import select
        if not select.select([cls.xvfb.stdout], [], [], 5)[0]:
            cls.xvfb.kill()
            raise RuntimeError("virtual X server did not start")
        display = cls.xvfb.stdout.readline().strip()
        if not display.isdigit():
            raise RuntimeError("virtual X server startup failed")
        cls.env = {**os.environ, "DISPLAY": ":" + display, "XAUTHORITY": str(cls.directory / "unused-auth")}
        cls.corpus = cls.directory / "corpus.bgrx"
        cls.corpus.write_bytes(b"".join(bytes((frame, 30 + head, 70, 0)) * (64 * 64)
                                       for frame in range(16) for head in range(2)))

    @classmethod
    def tearDownClass(cls):
        cls.xvfb.terminate()
        cls.xvfb.communicate(timeout=5)
        cls.temporary.cleanup()

    def player(self, static, rate=10):
        process = subprocess.Popen([self.binary, self.corpus, "64", "64", str(rate), ".2", "1", "0", "0",
                                    str(int(static)), "virtual test", "checked-layout"], env=self.env,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        return process, comparison.JsonReader(process)

    def test_dynamic_and_static_have_valid_gated_measurements(self):
        for static, rate in ((False, 10), (True, 10), (False, 20), (True, 20)):
            with self.subTest(static=static, rate=rate):
                process, reader = self.player(static, rate)
                try:
                    ready = reader.event("ready", 5, lambda: None)
                    self.assertEqual(ready["pixel_checks_passed"], 16)
                    self.assertEqual(ready["pid"], process.pid)
                    process.stdin.write(b"g")
                    reader.event("warm", 3, lambda: None)
                    before = comparison.process_counter(process.pid)
                    process.stdin.write(b"g")
                    result = reader.event("done", 3, lambda: None)
                    after = comparison.process_counter(process.pid)
                    self.assertIsNone(process.poll(), "player must remain alive for final CPU sampling")
                    self.assertGreaterEqual(comparison.cpu_delta(before, after, result["seconds"]), 0)
                    self.assertEqual(result["frames_submitted"], 0 if static else rate)
                    self.assertEqual(result["static_hold"], static)
                    self.assertAlmostEqual(result["seconds"], 1, delta=.1)
                    process.stdin.write(b"g")
                    self.assertEqual(process.wait(timeout=3), 0)
                finally:
                    comparison.stop_process(process)
                    reader.close()
                    process.stderr.close()

    def test_interrupted_replay_is_not_a_valid_done_measurement(self):
        process, reader = self.player(False)
        try:
            reader.event("ready", 5, lambda: None)
            process.stdin.write(b"g")
            reader.event("warm", 3, lambda: None)
            process.stdin.write(b"g")
            time.sleep(.1)
            process.terminate()
            with self.assertRaisesRegex(RuntimeError, "exited before done"):
                reader.event("done", 3, lambda: None)
            self.assertNotEqual(process.wait(timeout=3), 0)
        finally:
            comparison.stop_process(process)
            reader.close()
            process.stderr.close()

    def test_coordinator_records_telemetry_with_real_gated_replay(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            sleepers = [subprocess.Popen(["sleep", "10"]) for _ in range(3)]
            for process in sleepers:
                self.addCleanup(comparison.stop_process, process)
            hwmon = directory / "hwmon"
            hwmon.mkdir()
            (hwmon / "power1_average").write_text("12000000")
            (hwmon / "temp1_input").write_text("50000")
            (hwmon / "fan1_input").write_text("2200")
            c = comparison.Coordinator.__new__(comparison.Coordinator)
            c.directory = directory
            c.settings = dict(warmup=.2, seconds=1, input_fps=20, capture="direct")
            c.state = dict(panel=dict(mode="128x96"), processes={
                "Xorg": comparison.process_counter(self.xvfb.pid),
                "kwin_x11": comparison.process_counter(sleepers[0].pid)},
                sensor=dict(path=str(hwmon), thermal_sensors=[
                    dict(id="fan", path=str(hwmon / "fan1_input"), scale=1)]))
            c.data = dict(complete=False, runs=[])
            c.guard = Mock(ready=dict(pid=sleepers[2].pid))
            c.guard.confirm.return_value = dict(inhibited=True)
            c.tick = Mock()

            def invocation(state, args):
                args[0:4] = [str(self.binary), str(self.corpus), "64", "64"]
                args[7] = "0"  # Fit the test window on the private Xvfb display.
                return args, self.env

            with patch.object(comparison, "verify_environment", return_value={}), \
                 patch.object(comparison, "verify_evdi", return_value={}), \
                 patch.object(trial, "session_invocation", side_effect=invocation):
                c.replay(directory, dict(backend="gpu", repeat=0), "scroll", sleepers[1].pid)
            report = json.loads((directory / "measurements.json").read_text())["runs"][0]["measurement"]
            self.assertEqual(report["frames_submitted"], 20)
            self.assertEqual(report["capture_storage"], "direct")
            self.assertEqual(report["cursor_mode"], "client")
            self.assertTrue(report["sampling_ok"])
            self.assertTrue(report["screen_lock"]["inhibited"])
            self.assertEqual(report["power"]["thermal"]["fan"]["mean"], 2200)
            self.assertGreaterEqual(report["process_attribution"]["sample_count"], 2)
            self.assertIn("cgroups", report["counters_before"])
            self.assertIn("cgroups", report["counters_after"])
            self.assertGreater(report["cgroup_attribution"]["seconds"], 0)
            self.assertGreaterEqual(report["cpu_cores"]["sampler"], 0)
            raw = json.loads((directory / "scroll-raw-measurement.json").read_text())
            self.assertTrue(raw["environment_checked"])
            self.assertEqual(raw["measurement"]["capture_storage"], "direct")
            self.assertEqual(raw["measurement"]["cursor_mode"], "client")
            self.assertTrue((directory / "scroll-processes.json").is_file())


if __name__ == "__main__":
    unittest.main()
