#!/usr/bin/env python3
"""Build and check an isolated EVDI copy prototype; never install or load modules."""
import argparse
import difflib
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

TOOLS = Path(__file__).resolve().parent
TOOL_NAMES = ('prepare-evdi-copy.py', 'evdi-copy-batching.patch', 'evdi-copy-common.h',
              'evdi-copy-test.c', 'evdi-copy-kernel-bench.c', 'evdi-copy-layout.c',
              'bench-evdi-copy.py', 'test_evdi_copy.py')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def extract(path, name):
    text = path.read_text()
    start = text.index('static int copy_primary_pixels(struct')
    end = text.index('\nstatic void copy_cursor_pixels(', start)
    text = text[start:end]
    for original, replacement in [('struct drm_framebuffer', 'struct copy_framebuffer'),
                                  ('struct drm_clip_rect', 'struct copy_rect'),
                                  ('struct evdi_framebuffer', 'struct copy_frame')]:
        text = text.replace(original, replacement)
    text = text.replace('static int copy_primary_pixels(',
                        f'static COPY_ROUTINE_ATTRIBUTES int copy_{name}(', 1)
    return ('// SPDX-License-Identifier: GPL-2.0-only\n'
            '// Extracted from evdi_painter.c; copyright (c) 2013-2026 DisplayLink (UK) Ltd.\n' + text)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=Path('/usr/src/evdi-1.15.1'))
    parser.add_argument('--kernel', default=os.uname().release)
    parser.add_argument('--cc', default='x86_64-linux-gnu-gcc-13')
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    output = args.output.resolve() if args.output else Path(tempfile.mkdtemp(prefix='vino-evdi-copy-'))
    if args.output:
        output.mkdir(parents=True, exist_ok=False)
    print(f'Build directory: {output}', flush=True)
    if args.source.resolve() == output or args.source.resolve() in output.parents:
        raise RuntimeError('output must be outside the source directory')
    commands = []
    snapshot = output / 'tools'
    snapshot.mkdir()
    for name in TOOL_NAMES:
        shutil.copy2(TOOLS / name, snapshot / name)

    def run(label, arguments, cwd=output, env=None):
        commands.append({'label': label, 'arguments': [str(a) for a in arguments], 'cwd': str(cwd)})
        (output / 'commands.json').write_text(json.dumps(commands, indent=2) + '\n')
        print(label, flush=True)
        with (output / f'{label}.log').open('w') as log:
            result = subprocess.run([str(a) for a in arguments], cwd=cwd, env=env,
                                    stdout=log, stderr=subprocess.STDOUT)
        if result.returncode:
            raise RuntimeError(f'{label} failed; see {output / (label + ".log")}')

    ignore = shutil.ignore_patterns('*.o', '*.ko', '*.mod*', '*.cmd', '.tmp*',
                                    'Module.symvers', 'modules.order', 'evdi_detect.h',
                                    '.git', '__pycache__')
    for phase in ['before', 'after']:
        shutil.copytree(args.source, output / phase, ignore=ignore)
    before = output / 'before/evdi_painter.c'
    run('apply-patch', ['patch', '--batch', '--fuzz=0', '-p1', '-i', TOOLS / 'evdi-copy-batching.patch'],
        cwd=output / 'after')
    after = output / 'after/evdi_painter.c'
    (output / 'applied.patch').write_text(''.join(difflib.unified_diff(
        before.read_text().splitlines(True), after.read_text().splitlines(True),
        fromfile='a/evdi_painter.c', tofile='b/evdi_painter.c')))
    includes = output / 'includes'
    includes.mkdir()
    for phase, name in [('before', 'original'), ('after', 'batched')]:
        (includes / f'copy-{name}.inc').write_text(extract(output / phase / 'evdi_painter.c', name))
    run('compile-tests', [args.cc, '-std=gnu11', '-g', '-O1', '-fsanitize=address,undefined',
                         '-fno-omit-frame-pointer', '-Wall', '-Wextra', '-Werror',
                         '-I', includes, '-I', TOOLS, TOOLS / 'evdi-copy-test.c', '-o', output / 'copy-test'])
    run('correctness', [output / 'copy-test'], env={**os.environ, 'ASAN_OPTIONS': 'detect_leaks=1'})
    correctness = json.loads((output / 'correctness.log').read_text())
    run('runner-tests', ['python3', TOOLS / 'test_evdi_copy.py'])
    kernel = Path('/lib/modules') / args.kernel / 'build'
    for phase in ['before', 'after']:
        run(f'build-{phase}', ['make', '-C', kernel, f'M={output / phase}', f'CC={args.cc}', '-j4', 'modules'])
    bench = output / 'kernel-bench'
    bench.mkdir()
    for item in includes.iterdir():
        shutil.copy2(item, bench / item.name)
    shutil.copy2(TOOLS / 'evdi-copy-common.h', bench)
    shutil.copy2(TOOLS / 'evdi-copy-kernel-bench.c', bench / 'vino_evdi_copy_bench.c')
    (bench / 'Makefile').write_text('obj-m := vino_evdi_copy_bench.o\n')
    run('build-benchmark', ['make', '-C', kernel, f'M={bench}', f'CC={args.cc}', '-j4', 'modules'])
    run('compile-layout', [args.cc, '-std=c11', '-D_GNU_SOURCE', '-O2', '-Wall', '-Wextra', '-Werror',
                           '-I/usr/include/libdrm', TOOLS / 'evdi-copy-layout.c', '-ldrm', '-o', output / 'evdi-copy-layout'])

    def modinfo(field, path):
        return subprocess.check_output(['modinfo', '-F', field, str(path)], text=True).strip()

    report = {
        'kernel': args.kernel, 'source': str(args.source.resolve()),
        'loaded_evdi_srcversion': Path('/sys/module/evdi/srcversion').read_text().strip(),
        'source_painter_sha256': digest(before), 'patched_painter_sha256': digest(after),
        'correctness': correctness, 'modules': {},
        'scope': 'Build and host correctness checks only; no kernel module installed or loaded',
    }
    for name, relative in [('original', 'before/evdi.ko'), ('candidate', 'after/evdi.ko'),
                           ('isolated_benchmark', 'kernel-bench/vino_evdi_copy_bench.ko')]:
        path = output / relative
        report['modules'][name] = {'relative_path': relative, 'sha256': digest(path),
                                  'name': modinfo('name', path), 'vermagic': modinfo('vermagic', path),
                                  'srcversion': modinfo('srcversion', path)}
    report['rebuilt_original_matches_loaded_srcversion'] = (
        report['loaded_evdi_srcversion'] == report['modules']['original']['srcversion'])
    report['tool_sha256'] = {name: digest(snapshot / name) for name in TOOL_NAMES}
    (output / 'build-report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2), flush=True)


if __name__ == '__main__':
    main()
