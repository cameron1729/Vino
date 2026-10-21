"""Layout/recovery policy tests. These never call xrandr or systemctl."""
import importlib.util
import json
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("live_trial", Path(__file__).with_name("live-trial.py"))
trial = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(trial)

XRANDR = """Screen 0: current 3840 x 2280
eDP connected primary 1920x1200+960+1080 (normal left inverted right x axis y axis)
    EDID:
        00ffffffffffff0030ae3d4000000000
    1920x1200 60.00*+
DVI-I-1-1 connected 1920x1080+0+0 (normal left inverted right x axis y axis)
    EDID:
        00ffffffffffff000001020304050607
    1920x1080 60.00*+ 50.00 59.94
DVI-I-2-2 connected 1920x1080+1920+0 (normal left inverted right x axis y axis)
    EDID:
        00ffffffffffff000708090a0b0c0d0e
    1920x1080 60.00*+
HDMI-A-0 disconnected (normal left inverted right x axis y axis)
"""


class LayoutTests(unittest.TestCase):
    def test_snapshot_preserves_positions_refresh_and_primary(self):
        outputs = trial.parse_outputs(XRANDR)
        self.assertEqual((outputs[0]["x"], outputs[0]["y"], outputs[0]["rate"]), (960, 1080, "60.00"))
        self.assertTrue(outputs[0]["primary"])
        self.assertFalse(outputs[1]["primary"])
        self.assertFalse(outputs[3]["connected"])
        self.assertEqual(outputs[1]["edid"], "00ffffffffffff000001020304050607")

    def test_restores_by_edid_when_xrandr_renumbers_outputs(self):
        saved = trial.parse_outputs(XRANDR)
        current = trial.parse_outputs(XRANDR.replace("DVI-I-1-1", "DVI-I-7-5").replace("DVI-I-2-2", "DVI-I-9-6"))
        args = trial.layout_arguments(saved, current)
        self.assertIn("DVI-I-7-5", args)
        self.assertIn("DVI-I-9-6", args)
        self.assertNotIn("DVI-I-1-1", args)
        self.assertIn("960x1080", args)
        self.assertEqual(args[-3:], ["--output", "HDMI-A-0", "--off"])

    def test_restoring_other_outputs_does_not_clear_the_primary(self):
        saved = trial.parse_outputs(XRANDR)
        args = trial.layout_arguments(saved, saved)
        self.assertEqual(args.count("--primary"), 1)
        self.assertNotIn("--noprimary", args)
        self.assertLess(args.index("--primary"), args.index("DVI-I-1-1"))

    def test_layout_without_a_primary_clears_it_once_for_the_whole_screen(self):
        saved = trial.parse_outputs(XRANDR.replace(" connected primary ", " connected "))
        args = trial.layout_arguments(saved, trial.parse_outputs(XRANDR))
        self.assertEqual(args[0], "--noprimary")
        self.assertEqual(args.count("--noprimary"), 1)
        self.assertNotIn("--primary", args)

    def test_missing_monitor_does_not_restore_to_wrong_output(self):
        saved = trial.parse_outputs(XRANDR)
        with self.assertRaises(RuntimeError):
            trial.layout_arguments(saved, saved[:2])

    def test_ambiguous_edid_with_renamed_outputs_is_rejected(self):
        saved = trial.parse_outputs(XRANDR)[1]
        duplicates = [{**saved, "name": "a"}, {**saved, "name": "b"}]
        with self.assertRaises(RuntimeError):
            trial.match_output(saved, duplicates)

    def test_rotated_layout_is_rejected_before_takeover(self):
        with self.assertRaises(RuntimeError):
            trial.parse_outputs(XRANDR.replace("1920x1080+0+0 (", "1920x1080+0+0 left ("))


class CaptureSelectionTests(unittest.TestCase):
    def options(self, **overrides):
        return SimpleNamespace(**(dict(binary=Path("/worker"), dock="8-1.2", head=0, card=0,
                                       device=0, encoder="gpu", source="desktop", cursor="client",
                                       capture="direct", duration=120, fps=60, validation=True) | overrides))

    def test_direct_capture_reaches_staged_preflight_and_worker(self):
        options = self.options()
        trial.validate_capture(options)
        for directory in (None, Path("/var/tmp/vino-live-test")):
            arguments = trial.worker_arguments(options, directory)
            self.assertEqual(arguments[arguments.index("--capture") + 1], "direct")
            self.assertEqual(arguments[arguments.index("--cursor") + 1], "client")

    def test_unsupported_direct_combinations_fail_before_launch_side_effects(self):
        for overrides in (dict(encoder="cpu"), dict(source="pattern"), dict(cursor="evdi"), dict(head=1)):
            with self.subTest(overrides=overrides), patch.object(trial.os, "geteuid") as uid:
                with self.assertRaisesRegex(ValueError, "native capture requires"):
                    trial.launch(self.options(**overrides))
                uid.assert_not_called()

    def test_existing_comparison_arguments_keep_copied_capture(self):
        options = self.options(cursor="evdi")
        del options.capture
        trial.validate_capture(options)
        arguments = trial.worker_arguments(options)
        self.assertNotIn("--capture", arguments)

    def test_shared_capture_and_gpu_comparison_reach_worker(self):
        options = self.options(capture="shared", comparison="gpu")
        trial.validate_capture(options)
        arguments = trial.worker_arguments(options)
        self.assertEqual(arguments[arguments.index("--capture") + 1], "shared")
        self.assertEqual(arguments[arguments.index("--comparison") + 1], "gpu")

    def test_shared_capture_and_gpu_comparison_reject_incompatible_modes(self):
        for overrides in (dict(capture="copied", comparison="gpu"),
                          dict(capture="shared", cursor="evdi"),
                          dict(capture="shared", evdi_copy_rows=128)):
            with self.subTest(overrides=overrides), patch.object(trial.os, "geteuid") as uid:
                with self.assertRaises(ValueError):
                    trial.launch(self.options(**overrides))
                uid.assert_not_called()

    def test_default_comparison_remains_compatible_with_older_worker(self):
        self.assertNotIn("--comparison", trial.worker_arguments(self.options(comparison="cpu")))


class MotionTraceTests(unittest.TestCase):
    def test_layout_does_not_wait_for_an_x11_diagnostic_after_enabling_evdi(self):
        current = trial.parse_outputs(XRANDR)
        state = dict(panel=current[0], trace_motion=True)
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            trial.save(directory / "state.json", state)
            (directory / "events.jsonl").write_text(json.dumps(dict(event="monitor", edid=current[1]["edid"])) + "\n")
            with patch.object(trial, "outputs", return_value=current), \
                 patch.object(trial, "desktop_command") as layout, \
                 patch.object(trial, "start_motion_trace", side_effect=AssertionError("would block capture")):
                trial.configure_layout(directory)
                layout.assert_called_once()

    def test_diagnostics_start_only_when_requested(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            for enabled in (False, True):
                state = dict(trace_motion=enabled)
                trial.save(directory / "state.json", state)
                with patch.object(trial, "start_motion_trace") as trace:
                    trial.start_diagnostics(directory)
                    if enabled:
                        trace.assert_called_once_with(directory, state)
                    else:
                        trace.assert_not_called()

    def test_initialization_output_does_not_count_as_trace_readiness(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            process = Mock()
            process.poll.return_value = None

            def launch(*args, **kwargs):
                kwargs["stdout"].write('{"event":"initializing","step":"open_display"}\n')
                kwargs["stdout"].flush()
                return process

            with patch.object(trial, "session_invocation", return_value=(["trace"], None)), \
                 patch.object(trial.subprocess, "Popen", side_effect=launch), \
                 patch.object(trial.time, "monotonic", side_effect=[0, 0, 4]), \
                 patch.object(trial.time, "sleep"):
                with self.assertRaisesRegex(RuntimeError, "last step: open_display"):
                    trial.start_motion_trace(directory, dict(duration=60))

    def test_trace_start_after_initialization_is_accepted(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            process = Mock()
            process.poll.return_value = None

            def launch(*args, **kwargs):
                kwargs["stdout"].write('{"event":"initializing","step":"subscribe"}\n{"event":"start"}\n')
                kwargs["stdout"].flush()
                return process

            with patch.object(trial, "session_invocation", return_value=(["trace"], None)), \
                 patch.object(trial.subprocess, "Popen", side_effect=launch), \
                 patch.object(trial.time, "monotonic", side_effect=[0, 0]):
                trial.start_motion_trace(directory, dict(duration=60))

    def test_worker_failure_detail_survives_a_partial_last_log_line(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            self.assertIsNone(trial.trial_error(directory))
            (directory / "events.jsonl").write_text(
                '{"event":"exit","error":"X11 trial trace-start failed: open_display timed out"}\n{"event":')
            self.assertEqual(trial.trial_error(directory), "X11 trial trace-start failed: open_display timed out")


class CpuProfileTests(unittest.TestCase):
    def test_profiler_follows_worker_threads_with_kernel_and_user_cpu_samples(self):
        args = trial.cpu_profile_command(["worker", "--duration", "90"], Path("/tmp/profile.data"))
        self.assertEqual(args[args.index("--") + 1:], ["worker", "--duration", "90"])
        self.assertEqual(args[args.index("--event") + 1], "cpu-clock")
        self.assertEqual(args[args.index("--clockid") + 1], "mono")
        self.assertEqual(args[args.index("--call-graph") + 1], "dwarf,8192")
        self.assertIn("--no-buildid-cache", args)
        self.assertNotIn("--no-inherit", args)
        self.assertNotIn("--all-cpus", args)

    def test_wrapped_worker_keeps_readiness_watchdog_and_independent_recovery(self):
        directory = Path("/var/tmp/vino-profile-test")
        worker = trial.cpu_profile_command([str(directory / "worker")], directory / "profile.data")
        args = trial.unit_command(directory, worker, 92, stop_timeout=5)
        self.assertIn("NotifyAccess=all", args)
        self.assertIn("KillMode=control-group", args)
        self.assertIn("WatchdogSec=5s", args)
        self.assertIn("TimeoutStopSec=5s", args)
        self.assertTrue(any("ExecStopPost=" in arg and "recover-start" in arg for arg in args))
        self.assertEqual(args[args.index("--") + 1:], worker)

    def test_missing_kernel_symbols_fail_before_recording(self):
        with tempfile.TemporaryDirectory() as name:
            with patch.object(Path, "read_text", return_value="0000000000000000 T copy_user\n"), \
                 patch.object(trial, "command") as command:
                with self.assertRaisesRegex(RuntimeError, "kernel symbol addresses"):
                    trial.prepare_cpu_profile(Path(name))
                command.assert_not_called()

    def test_profiler_permission_error_does_not_produce_a_success_manifest(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            with patch.object(Path, "read_text", return_value="ffffffff81000000 T copy_user\n"), \
                 patch.object(trial, "command", side_effect=RuntimeError("perf denied")):
                with self.assertRaisesRegex(RuntimeError, "perf denied"):
                    trial.prepare_cpu_profile(directory)
            self.assertFalse((directory / "profile.json").exists())
            self.assertEqual(stat.S_IMODE((directory / "profile-kallsyms").stat().st_mode), 0o600)

    def test_empty_recording_is_not_accepted(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            with patch.object(Path, "read_text", return_value="ffffffff81000000 T copy_user\n"), \
                 patch.object(trial, "command", return_value=subprocess.CompletedProcess([], 0, "", "")):
                with self.assertRaisesRegex(RuntimeError, "no preflight samples"):
                    trial.prepare_cpu_profile(directory)
            self.assertFalse((directory / "profile.json").exists())

    def test_profile_symbols_become_readable_only_after_worker_recovery(self):
        state = dict(mock=False, takeover=False, mask_added=False, dock="8-1.2", profile_cpu=True,
                     bindings={}, uid=1, gid=1)
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            trial.save(directory / "state.json", state)
            (directory / "worker").write_bytes(b"binary")
            (directory / "worker").chmod(0o700)
            (directory / "profile.data").write_bytes(b"samples")
            with patch.object(trial, "interface_bindings", return_value={}), patch.object(trial.os, "chown"):
                trial.recover(directory)
            self.assertTrue(json.loads((directory / "recovery.json").read_text())["restored"])
            self.assertEqual(stat.S_IMODE((directory / "worker").stat().st_mode), 0o744)
            self.assertEqual(stat.S_IMODE((directory / "profile.data").stat().st_mode), 0o600)


class RecoveryTests(unittest.TestCase):
    def test_compositor_restore_is_armed_before_suspend_can_fail(self):
        state = dict(mock=False, compositor_was_active=True, compositor_restore_pending=False,
                     takeover=False, mask_added=False)
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            trial.save(directory / "state.json", state)
            def suspend(saved, active):
                self.assertFalse(active)
                self.assertTrue(json.loads((directory / "state.json").read_text())["compositor_restore_pending"])
                raise RuntimeError("suspend interrupted")
            with patch.object(trial, "set_compositor_active", side_effect=suspend), \
                 patch.object(trial, "command") as command:
                with self.assertRaisesRegex(RuntimeError, "suspend interrupted"):
                    trial.prepare(directory)
                command.assert_not_called()

    def test_compositor_restoration_failure_does_not_prevent_display_recovery(self):
        state = dict(mock=False, takeover=True, mask_added=False, dock="8-1.2",
                     outputs=trial.parse_outputs(XRANDR), bindings={}, uid=1, gid=1,
                     compositor_restore_pending=True)
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            trial.save(directory / "state.json", state)
            with patch.object(trial, "notify"), patch.object(trial, "command") as command, \
                 patch.object(trial, "outputs", return_value=state["outputs"]), \
                 patch.object(trial, "desktop_command") as restore, \
                 patch.object(trial, "set_compositor_active", side_effect=RuntimeError("resume failed")) as compositor, \
                 patch.object(trial, "interface_bindings", return_value={}), \
                 patch.object(trial.os, "chown"):
                with self.assertRaisesRegex(RuntimeError, "compositor restoration: resume failed"):
                    trial.recover(directory)
                compositor.assert_called_once_with(state, True)
                command.assert_called_once_with(["systemctl", "start", trial.SERVICE], timeout=15)
                restore.assert_called_once()

    def test_compositor_restores_after_failure_before_display_takeover(self):
        state = dict(mock=False, takeover=False, mask_added=False, dock="8-1.2",
                     bindings={}, uid=1, gid=1, compositor_restore_pending=True)
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            trial.save(directory / "state.json", state)
            with patch.object(trial, "notify"), patch.object(trial, "command") as command, \
                 patch.object(trial, "set_compositor_active") as compositor, \
                 patch.object(trial, "interface_bindings", return_value={}), \
                 patch.object(trial.os, "chown"):
                trial.recover(directory)
                compositor.assert_called_once_with(state, True)
                command.assert_not_called()
            self.assertTrue(json.loads((directory / "recovery.json").read_text())["restored"])

    def test_compositor_left_disabled_when_it_was_disabled_before_trial(self):
        state = dict(mock=False, takeover=False, mask_added=False, dock="8-1.2",
                     bindings={}, uid=1, gid=1, compositor_restore_pending=False,
                     compositor_was_active=False)
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            trial.save(directory / "state.json", state)
            with patch.object(trial, "notify"), patch.object(trial, "set_compositor_active") as compositor, \
                 patch.object(trial, "interface_bindings", return_value={}), \
                 patch.object(trial.os, "chown"):
                trial.recover(directory)
                compositor.assert_not_called()

    def test_cleanup_survives_service_restart_failure_and_reports_it(self):
        state = dict(mock=False, takeover=True, mask_added=False, dock="8-1.2",
                     outputs=trial.parse_outputs(XRANDR), bindings={"2": "snd-usb-audio"}, uid=1, gid=1)
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            trial.save(directory / "state.json", state)
            with patch.object(trial, "notify"), patch.object(trial, "command", side_effect=RuntimeError("start failed")), \
                 patch.object(trial, "outputs", return_value=state["outputs"]), \
                 patch.object(trial, "desktop_command") as restore, \
                 patch.object(trial, "interface_bindings", return_value=state["bindings"]), \
                 patch.object(trial.os, "chown"):
                with self.assertRaisesRegex(RuntimeError, "start failed"):
                    trial.recover(directory)
                restore.assert_called_once()
            result = json.loads((directory / "recovery.json").read_text())
            self.assertFalse(result["restored"])
            self.assertEqual(result["errors"], ["start failed"])

    def test_audio_or_network_change_is_a_failed_recovery(self):
        state = dict(mock=False, takeover=False, mask_added=False, dock="8-1.2",
                     bindings={"2": "snd-usb-audio", "5": "cdc_ncm"}, uid=1, gid=1)
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            trial.save(directory / "state.json", state)
            with patch.object(trial, "notify"), patch.object(trial, "interface_bindings", return_value={}), \
                 patch.object(trial.os, "chown"):
                with self.assertRaisesRegex(RuntimeError, "bindings changed"):
                    trial.recover(directory)


class EvdiBatchTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.parameter = self.directory / 'batch'
        self.version = self.directory / 'version'
        self.active = self.directory / 'active.json'
        self.parameter.write_text('0\n')
        self.version.write_text('candidate\n')
        trial.save(self.active, dict(directory=str(self.directory)))
        trial.save(self.directory / 'state.json', dict(kernel=trial.os.uname().release,
                   goal='candidate', phase='complete', versions=dict(candidate='candidate')))
        for attribute, path in [('EVDI_BATCH_PARAMETER', self.parameter),
                                ('EVDI_VERSION_PATH', self.version), ('EVDI_MODULE_STATE', self.active)]:
            item = patch.object(trial, attribute, path)
            item.start()
            self.addCleanup(item.stop)

    def test_configuration_accepts_only_the_prepared_loaded_candidate(self):
        self.assertEqual(trial.evdi_copy_configuration(128),
                         dict(rows=128, previous=0, srcversion='candidate'))
        self.version.write_text('original\n')
        with self.assertRaisesRegex(RuntimeError, 'candidate is not loaded'):
            trial.evdi_copy_configuration(128)
        self.assertEqual(self.parameter.read_text(), '0\n')

    def test_configuration_rejects_nonzero_starting_value(self):
        self.parameter.write_text('32\n')
        with self.assertRaisesRegex(RuntimeError, 'batching disabled'):
            trial.evdi_copy_configuration(128)

    def test_configuration_rejects_unfinished_switch(self):
        state = json.loads((self.directory / 'state.json').read_text())
        state['phase'] = 'prepared'
        trial.save(self.directory / 'state.json', state)
        with self.assertRaisesRegex(RuntimeError, 'not completed'):
            trial.evdi_copy_configuration(128)

    def test_module_change_prevents_parameter_write(self):
        trial.set_evdi_copy_rows(dict(srcversion='candidate'), 128)
        self.version.write_text('replacement\n')
        with self.assertRaisesRegex(RuntimeError, 'module changed'):
            trial.set_evdi_copy_rows(dict(srcversion='candidate'), 0)
        self.assertEqual(self.parameter.read_text(), '128\n')

    def test_prepare_arms_reset_before_enabling_batching_and_after_official_stop(self):
        state = dict(mock=False, mask_added=False, takeover=False,
                     evdi_copy=dict(rows=128, previous=0, srcversion='candidate'),
                     evdi_copy_restore_pending=False)
        trial.save(self.directory / 'state.json', state)
        calls = []

        def command(args, **kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, 1 if args[0] == 'pgrep' else 0, '0', '')

        def enable(config, value):
            saved = json.loads((self.directory / 'state.json').read_text())
            self.assertTrue(saved['evdi_copy_restore_pending'])
            self.assertIn(['systemctl', 'stop', trial.SERVICE], calls)
            raise RuntimeError('interrupted write')

        with patch.object(trial, 'command', side_effect=command), \
                patch.object(trial, 'set_evdi_copy_rows', side_effect=enable):
            with self.assertRaisesRegex(RuntimeError, 'interrupted write'):
                trial.prepare(self.directory)

    def recover_case(self, fail=False):
        state = dict(mock=False, takeover=True, mask_added=False, dock='mock', bindings={},
                     uid=1, gid=1, outputs=[], evdi_copy_restore_pending=True,
                     evdi_copy=dict(rows=128, previous=0, srcversion='candidate'))
        trial.save(self.directory / 'state.json', state)
        self.parameter.write_text('128\n')

        def command(args, **kwargs):
            if args[:2] == ['systemctl', 'start']:
                self.assertEqual(self.parameter.read_text().strip(), '128' if fail else '0')
            return subprocess.CompletedProcess(args, 0, '', '')

        with patch.object(trial, 'command', side_effect=command) as run, \
                patch.object(trial, 'outputs', return_value=[]), \
                patch.object(trial, 'desktop_command'), \
                patch.object(trial, 'interface_bindings', return_value={}), patch.object(trial.os, 'chown'):
            if fail:
                with patch.object(trial, 'set_evdi_copy_rows', side_effect=RuntimeError('reset failed')):
                    with self.assertRaisesRegex(RuntimeError, 'batch restoration'):
                        trial.recover(self.directory)
            else:
                trial.recover(self.directory)
            run.assert_any_call(['systemctl', 'start', trial.SERVICE], timeout=15)
        return json.loads((self.directory / 'recovery.json').read_text())

    def test_recovery_resets_batching_before_restarting_official_driver(self):
        result = self.recover_case()
        self.assertTrue(result['restored'])
        self.assertEqual(result['evdi_copy_rows_restored'], 0)

    def test_reset_failure_is_reported_without_skipping_display_restoration(self):
        result = self.recover_case(fail=True)
        self.assertFalse(result['restored'])
        self.assertIn('EVDI batch restoration: reset failed', result['errors'])


if __name__ == "__main__":
    unittest.main()
