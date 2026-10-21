#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Controlled X11 input/CPU comparison; never claims equal delivered output."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import time


def command(args, **kwargs):
    kwargs.setdefault('timeout', 15)
    return subprocess.run([str(x) for x in args], check=True, capture_output=True, text=True, **kwargs).stdout


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def preflight():
    if os.environ.get('XDG_SESSION_TYPE') != 'x11':
        raise RuntimeError('This replay is only validated for X11')
    monitors = command(['xrandr', '--listmonitors'])
    heads = []
    for line in monitors.splitlines():
        m = re.search(r'(\d+)/\d+x(\d+)/\d+([+-]\d+)([+-]\d+)\s+(\S+)$', line)
        if m and m[5].startswith('DVI-I-'):
            width, height, x, y = map(int, m.group(1, 2, 3, 4))
            heads.append({'name': m[5], 'width': width, 'height': height, 'x': x, 'y': y})
    heads.sort(key=lambda h: (h['x'], h['y']))
    if len(heads) != 2 or any((h['width'], h['height']) != (1920, 1080) for h in heads):
        raise RuntimeError('Expected exactly two active 1920x1080 DisplayLink DVI monitors')
    if max(heads[0]['x'], heads[1]['x']) < min(heads[0]['x']+1920, heads[1]['x']+1920) and max(heads[0]['y'], heads[1]['y']) < min(heads[0]['y']+1080, heads[1]['y']+1080):
        raise RuntimeError('DisplayLink monitor rectangles overlap')
    pids = {}
    for name in ('Xorg', 'kwin_x11'):
        found = command(['pgrep', '-x', name]).split()
        if len(found) != 1:
            raise RuntimeError(f'Expected one {name} process')
        pids[name] = int(found[0])
    pids['DisplayLinkManager'] = int(command(['systemctl', 'show', 'displaylink-driver.service', '--property=MainPID', '--value']))
    if pids['DisplayLinkManager'] <= 0:
        raise RuntimeError('DisplayLink service is not running')
    return {'heads': heads, 'pids': pids, 'monitor_listing': monitors}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--binary', type=Path, default=Path('revdi/target/release/vino-gpu'))
    parser.add_argument('--player', type=Path, default=Path('gpu-artifacts/official-comparison/live-display'))
    parser.add_argument('--seconds', type=float, default=10)
    parser.add_argument('--warmup', type=float, default=3)
    parser.add_argument('--pairs', type=int, default=3)
    parser.add_argument('--preflight-only', action='store_true')
    args = parser.parse_args()
    if not 0 < args.seconds <= 60 or not 0 <= args.warmup <= 30 or not 1 <= args.pairs <= 10:
        parser.error('Unsupported duration or repetitions')
    setup = preflight()
    if args.preflight_only:
        print(json.dumps(setup, indent=2))
        return
    args.output.mkdir(parents=True, exist_ok=False)
    binary, player = args.binary.resolve(), args.player.resolve()
    data = {
        'schema_version': 1, 'started_unix': time.time(), 'setup': setup,
        'method': 'same source pixels and nominal input cadence; live driver versus offline Vino GPU',
        'delivered_frame_rate_verified': False, 'quality_equivalence_verified': False,
        'seconds': args.seconds, 'warmup': args.warmup, 'pairs': args.pairs,
        'limitations': [
            'No dock/output completion counter, dropped-frame count, or screen-to-screen latency measurement',
            'Official service includes EVDI capture, codec selection, adaptive quality, transport and management',
            'Vino GPU includes its padding/cache/framing but no EVDI capture or USB transport',
            'Official quality, damage handling and frame selection are not forced to match Vino',
            'Static baseline holds one image; active cases submit full-window damage at each input frame',
            'Normal desktop, automatic clocks, short runs; no heat or power conclusion',
        ],
        'versions': command(['dpkg-query', '-W', 'displaylink-driver', 'evdi']),
        'kernel': command(['uname', '-r']).strip(),
        'sha256': {str(p): digest(p) for p in (binary, player, Path('/usr/lib/displaylink-driver/DisplayLinkManager'), Path(__file__))},
        'runs': [],
    }
    for case in ('idle', 'scroll', 'motion'):
        path = args.output / (case+'.bgrx')
        command([binary, 'export', '--case', case, '--output', path])
        data['sha256'][str(path)] = digest(path)
    def save():
        (args.output/'measurements.json').write_text(json.dumps(data, indent=2)+'\n')
    save()
    heads, pids = setup['heads'], setup['pids']
    cases = [('idle', 30, True), ('scroll', 30, False), ('motion', 10, False), ('motion', 30, False)]
    for repeat in range(args.pairs):
        for case, rate, static in cases:
            order = ['official'] if static else (['official', 'gpu'] if repeat % 2 == 0 else ['gpu', 'official'])
            for backend in order:
                current = preflight()
                if current != setup:
                    raise RuntimeError('Display layout or process identity changed; comparison stopped')
                label = f'{repeat}-{case}-{rate}-{backend}'
                print(f'Running {label}', flush=True)
                if backend == 'official':
                    argv = [player, args.output/(case+'.bgrx'), 1920, 1080, rate, args.warmup, args.seconds,
                            heads[0]['x'], heads[0]['y'], heads[1]['x'], heads[1]['y'], pids['Xorg'], pids['kwin_x11'],
                            int(static), 'checked-layout']
                    raw = command(argv, timeout=args.warmup+args.seconds+20)
                    measurement = json.loads(raw)
                    (args.output/(label+'.json')).write_text(json.dumps(measurement, indent=2)+'\n')
                    print(f"  DisplayLink {measurement['displaylink_cpu_cores']:.3f} cores; input {measurement['input_pairs_per_second']:.2f}/s", flush=True)
                else:
                    report = args.output/(label+'.json')
                    argv = [binary, 'sample', '--case', case, '--rate', rate, '--warmup', args.warmup,
                            '--seconds', args.seconds, '--output', report]
                    raw = command(argv, timeout=4*args.seconds+args.warmup+30)
                    measurement = json.loads(raw)['measurement']
                    print(f"  GPU prototype {measurement['average_cpu_cores']:.3f} CPU cores; completed {measurement['frame_pairs_per_second']:.2f}/s", flush=True)
                if preflight() != setup:
                    raise RuntimeError('Display layout or process identity changed during measurement')
                data['runs'].append({'repeat':repeat,'case':case,'target_rate':rate,'backend':backend,'command':[str(x) for x in argv],'measurement':measurement})
                save()
    data['finished_unix'] = time.time()
    data['summary'] = []
    for case, rate, static in cases:
        entry = {'case':case,'target_rate':rate,'static_hold':static}
        for backend in ('official','gpu'):
            rows=[r['measurement'] for r in data['runs'] if r['case']==case and r['target_rate']==rate and r['backend']==backend]
            if not rows:
                continue
            fields = ['displaylink_cpu_cores','input_pairs_per_second','xorg_cpu_cores','kwin_cpu_cores','player_cpu_cores'] if backend=='official' else ['average_cpu_cores','frame_pairs_per_second','p99_ms']
            entry[backend] = {field:{'mean':statistics.mean(row[field] for row in rows), 'min':min(row[field] for row in rows), 'max':max(row[field] for row in rows)} for field in fields}
        data['summary'].append(entry)
    save()
    print(json.dumps(data['summary'], indent=2), flush=True)


if __name__ == '__main__':
    main()
