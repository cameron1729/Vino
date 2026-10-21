#!/usr/bin/env python3
"""Compare EVDI copy routines in a private-RAM module, without replacing EVDI."""
import argparse
import ctypes
import errno
import fcntl
import hashlib
import json
import mmap
import os
from pathlib import Path
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import time

MODULE = 'vino_evdi_copy_bench'
DEVICE = Path('/dev/vino-evdi-copy-bench')
FRAME_BYTES = 1920 * 1080 * 4
REQUEST = struct.Struct('=QIIIIQ')
COPY_BENCH = (3 << 30) | (REQUEST.size << 16) | (ord('V') << 8) | 0x91
BATCHES = (0, 8, 32, 128, 1080)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def command(arguments, *, check=True, timeout=10):
    result = subprocess.run([str(a) for a in arguments], capture_output=True,
                            text=True, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f'{arguments[0]}: {result.stderr.strip() or result.stdout.strip()}')
    return result


def snapshot():
    version = Path('/sys/module/evdi/srcversion')
    return {
        'evdi_srcversion': version.read_text().strip() if version.exists() else None,
        'official_service': command(['systemctl', 'is-active', 'displaylink-driver.service'],
                                    check=False).stdout.strip(),
    }


def validate_manifest(build, running_kernel):
    report = json.loads((build / 'build-report.json').read_text())
    if report['kernel'] != running_kernel:
        raise RuntimeError('Kernel changed since the build; rebuild before testing')
    entry = report['modules']['isolated_benchmark']
    if entry['name'] != MODULE or entry['relative_path'] != f'kernel-bench/{MODULE}.ko':
        raise RuntimeError('Build report does not identify the isolated benchmark module')
    module = build / entry['relative_path']
    if digest(module) != entry['sha256']:
        raise RuntimeError('Benchmark module does not match the build report')
    if not report['correctness']['passed']:
        raise RuntimeError('Host correctness checks did not pass')
    return report, module


def summarize(samples):
    summary = []
    for rows in BATCHES:
        selected = [s for s in samples if s['rows'] == rows]
        pairs = sorted({s['pair'] for s in selected})
        changes = []
        for pair in pairs:
            values = {s['method']: s for s in selected if s['pair'] == pair}
            if set(values) != {'original', 'batched'}:
                raise RuntimeError('Incomplete comparison pair')
            changes.append(100 * (1 - values['batched']['cpu_ms_per_frame'] /
                                  values['original']['cpu_ms_per_frame']))
        if not changes or len(selected) != 2 * len(pairs):
            raise RuntimeError('Missing or duplicated benchmark samples')
        summary.append({
            'rows': rows,
            'copy_calls_per_frame': 1080 if rows <= 1 else (1080 + rows - 1) // rows,
            'original_cpu_ms_per_frame': statistics.median(
                s['cpu_ms_per_frame'] for s in selected if s['method'] == 'original'),
            'batched_cpu_ms_per_frame': statistics.median(
                s['cpu_ms_per_frame'] for s in selected if s['method'] == 'batched'),
            'paired_cpu_reduction_percent': statistics.median(changes),
            'paired_cpu_reduction_percent_all': changes,
        })
    return summary


def client(output):
    if ctypes.sizeof(ctypes.c_void_p) != 8 or os.uname().machine != 'x86_64':
        raise RuntimeError('This benchmark ABI has only been checked on x86-64')
    iterations, pairs = 64, 4
    expected = bytes(range(256)) * (FRAME_BYTES // 256)
    memory = mmap.mmap(-1, FRAME_BYTES, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
    address = ctypes.addressof(ctypes.c_char.from_buffer(memory))
    libc = ctypes.CDLL(None, use_errno=True)
    libc.mprotect.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
    libc.mprotect.restype = ctypes.c_int
    affinity = sorted(os.sched_getaffinity(0))
    # Pin only this short-lived benchmark process; no system affinity changes.
    os.sched_setaffinity(0, {affinity[-1]})

    def protect(value):
        if libc.mprotect(address + FRAME_BYTES - mmap.PAGESIZE, mmap.PAGESIZE, value):
            raise OSError(ctypes.get_errno(), 'mprotect failed')

    with DEVICE.open('rb', buffering=0) as device:
        def invoke(method, rows, count, destination=address):
            request = bytearray(REQUEST.pack(destination, rows, count, method, 0, 0))
            wall_start, cpu_start = time.perf_counter_ns(), time.thread_time_ns()
            fcntl.ioctl(device.fileno(), COPY_BENCH, request, True)
            cpu_ns, wall_ns = time.thread_time_ns() - cpu_start, time.perf_counter_ns() - wall_start
            return cpu_ns, wall_ns, REQUEST.unpack(request)[-1]

        fault_checks = 0
        for method in (0, 1):
            for rows in BATCHES:
                for destination, guard in [(0, False), (address, True)]:
                    if guard:
                        protect(0)
                    try:
                        try:
                            invoke(method, rows, 1, destination)
                        except OSError as error:
                            if error.errno != errno.EFAULT:
                                raise
                            fault_checks += 1
                        else:
                            raise RuntimeError('Invalid destination did not produce EFAULT')
                    finally:
                        if guard:
                            protect(mmap.PROT_READ | mmap.PROT_WRITE)
        samples = []
        for rows in BATCHES:
            for pair in range(pairs):
                for method in ((0, 1) if pair % 2 == 0 else (1, 0)):
                    memory[:] = b'\xa5' * FRAME_BYTES  # Fault in pages outside the timed interval.
                    invoke(method, rows, 8)
                    if memory[:] != expected:
                        raise RuntimeError('Warmup produced incorrect pixels')
                    # Require each timed run to write the expected bytes again.
                    memory[:] = b'\xa5' * FRAME_BYTES
                    cpu_ns, wall_ns, kernel_ns = invoke(method, rows, iterations)
                    if memory[:] != expected:
                        raise RuntimeError('Measured copy produced incorrect pixels')
                    samples.append({
                        'rows': rows, 'pair': pair, 'method': 'batched' if method else 'original',
                        'iterations': iterations, 'cpu_ns': cpu_ns, 'wall_ns': wall_ns,
                        'kernel_elapsed_ns': kernel_ns, 'cpu_ms_per_frame': cpu_ns / iterations / 1e6,
                    })
            print(f'Compared {rows or "disabled"} rows per batch', flush=True)
        report = {
            'kernel': os.uname().release, 'width': 1920, 'height': 1080,
            'source': 'private vmalloc RAM, not a GPU-backed framebuffer',
            'cache_flush': 'actual running-kernel drm_clflush_virt_range',
            'cpu': affinity[-1], 'original_affinity': affinity,
            'fault_checks': fault_checks, 'samples': samples, 'summary': summarize(samples),
            'scope': 'Copy/cache-flush component only; excludes GPU capture, fences, encoding, USB and power',
        }
        output.write_text(json.dumps(report, indent=2) + '\n')
    memory.close()


def run_loaded(module, output, invoke=command, present=lambda: Path('/sys/module', MODULE).exists()):
    """Own only a fresh benchmark module; always attempt removal after a successful load."""
    if present():
        raise RuntimeError('A benchmark module is already loaded; refusing to take ownership')
    loaded = False
    failure = None
    cleanup = {'module_unloaded': False, 'errors': []}
    try:
        invoke(['insmod', module])
        loaded = True
        deadline = time.monotonic() + 5
        while not DEVICE.exists():
            if time.monotonic() >= deadline:
                raise RuntimeError('Benchmark device did not appear')
            time.sleep(0.05)
        result = invoke([sys.executable, Path(__file__).resolve(), '_client', '--output', output / 'results.json'],
                        check=False, timeout=90)
        (output / 'client.log').write_text(result.stdout + result.stderr)
        if result.returncode:
            raise RuntimeError(f'Copy benchmark failed; see {output / "client.log"}')
    except BaseException as error:
        failure = error
    finally:
        if loaded:
            # A Ctrl+C stops the client before removal; a second Ctrl+C must not skip cleanup.
            previous = {sig: signal.signal(sig, signal.SIG_IGN) for sig in (signal.SIGINT, signal.SIGTERM)}
            try:
                invoke(['rmmod', MODULE])
                cleanup['module_unloaded'] = not present()
                if not cleanup['module_unloaded']:
                    cleanup['errors'].append('Benchmark module remains loaded')
            except BaseException as error:
                cleanup['errors'].append(str(error))
            finally:
                for sig, handler in previous.items():
                    signal.signal(sig, handler)
        (output / 'cleanup.json').write_text(json.dumps(cleanup, indent=2) + '\n')
    if cleanup['errors']:
        raise RuntimeError(f'Could not remove {MODULE}: {cleanup["errors"]}') from failure
    if failure is not None:
        raise failure


def run(build):
    if os.geteuid() != 0:
        raise RuntimeError('Module loading needs sudo; run this command in your terminal')
    os.environ['PATH'] = '/usr/sbin:/usr/bin:/sbin:/bin'
    report, module = validate_manifest(build.resolve(), os.uname().release)
    output = Path(tempfile.mkdtemp(prefix='vino-evdi-copy-', dir='/var/tmp'))
    output.chmod(0o755)
    print(f'Benchmark logs: {output}', flush=True)
    print('Private RAM copy test; the running EVDI module and displays stay in place', flush=True)
    stage = output / 'module'
    stage.mkdir(mode=0o700)
    staged = stage / module.name
    shutil.copyfile(module, staged)
    staged.chmod(0o600)
    if digest(staged) != report['modules']['isolated_benchmark']['sha256']:
        raise RuntimeError('Staged module checksum mismatch')
    if command(['modinfo', '-F', 'name', staged]).stdout.strip() != MODULE:
        raise RuntimeError('Staged file is not the benchmark module')
    if command(['modinfo', '-F', 'vermagic', staged]).stdout.split()[0] != os.uname().release:
        raise RuntimeError('Staged module was built for a different kernel')
    before = snapshot()
    if not before['evdi_srcversion']:
        raise RuntimeError('Expected the existing EVDI module to remain loaded')
    (output / 'build-report.json').write_text(json.dumps(report, indent=2) + '\n')
    (output / 'before.json').write_text(json.dumps(before, indent=2) + '\n')
    def interrupted(signum, frame):
        raise KeyboardInterrupt(f'Interrupted by signal {signum}')

    previous = signal.signal(signal.SIGTERM, interrupted)
    try:
        run_loaded(staged, output)
    finally:
        signal.signal(signal.SIGTERM, previous)
        after = snapshot()
        (output / 'after.json').write_text(json.dumps(after, indent=2) + '\n')
        for entry in output.glob('*.json'):
            entry.chmod(0o644)
    if after != before:
        raise RuntimeError('EVDI/service state changed during measurement; inspect before/after logs')
    result = json.loads((output / 'results.json').read_text())
    for item in result['summary']:
        print(f'{item["rows"] or "disabled":>8} rows: '
              f'{item["original_cpu_ms_per_frame"]:.3f} -> {item["batched_cpu_ms_per_frame"]:.3f} CPU ms/frame; '
              f'{item["paired_cpu_reduction_percent"]:+.1f}% saving')
    print('Benchmark module removed; component results saved', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest='mode', required=True)
    runner = modes.add_parser('run')
    runner.add_argument('--build', type=Path, required=True)
    internal = modes.add_parser('_client', help=argparse.SUPPRESS)
    internal.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.mode == 'run':
            run(args.build)
        else:
            client(args.output)
    except (Exception, KeyboardInterrupt) as error:
        print(f'bench-evdi-copy: {error or "interrupted"}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
