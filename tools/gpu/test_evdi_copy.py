#!/usr/bin/env python3
"""Exercise benchmark ownership, cleanup and result rejection without loading modules."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('bench', Path(__file__).with_name('bench-evdi-copy.py'))
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class CopyBenchmarkTests(unittest.TestCase):
    def exercise(self, failure=None, loaded=False, unload_failure=False):
        self.calls = []
        self.loaded = loaded

        def invoke(args, **kwargs):
            self.calls.append(args)
            if args[0] == 'insmod':
                self.loaded = True
            elif args[0] == 'rmmod':
                if unload_failure:
                    raise RuntimeError('module busy')
                self.loaded = False
            elif failure:
                raise failure
            return subprocess.CompletedProcess(args, 0, 'client completed\n', '')

        with tempfile.TemporaryDirectory() as directory, patch.object(bench, 'DEVICE') as device:
            device.exists.return_value = True
            try:
                bench.run_loaded(Path(directory) / 'benchmark.ko', Path(directory),
                                 invoke=invoke, present=lambda: self.loaded)
            finally:
                cleanup = Path(directory) / 'cleanup.json'
                self.cleanup = json.loads(cleanup.read_text()) if cleanup.exists() else None

    def test_success_removes_only_benchmark(self):
        self.exercise()
        self.assertFalse(self.loaded)
        self.assertTrue(self.cleanup['module_unloaded'])
        self.assertEqual(self.calls[-1], ['rmmod', 'vino_evdi_copy_bench'])

    def test_client_failure_still_unloads(self):
        with self.assertRaisesRegex(RuntimeError, 'copy failed'):
            self.exercise(RuntimeError('copy failed'))
        self.assertFalse(self.loaded)
        self.assertTrue(self.cleanup['module_unloaded'])

    def test_timeout_still_unloads(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            self.exercise(subprocess.TimeoutExpired('client', 90))
        self.assertFalse(self.loaded)

    def test_cancel_still_unloads(self):
        with self.assertRaises(KeyboardInterrupt):
            self.exercise(KeyboardInterrupt())
        self.assertFalse(self.loaded)

    def test_preloaded_module_never_touched(self):
        with self.assertRaisesRegex(RuntimeError, 'already loaded'):
            self.exercise(loaded=True)
        self.assertEqual(self.calls, [])
        self.assertTrue(self.loaded)

    def test_unload_failure_is_reported(self):
        with self.assertRaisesRegex(RuntimeError, 'Could not remove'):
            self.exercise(unload_failure=True)
        self.assertTrue(self.loaded)
        self.assertEqual(self.cleanup['errors'], ['module busy'])

    def test_failed_load_does_not_unload_someone_elses_module(self):
        calls = []

        def invoke(args, **kwargs):
            calls.append(args)
            raise RuntimeError('insmod failed')

        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, 'insmod failed'):
                bench.run_loaded(Path('unused'), Path(directory), invoke=invoke, present=lambda: False)
        self.assertEqual(len(calls), 1)

    def test_manifest_refuses_wrong_kernel_module_and_checksum(self):
        with tempfile.TemporaryDirectory() as directory:
            build = Path(directory)
            module = build / f'kernel-bench/{bench.MODULE}.ko'
            module.parent.mkdir()
            module.write_bytes(b'test-only fake module')
            report = {'kernel': 'test-kernel', 'correctness': {'passed': True}, 'modules': {
                'isolated_benchmark': {'name': bench.MODULE, 'relative_path': str(module.relative_to(build)),
                                       'sha256': bench.digest(module)}}}

            def write():
                (build / 'build-report.json').write_text(json.dumps(report))

            write()
            bench.validate_manifest(build, 'test-kernel')
            with self.assertRaisesRegex(RuntimeError, 'Kernel changed'):
                bench.validate_manifest(build, 'wrong-kernel')
            entry = report['modules']['isolated_benchmark']
            entry['name'] = 'evdi'
            write()
            with self.assertRaisesRegex(RuntimeError, 'isolated benchmark'):
                bench.validate_manifest(build, 'test-kernel')
            entry['name'] = bench.MODULE
            entry['relative_path'] = '../evdi.ko'
            write()
            with self.assertRaisesRegex(RuntimeError, 'isolated benchmark'):
                bench.validate_manifest(build, 'test-kernel')
            entry['relative_path'] = str(module.relative_to(build))
            write()
            module.write_bytes(b'changed')
            with self.assertRaisesRegex(RuntimeError, 'does not match'):
                bench.validate_manifest(build, 'test-kernel')

    def test_paired_reduction_and_missing_or_duplicated_samples(self):
        samples = [dict(rows=rows, pair=pair, method=method, cpu_ms_per_frame=cpu)
                   for rows in bench.BATCHES for pair in range(4)
                   for method, cpu in [('original', 2), ('batched', 1)]]
        self.assertEqual([s['paired_cpu_reduction_percent'] for s in bench.summarize(samples)], [50] * 5)
        with self.assertRaisesRegex(RuntimeError, 'Incomplete comparison pair'):
            bench.summarize(samples[:-1])
        with self.assertRaisesRegex(RuntimeError, 'duplicated'):
            bench.summarize(samples + [samples[0]])


if __name__ == '__main__':
    unittest.main()
