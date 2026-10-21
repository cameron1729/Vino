"""Model driver ownership/failure paths without changing modules or display services."""
import contextlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('module_trial', Path(__file__).with_name('evdi-module-trial.py'))
trial = importlib.util.module_from_spec(spec)
spec.loader.exec_module(trial)


class ModuleTrialTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.parameter = self.directory / 'parameter'
        self.mask = self.directory / 'mask'
        self.loaded = 'old'
        self.services = {'sddm.service': True, trial.SERVICE: True}
        self.commands = []
        self.fail_load = False
        self.busy = False
        self.fail_start = False
        self.state = dict(goal='candidate', versions=dict(original='old', candidate='new'),
                          parameters=dict(initial_device_count=4, initial_loglevel=6),
                          display_manager='sddm.service', phase='prepared',
                          stop_started=False, mask_added=False, hashes={})
        for name in ['original', 'candidate']:
            path = self.directory / f'{name}.ko'
            path.write_text(name)
            self.state[name] = str(path)
            self.state['hashes'][name] = trial.digest(path)
        trial.save(self.directory / 'state.json', self.state)
        patches = [patch.object(trial, 'command', side_effect=self.command),
                   patch.object(trial, 'version', side_effect=lambda: self.loaded),
                   patch.object(trial, 'active', side_effect=lambda unit: self.services[unit]),
                   patch.object(trial, 'exclusive', side_effect=lambda **kwargs: contextlib.nullcontext()),
                   patch.object(trial, 'PARAMETER', self.parameter),
                   patch.object(trial, 'MASK', self.mask), patch.object(trial.os, 'geteuid', return_value=0)]
        for item in patches:
            item.start()
            self.addCleanup(item.stop)

    def command(self, arguments, **kwargs):
        arguments = list(map(str, arguments))
        self.commands.append(arguments)
        output = ''
        if arguments[:2] == ['systemctl', 'mask']:
            if not self.mask.is_symlink():
                self.mask.symlink_to('/dev/null')
        elif arguments[:2] == ['systemctl', 'unmask']:
            self.mask.unlink()
        elif arguments[:2] == ['systemctl', 'stop']:
            self.services = {name: False for name in self.services}
        elif arguments[:2] == ['systemctl', 'show']:
            output = '0\n'
        elif arguments[:2] == ['systemctl', 'start']:
            if self.fail_start:
                self.fail_start = False
                raise RuntimeError('start failed')
            self.services = {name: True for name in self.services}
        elif arguments[0] == 'rmmod':
            if self.busy:
                raise RuntimeError('module busy')
            self.assertEqual(arguments, ['rmmod', 'evdi'])
            self.loaded = None
            self.parameter.unlink(missing_ok=True)
        elif arguments[0] == 'insmod':
            candidate = arguments[1] == self.state['candidate']
            if candidate and self.fail_load:
                raise RuntimeError('candidate load failed')
            self.loaded = 'new' if candidate else 'old'
            if candidate:
                self.assertIn('copy_batch_rows=0', arguments)
                self.parameter.write_text('0\n')
        return subprocess.CompletedProcess(arguments, 0, output, '')

    def recovered(self):
        trial.recover(self.directory)
        outcome = json.loads((self.directory / 'recovery.json').read_text())
        self.assertTrue(outcome['original_restored'])
        self.assertFalse(outcome['errors'])
        self.assertTrue(all(self.services.values()))
        self.assertFalse(self.mask.is_symlink())

    def test_success_restarts_services_with_batching_disabled(self):
        trial.step(self.directory)
        self.assertEqual(self.loaded, 'new')
        self.assertEqual(self.parameter.read_text().strip(), '0')
        self.assertTrue(all(self.services.values()))
        self.assertEqual(json.loads((self.directory / 'state.json').read_text())['phase'], 'complete')
        self.assertFalse(self.mask.is_symlink())

    def test_failed_candidate_load_restores_original_and_login_screen(self):
        self.fail_load = True
        with self.assertRaisesRegex(RuntimeError, 'candidate load failed'):
            trial.step(self.directory)
        self.assertIsNone(self.loaded)
        self.recovered()

    def test_busy_original_is_never_forced_out(self):
        self.busy = True
        with self.assertRaisesRegex(RuntimeError, 'module busy'):
            trial.step(self.directory)
        self.assertEqual(self.loaded, 'old')
        self.recovered()

    def test_failed_desktop_start_after_load_rolls_back(self):
        self.fail_start = True
        with self.assertRaisesRegex(RuntimeError, 'start failed'):
            trial.step(self.directory)
        self.assertEqual(self.loaded, 'new')
        self.recovered()

    def test_bad_module_checksum_rejected_before_unload(self):
        Path(self.state['candidate']).write_text('changed')
        with self.assertRaisesRegex(RuntimeError, 'checksum'):
            trial.load_module(self.state, 'candidate')
        self.assertEqual(self.loaded, 'old')
        self.assertFalse(self.commands)

    def test_unknown_module_is_never_removed(self):
        self.loaded = 'someone-else'
        with self.assertRaisesRegex(RuntimeError, 'unexpected'):
            trial.load_module(self.state, 'original')
        self.assertFalse(self.commands)

    def test_recovery_failure_still_attempts_login_screen(self):
        trial.step(self.directory)
        self.busy = True
        with self.assertRaisesRegex(RuntimeError, 'Recovery needs attention'):
            trial.recover(self.directory)
        self.assertTrue(all(self.services.values()))
        self.assertFalse(self.mask.is_symlink())

    def test_unowned_runtime_override_is_retained(self):
        self.state['mask_added'] = True
        self.mask.write_text('another override')
        with self.assertRaisesRegex(RuntimeError, 'override changed'):
            trial.unmask(self.state)
        self.assertEqual(self.mask.read_text(), 'another override')

    def test_success_does_not_schedule_fallback(self):
        trial.step(self.directory)
        self.commands.clear()
        with patch.dict(trial.os.environ, {'SERVICE_RESULT': 'success'}):
            trial.recover_start(self.directory)
        self.assertFalse(self.commands)
        self.assertFalse(json.loads((self.directory / 'recovery.json').read_text())['fallback_used'])

    def test_timeout_uses_independent_recovery_service(self):
        with patch.dict(trial.os.environ, {'SERVICE_RESULT': 'timeout'}):
            trial.recover_start(self.directory)
        arguments = self.commands[-1]
        self.assertEqual(arguments[0], 'systemd-run')
        self.assertIn(f'vino-module-recover-{self.directory.name}', arguments)
        self.assertEqual(arguments[-2:], ['_recover', str(self.directory)])

    def test_upgrade_loads_new_candidate_and_recovery_keeps_true_original(self):
        self.state['versions']['previous'] = 'previous-candidate'
        self.loaded = 'previous-candidate'
        trial.save(self.directory / 'state.json', self.state)
        trial.step(self.directory)
        self.assertEqual(self.loaded, 'new')
        self.recovered()
        self.assertEqual(self.loaded, 'old')
        self.assertEqual([args[1] for args in self.commands if args[0] == 'insmod'],
                         [self.state['candidate'], self.state['original']])

    def test_failed_upgrade_restores_true_original(self):
        self.state['versions']['previous'] = 'previous-candidate'
        self.loaded = 'previous-candidate'
        self.fail_load = True
        trial.save(self.directory / 'state.json', self.state)
        with self.assertRaisesRegex(RuntimeError, 'candidate load failed'):
            trial.step(self.directory)
        self.recovered()
        self.assertEqual(self.loaded, 'old')


class PreviousCandidateTests(unittest.TestCase):
    """The only accepted upgrade source is a verified prior supervisor state."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.installed = self.directory / 'installed.ko'
        self.installed.write_text('true original')
        self.previous_dir = self.directory / 'previous'
        self.previous_dir.mkdir()
        original = self.previous_dir / 'original.ko'
        original.write_bytes(self.installed.read_bytes())
        candidate = self.previous_dir / 'candidate.ko'
        candidate.write_text('previous candidate')
        self.previous = dict(
            kernel=trial.os.uname().release, goal='candidate', phase='complete',
            versions=dict(original='original-version', candidate='previous-version'),
            original=str(original), candidate=str(candidate),
            hashes=dict(original=trial.digest(original), candidate=trial.digest(candidate)))
        self.state = self.previous_dir / 'state.json'
        trial.save(self.state, self.previous)
        active = self.directory / 'active.json'
        trial.save(active, dict(directory=str(self.previous_dir)))
        self.build = self.directory / 'build'
        (self.build / 'after').mkdir(parents=True)
        new_candidate = self.build / 'after/evdi.ko'
        new_candidate.write_text('new candidate')
        self.report = dict(
            kernel=trial.os.uname().release, correctness=dict(passed=True),
            rebuilt_original_matches_installed_srcversion=True,
            rebuilt_original_matches_loaded_srcversion=False,
            modules=dict(original=dict(srcversion='original-version'),
                         candidate=dict(name='evdi', relative_path='after/evdi.ko',
                                        sha256=trial.digest(new_candidate),
                                        srcversion='new-version')))
        trial.save(self.build / 'build-report.json', self.report)
        self.loaded = 'previous-version'
        patches = [patch.object(trial, 'ACTIVE', active),
                   patch.object(trial, 'version', side_effect=lambda: self.loaded),
                   patch.object(trial, 'modinfo', side_effect=self.modinfo),
                   patch.object(trial, 'protected_file')]
        for item in patches:
            mocked = item.start()
            self.addCleanup(item.stop)
            if item.attribute == 'protected_file':
                self.protected = mocked

    def modinfo(self, field, path):
        if field == 'filename':
            return str(self.installed)
        if field == 'name':
            return 'evdi'
        if field == 'vermagic':
            return trial.os.uname().release + ' SMP'
        if field == 'srcversion':
            return 'new-version' if Path(path) == self.build / 'after/evdi.ko' else 'original-version'
        raise AssertionError((field, path))

    def verify(self):
        return trial.verified_previous_candidate(self.installed, 'original-version')

    def test_known_previous_candidate_keeps_installed_original(self):
        report, candidate, installed, previous = trial.build_info(self.build)
        self.assertEqual(previous, self.previous)
        self.assertEqual(installed, self.installed)
        self.assertEqual(candidate, self.build / 'after/evdi.ko')
        self.assertEqual(report['modules']['original']['srcversion'], 'original-version')
        self.assertEqual(len(self.protected.call_args_list), 4)

    def test_original_loaded_does_not_require_previous_state(self):
        self.loaded = 'original-version'
        self.assertIsNone(trial.build_info(self.build)[3])
        self.protected.assert_not_called()

    def test_unknown_loaded_module_is_rejected(self):
        self.loaded = 'unknown-version'
        with self.assertRaisesRegex(RuntimeError, 'does not match'):
            trial.build_info(self.build)

    def test_incomplete_wrong_kernel_and_restore_states_are_rejected(self):
        for key, value in [('phase', 'loading'), ('kernel', 'other-kernel'),
                           ('goal', 'original')]:
            with self.subTest(key=key):
                trial.save(self.state, {**self.previous, key: value})
                with self.assertRaisesRegex(RuntimeError, 'not complete'):
                    self.verify()

    def test_unprotected_state_is_rejected(self):
        self.protected.side_effect = RuntimeError('not protected root-owned storage')
        with self.assertRaisesRegex(RuntimeError, 'protected root-owned'):
            self.verify()

    def test_changed_staged_original_or_candidate_is_rejected(self):
        for name in ('original', 'candidate'):
            with self.subTest(name=name):
                path = Path(self.previous[name])
                original = path.read_bytes()
                path.write_text('tampered')
                with self.assertRaisesRegex(RuntimeError, 'fingerprint mismatch'):
                    self.verify()
                path.write_bytes(original)

    def test_changed_installed_original_is_rejected(self):
        self.installed.write_text('changed installed original')
        with self.assertRaisesRegex(RuntimeError, 'Installed original changed'):
            self.verify()

    def test_wrong_original_identity_is_rejected(self):
        self.previous['versions']['original'] = 'other-original'
        trial.save(self.state, self.previous)
        with self.assertRaisesRegex(RuntimeError, 'does not match'):
            self.verify()

    def test_candidate_source_version_must_match_report(self):
        with patch.object(trial, 'modinfo', side_effect=lambda field, path:
                          'wrong-version' if field == 'srcversion' and Path(path) == self.build / 'after/evdi.ko' else self.modinfo(field, path)):
            with self.assertRaisesRegex(RuntimeError, 'Candidate source version'):
                trial.build_info(self.build)

    def test_installed_source_version_must_match_report(self):
        with patch.object(trial, 'modinfo', side_effect=lambda field, path:
                          'wrong-version' if field == 'srcversion' and Path(path) == self.installed else self.modinfo(field, path)):
            with self.assertRaisesRegex(RuntimeError, 'Installed EVDI differs'):
                trial.build_info(self.build)


class ProtectedStorageTests(unittest.TestCase):
    def test_root_owned_nonwritable_file_and_parent_are_accepted(self):
        target = Path('/mock/private/state.json')
        with patch.object(Path, 'lstat', return_value=SimpleNamespace(st_uid=0, st_mode=0o100600)), \
                patch.object(Path, 'is_symlink', return_value=False):
            trial.protected_file(target)

    def test_writable_or_unowned_file_or_parent_is_rejected(self):
        good = SimpleNamespace(st_uid=0, st_mode=0o100600)
        bad_entries = [SimpleNamespace(st_uid=1000, st_mode=0o100600),
                       SimpleNamespace(st_uid=0, st_mode=0o100622)]
        for bad in bad_entries:
            for sequence in ([bad], [good, bad]):
                with self.subTest(sequence=sequence), \
                        patch.object(Path, 'lstat', side_effect=sequence), \
                        patch.object(Path, 'is_symlink', return_value=False):
                    with self.assertRaisesRegex(RuntimeError, 'protected root-owned'):
                        trial.protected_file(Path('/mock/private/state.json'))

    def test_symlink_file_or_parent_is_rejected(self):
        good = SimpleNamespace(st_uid=0, st_mode=0o100600)
        for symlinks in ([True], [False, True]):
            with self.subTest(symlinks=symlinks), \
                    patch.object(Path, 'lstat', return_value=good), \
                    patch.object(Path, 'is_symlink', side_effect=symlinks):
                with self.assertRaisesRegex(RuntimeError, 'protected root-owned'):
                    trial.protected_file(Path('/mock/private/state.json'))


if __name__ == '__main__':
    unittest.main()
