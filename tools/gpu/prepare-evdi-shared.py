#!/usr/bin/env python3
"""Build an isolated EVDI shared-capture prototype; never install or load it."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

TOOLS = Path(__file__).resolve().parent
TOOL_NAMES = ('prepare-evdi-shared.py', 'evdi-copy-batching.patch',
              'evdi-shared-capture.patch', 'evdi-shared-capture.h',
              'evdi-shared-capture-test.c')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=Path('/usr/src/evdi-1.15.1'))
    parser.add_argument('--kernel', default=os.uname().release)
    parser.add_argument('--cc', default='x86_64-linux-gnu-gcc-13')
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    source = args.source.resolve()
    output = (args.output.resolve() if args.output else
              Path(tempfile.mkdtemp(prefix='vino-evdi-shared-')))
    if output == source or source in output.parents:
        raise RuntimeError('output must be outside the source directory')
    if args.output:
        output.mkdir(parents=True, exist_ok=False)
    print(f'Build directory: {output}', flush=True)
    snapshot = output / 'tools'
    snapshot.mkdir()
    for name in TOOL_NAMES:
        shutil.copy2(TOOLS / name, snapshot / name)
    commands = []

    def run(label, arguments, cwd=output, env=None):
        commands.append({'label': label, 'arguments': [str(a) for a in arguments],
                         'cwd': str(cwd)})
        (output / 'commands.json').write_text(json.dumps(commands, indent=2) + '\n')
        print(label, flush=True)
        log_path = output / f'{label}.log'
        with log_path.open('w') as log:
            result = subprocess.run([str(a) for a in arguments], cwd=cwd, env=env,
                                    stdout=log, stderr=subprocess.STDOUT)
        if result.returncode:
            raise RuntimeError(f'{label} failed; see {log_path}')

    ignore = shutil.ignore_patterns('*.o', '*.ko', '*.mod*', '*.cmd', '.tmp*',
                                   'Module.symvers', 'modules.order', 'evdi_detect.h',
                                   '.git', '__pycache__')
    for phase in ('before', 'after'):
        shutil.copytree(source, output / phase, ignore=ignore)
    for patch in ('evdi-copy-batching.patch', 'evdi-shared-capture.patch'):
        run(f'apply-{patch}', ['patch', '--batch', '--fuzz=0', '-p1', '-i',
                              snapshot / patch], cwd=output / 'after')
    if ((output / 'after/evdi_shared_capture.h').read_bytes() !=
            (snapshot / 'evdi-shared-capture.h').read_bytes()):
        raise RuntimeError('patched ABI header differs from userspace header')
    # Compile the actual kernel implementation under the host fault harness.
    # Keep it in the .c file for the kernel: this kernel's modpost srcversion
    # calculation omits relative includes, so separate implementation headers
    # would allow different candidates to acquire the same source identity.
    implementation = (output / 'after/evdi_painter.c').read_text().split(
        '/* BEGIN VINO_SHARED_CAPTURE_IMPLEMENTATION */\n', 1)[1].split(
        '/* END VINO_SHARED_CAPTURE_IMPLEMENTATION */', 1)[0]
    includes = output / 'includes'
    includes.mkdir()
    (includes / 'evdi_shared_capture_impl.inc').write_text(implementation)
    run('compile-tests', [args.cc, '-std=gnu11', '-D_GNU_SOURCE', '-g', '-O1',
                         '-fsanitize=address,undefined', '-fno-omit-frame-pointer',
                         '-Wall', '-Wextra', '-Werror', '-Wno-unused-parameter',
                         '-I', snapshot, '-I', includes,
                         snapshot / 'evdi-shared-capture-test.c',
                         '-o', output / 'shared-capture-test'])
    run('correctness', [output / 'shared-capture-test'],
        env={**os.environ, 'ASAN_OPTIONS': 'detect_leaks=1'})
    correctness = json.loads((output / 'correctness.log').read_text())
    kernel = Path('/lib/modules') / args.kernel / 'build'
    for phase in ('before', 'after'):
        run(f'build-{phase}', ['make', '-C', kernel, f'M={output / phase}',
                              f'CC={args.cc}', '-j4', 'modules'])

    def modinfo(field, path):
        return subprocess.check_output(['modinfo', '-F', field, str(path)],
                                       text=True).strip()

    loaded = Path('/sys/module/evdi/srcversion')
    installed = Path(subprocess.check_output(['modinfo', '-n', 'evdi'], text=True).strip())
    report = {
        'kernel': args.kernel,
        'source': str(source),
        'shared_capture_abi': 1,
        'loaded_evdi_srcversion': loaded.read_text().strip() if loaded.exists() else None,
        'installed_module': str(installed),
        'installed_evdi_srcversion': modinfo('srcversion', installed),
        'source_painter_sha256': digest(output / 'before/evdi_painter.c'),
        'patched_painter_sha256': digest(output / 'after/evdi_painter.c'),
        'correctness': correctness,
        'modules': {},
        'scope': 'Isolated build and host fault-injection checks only; no module installed or loaded',
        'runtime_validation': 'Pending: real producer fences, Vulkan import, mode/close races and display recovery',
        'producer_scope': 'Experimental amdgpu-imported linear XRGB8888 with implicit-sync GPU writes and separate cursor; CPU mmap or explicit-sync writers unsupported',
        'tool_sha256': {name: digest(snapshot / name) for name in TOOL_NAMES},
    }
    for name, relative in [('original', 'before/evdi.ko'), ('candidate', 'after/evdi.ko')]:
        path = output / relative
        report['modules'][name] = {
            'relative_path': relative, 'sha256': digest(path),
            'name': modinfo('name', path), 'vermagic': modinfo('vermagic', path),
            'srcversion': modinfo('srcversion', path),
        }
    original = report['modules']['original']['srcversion']
    report['rebuilt_original_matches_loaded_srcversion'] = (
        original == report['loaded_evdi_srcversion'])
    report['rebuilt_original_matches_installed_srcversion'] = (
        original == report['installed_evdi_srcversion'])
    (output / 'build-report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2), flush=True)


if __name__ == '__main__':
    main()
