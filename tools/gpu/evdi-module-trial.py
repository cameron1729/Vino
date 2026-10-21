#!/usr/bin/env python3
"""Temporarily switch EVDI under systemd; switch/restore sign out the desktop."""
import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import pwd
import shutil
import subprocess
import sys
import tempfile
import time

SERVICE = 'displaylink-driver.service'
ACTIVE = Path('/run/vino-evdi-module-trial.json')
PARAMETER = Path('/sys/module/evdi/parameters/copy_batch_rows')
MASK = Path('/run/systemd/system') / SERVICE


def command(args, *, check=True, timeout=15):
    result = subprocess.run([str(a) for a in args], capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f'{args[0]}: {result.stderr.strip() or result.stdout.strip()}')
    return result


def save(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.chmod(0o644)
    temporary.replace(path)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def version():
    path = Path('/sys/module/evdi/srcversion')
    return path.read_text().strip() if path.exists() else None


def modinfo(field, path):
    return command(['modinfo', '-F', field, path]).stdout.strip()


def active(unit):
    return command(['systemctl', 'is-active', unit], check=False).stdout.strip() == 'active'


def layout_helper():
    spec = importlib.util.spec_from_file_location('live_trial', Path(__file__).with_name('live-trial.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def desktop_context():
    uid = int(os.environ.get('SUDO_UID', os.getuid()))
    if not uid:
        raise RuntimeError('Run from the desktop account, with sudo only for switch/restore')
    account = pwd.getpwuid(uid)
    context = dict(uid=uid, user=account.pw_name, gid=account.pw_gid,
                   display=os.environ.get('DISPLAY', ''), xauthority=os.environ.get('XAUTHORITY', ''),
                   runtime_dir=f'/run/user/{uid}')
    if not context['display'] or not Path(context['xauthority']).is_file():
        raise RuntimeError('Preserve DISPLAY and XAUTHORITY when invoking sudo')
    return context


def build_info(build):
    report = json.loads((build / 'build-report.json').read_text())
    if report['kernel'] != os.uname().release or not report['correctness']['passed']:
        raise RuntimeError('Build kernel or correctness checks do not match this machine')
    if not report.get('rebuilt_original_matches_installed_srcversion',
                      report.get('rebuilt_original_matches_loaded_srcversion', False)):
        raise RuntimeError('Build did not match the installed original EVDI source version')
    entry = report['modules']['candidate']
    if entry['name'] != 'evdi' or entry['relative_path'] != 'after/evdi.ko':
        raise RuntimeError('Unexpected candidate module identity')
    candidate = build / entry['relative_path']
    if digest(candidate) != entry['sha256'] or modinfo('name', candidate) != 'evdi':
        raise RuntimeError('Candidate module fingerprint mismatch')
    if modinfo('vermagic', candidate).split()[0] != os.uname().release:
        raise RuntimeError('Candidate module targets another kernel')
    if modinfo('srcversion', candidate) != entry['srcversion']:
        raise RuntimeError('Candidate source version differs from its build report')
    installed = Path(modinfo('filename', 'evdi'))
    original_version = report['modules']['original']['srcversion']
    if modinfo('srcversion', installed) != original_version:
        raise RuntimeError('Installed EVDI differs from the rebuilt original')
    previous = None
    if version() != original_version:
        previous = verified_previous_candidate(installed, original_version)
    return report, candidate, installed, previous


def protected_file(path):
    # An upgrade trusts only state staged by the earlier root supervisor. Neither the
    # state nor its containing directory may be writable by the desktop account.
    for entry in (path, path.parent):
        metadata = entry.lstat()
        if entry.is_symlink() or metadata.st_uid != 0 or metadata.st_mode & 0o022:
            raise RuntimeError('Previous EVDI trial state is not protected root-owned storage')


def verified_previous_candidate(installed, original_version):
    protected_file(ACTIVE)
    directory = Path(json.loads(ACTIVE.read_text())['directory'])
    path = directory / 'state.json'
    protected_file(path)
    previous = json.loads(path.read_text())
    if previous['kernel'] != os.uname().release or previous['goal'] != 'candidate' or previous['phase'] != 'complete':
        raise RuntimeError('The previous EVDI candidate switch is not complete for this kernel')
    if previous['versions']['original'] != original_version or previous['versions']['candidate'] != version():
        raise RuntimeError('Running EVDI does not match the previous temporary candidate')
    for name in ('original', 'candidate'):
        module = Path(previous[name])
        protected_file(module)
        if digest(module) != previous['hashes'][name]:
            raise RuntimeError('Previous staged module fingerprint mismatch')
    if digest(installed) != previous['hashes']['original']:
        raise RuntimeError('Installed original changed since the previous module switch')
    return previous


def exclusive(*, wait=False):
    lock = open('/run/lock/vino-live-trial.lock', 'a')
    fcntl.flock(lock, fcntl.LOCK_EX | (0 if wait else fcntl.LOCK_NB))
    return lock


def ensure_idle():
    units = command(['systemctl', 'list-units', '--all', '--no-legend',
                     'vino-trial-*', 'vino-recover-*', 'vino-module-*'], check=False).stdout
    if units.strip():
        raise RuntimeError('A Vino trial, module switch or recovery unit still exists')


def stage(goal, build=None):
    if os.geteuid():
        raise RuntimeError('Switch/restore needs sudo in your terminal')
    os.environ['PATH'] = '/usr/sbin:/usr/bin:/sbin:/bin'
    with exclusive():
        ensure_idle()
        dm = command(['systemctl', 'show', 'display-manager.service', '-p', 'Id', '--value']).stdout.strip()
        if dm != 'sddm.service':
            raise RuntimeError('This procedure has only been prepared for the current SDDM desktop')
        if not active(dm) or not active(SERVICE):
            raise RuntimeError('Start with the normal display manager and DisplayLink service active')
        if MASK.exists() or MASK.is_symlink():
            raise RuntimeError('An existing runtime DisplayLink override must be left alone')
        if command(['mokutil', '--sb-state']).stdout.strip() != 'SecureBoot disabled':
            raise RuntimeError('This unsigned temporary build requires the existing disabled Secure Boot state')
        context = desktop_context()
        helper = layout_helper()
        outputs = helper.outputs(context)
        if not any(o['mode'] and o['name'].startswith(('eDP', 'LVDS')) for o in outputs):
            raise RuntimeError('The laptop panel must be active before the switch')
        if goal == 'candidate':
            report, candidate, installed, previous = build_info(build.resolve())
            expected = {name: report['modules'][name]['srcversion'] for name in ['original', 'candidate']}
            if previous is not None:
                # Permit removal of precisely the already verified older candidate while
                # retaining the installed original as the rollback target for this switch.
                expected['previous'] = previous['versions']['candidate']
            original_hash = digest(installed)
            candidate_hash = report['modules']['candidate']['sha256']
            params = {name: int((Path('/sys/module/evdi/parameters') / name).read_text())
                      for name in ['initial_device_count', 'initial_loglevel']}
        else:
            previous = json.loads((Path(json.loads(ACTIVE.read_text())['directory']) / 'state.json').read_text())
            if previous['kernel'] != os.uname().release:
                raise RuntimeError('Kernel changed; the previous temporary module is no longer this boot')
            expected, params = previous['versions'], previous['parameters']
            permitted = {expected['candidate']}
            if 'previous' in expected:
                # A failed systemd launch can leave the verified older candidate running
                # after staging an upgrade; restore still targets the true original.
                permitted.add(expected['previous'])
            if version() not in permitted:
                raise RuntimeError('The temporary candidate is not currently loaded')
            installed, candidate = Path(previous['original']), Path(previous['candidate'])
            original_hash, candidate_hash = previous['hashes']['original'], previous['hashes']['candidate']
            report = previous['build_report']
        if digest(installed) != original_hash or digest(candidate) != candidate_hash:
            raise RuntimeError('Module files changed; refusing to switch')
        directory = Path(tempfile.mkdtemp(prefix='vino-evdi-module-', dir='/var/tmp'))
        directory.chmod(0o755)
        private = directory / 'private'
        private.mkdir(mode=0o700)
        old = private / ('original.ko.zst' if installed.suffix == '.zst' else 'original.ko')
        new = private / 'candidate.ko'
        shutil.copyfile(installed, old)
        shutil.copyfile(candidate, new)
        shutil.copyfile(__file__, directory / 'helper.py')
        for path in [old, new, directory / 'helper.py']:
            path.chmod(0o600)
        if digest(old) != original_hash or digest(new) != candidate_hash:
            raise RuntimeError('Staged module fingerprint mismatch')
        state = dict(goal=goal, kernel=os.uname().release, versions=expected, parameters=params,
                     original=str(old), candidate=str(new), display_manager=dm,
                     hashes=dict(original=original_hash, candidate=candidate_hash),
                     phase='prepared', mask_added=False, stop_started=False, outputs=outputs,
                     uid=context['uid'], build_report=report)
        save(directory / 'state.json', state)
        save(ACTIVE, dict(directory=str(directory)))
        print(f'Switch logs: {directory}', flush=True)
        print('Signing out to switch EVDI; systemd will bring the login screen back', flush=True)
        command(unit_command(directory))
        return directory


def unit_command(directory, *, mock=False):
    helper = f'/usr/bin/python3 {directory}/helper.py'
    args = ['systemd-run', *(['--user'] if mock else []), '--quiet', '--no-block', '--collect',
            '--unit', f'vino-module-switch-{directory.name}']
    for value in ['Type=oneshot', 'Restart=no', 'KillMode=control-group', 'LimitCORE=0',
                  f'TimeoutStartSec={2 if mock else 120}s', 'TimeoutStopSec=3s',
                  f'ExecStopPost={helper} _recover-start {directory}']:
        args += ['--property', value]
    return args + ['--', '/usr/bin/python3', str(directory / 'helper.py'), '_step', str(directory)]


def unmask(state):
    if not state['mask_added']:
        return
    if MASK.is_symlink() and MASK.readlink() == Path('/dev/null'):
        command(['systemctl', 'unmask', '--runtime', SERVICE])
    elif MASK.exists() or MASK.is_symlink():
        raise RuntimeError('Runtime service override changed; refusing to remove it')


def stop_graphics(directory, state):
    # Record ownership/stop intent before either operation can be interrupted.
    state['mask_added'] = True
    state['stop_started'] = True
    save(directory / 'state.json', state)
    command(['systemctl', 'mask', '--runtime', SERVICE])
    command(['systemctl', 'stop', SERVICE, state['display_manager']], timeout=40)
    for unit in [SERVICE, state['display_manager']]:
        pid = command(['systemctl', 'show', unit, '-p', 'MainPID', '--value']).stdout.strip()
        if pid != '0':
            raise RuntimeError(f'{unit} did not stop')


def load_module(state, goal):
    current = version()
    if current is not None and current not in state['versions'].values():
        raise RuntimeError('An unexpected EVDI module is loaded; refusing to remove it')
    if current != state['versions'][goal]:
        path = Path(state[goal])
        if digest(path) != state['hashes'][goal]:
            raise RuntimeError(f'{goal} module checksum mismatch')
        if current is not None:
            # Never force an unload or remove amdgpu/other dependencies.
            command(['rmmod', 'evdi'])
        parameters = [f'{key}={value}' for key, value in state['parameters'].items()]
        if goal == 'candidate':
            parameters.append('copy_batch_rows=0')
        command(['insmod', path, *parameters])
    if version() != state['versions'][goal]:
        raise RuntimeError('Loaded module source version does not match the intended module')
    if goal == 'candidate':
        PARAMETER.write_text('0\n')
        if PARAMETER.read_text().strip() != '0':
            raise RuntimeError('Could not leave batching disabled outside the trial')


def start_graphics(state):
    unmask(state)
    command(['systemctl', 'start', state['display_manager'], SERVICE], timeout=40)
    if not active(state['display_manager']) or not active(SERVICE):
        raise RuntimeError('Display services did not become active')


def step(directory):
    state = json.loads((directory / 'state.json').read_text())
    if state.get('mock'):
        if state['case'] == 'hang':
            time.sleep(30)
        if state['case'] == 'fail':
            raise RuntimeError('Intentional mock failure')
    else:
        if os.geteuid():
            raise RuntimeError('Root is required for the module switch')
        with exclusive(wait=True):
            stop_graphics(directory, state)
            load_module(state, state['goal'])
            start_graphics(state)
    state['phase'] = 'complete'
    save(directory / 'state.json', state)


def recover_start(directory):
    state = json.loads((directory / 'state.json').read_text())
    result = os.environ.get('SERVICE_RESULT', 'unknown')
    if result == 'success' and state['phase'] == 'complete':
        save(directory / 'recovery.json', dict(switched=True, errors=[], fallback_used=False))
        return
    # Separate service: recovery survives the failed switch unit's stop timeout.
    args = ['systemd-run', *(['--user'] if state.get('mock') else []), '--quiet', '--no-block', '--collect',
            '--unit', f'vino-module-recover-{directory.name}',
            '--property', 'Type=oneshot', '--property', 'TimeoutStartSec=120s',
            '--property', 'TimeoutStopSec=3s', '--property', 'KillMode=control-group',
            '--setenv', f'VINO_SWITCH_RESULT={result}', '--', '/usr/bin/python3',
            str(directory / 'helper.py'), '_recover', str(directory)]
    command(args, timeout=2)


def recover(directory):
    state = json.loads((directory / 'state.json').read_text())
    outcome = dict(switched=False, fallback_used=True, errors=[], result=os.environ.get('VINO_SWITCH_RESULT'))
    if state.get('mock'):
        outcome['original_restored'] = True
    else:
        if os.geteuid():
            raise RuntimeError('Root is required for module recovery')
        with exclusive(wait=True):
            if state['stop_started']:
                try:
                    stop_graphics(directory, state)
                    load_module(state, 'original')
                except Exception as error:
                    outcome['errors'].append(str(error))
                # Even a failed unload must not prevent bringing the login screen back.
                try:
                    start_graphics(state)
                except Exception as error:
                    outcome['errors'].append(str(error))
            outcome['original_restored'] = version() == state['versions']['original']
    save(directory / 'recovery.json', outcome)
    if outcome['errors'] or not outcome['original_restored']:
        raise RuntimeError(f'Recovery needs attention: {outcome}')


def latest():
    directory = Path(json.loads(ACTIVE.read_text())['directory'])
    return directory, json.loads((directory / 'state.json').read_text())


def status():
    directory, state = latest()
    print(json.dumps(dict(directory=str(directory), intended=state['goal'], phase=state['phase'],
                          loaded_evdi=version(), expected_versions=state['versions'],
                          copy_batch_rows=PARAMETER.read_text().strip() if PARAMETER.exists() else None,
                          shared_capture_abi=(Path('/sys/module/evdi/parameters/shared_capture_abi').read_text().strip()
                                              if Path('/sys/module/evdi/parameters/shared_capture_abi').exists() else None),
                          display_manager_active=active(state['display_manager']),
                          displaylink_active=active(SERVICE),
                          recovery=json.loads((directory / 'recovery.json').read_text())
                          if (directory / 'recovery.json').exists() else None), indent=2))


def preflight(build):
    report, candidate, installed, previous = build_info(build.resolve())
    helper = layout_helper()
    outputs = helper.outputs(desktop_context())
    if not active('sddm.service') or not active(SERVICE):
        raise RuntimeError('Expected the normal desktop and official driver to be active')
    ensure_idle()
    print(json.dumps(dict(kernel=os.uname().release, loaded_evdi=version(),
                          candidate_srcversion=report['modules']['candidate']['srcversion'],
                          candidate_sha256=digest(candidate), installed_module=str(installed),
                          installed_sha256=digest(installed),
                          current_layout=[o for o in outputs if o['mode']],
                          upgrades_temporary_candidate=previous is not None,
                          requires_logout=True, hardware_changed=False), indent=2))


def restore_layout():
    directory, state = latest()
    context = desktop_context()
    if context['uid'] != state['uid']:
        raise RuntimeError('The saved layout belongs to a different desktop account')
    helper = layout_helper()
    helper.desktop_command(context, helper.layout_arguments(state['outputs'], helper.outputs(context)))
    current = helper.outputs(context)
    for old in state['outputs']:
        if old['mode']:
            new = helper.match_output(old, current)
            if any(new[key] != old[key] for key in ['mode', 'x', 'y', 'primary', 'rate']):
                raise RuntimeError('Desktop layout has not settled to its saved geometry')
    print('Saved desktop layout restored')


def self_test():
    for case in ['success', 'fail', 'hang']:
        directory = Path(tempfile.mkdtemp(prefix=f'vino-module-mock-{case}-'))
        shutil.copyfile(__file__, directory / 'helper.py')
        save(directory / 'state.json', dict(mock=True, case=case, phase='prepared'))
        command(unit_command(directory, mock=True))
        deadline = time.monotonic() + 20
        while not (directory / 'recovery.json').exists() and time.monotonic() < deadline:
            time.sleep(0.1)
        outcome = json.loads((directory / 'recovery.json').read_text())
        assert not outcome['errors']
        assert outcome['fallback_used'] == (case != 'success')
        print(json.dumps(dict(case=case, directory=str(directory), outcome=outcome)))


def main():
    if len(sys.argv) == 3 and sys.argv[1] in ['_step', '_recover-start', '_recover']:
        {'_step': step, '_recover-start': recover_start, '_recover': recover}[sys.argv[1]](Path(sys.argv[2]))
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['preflight', 'switch', 'restore', 'status', 'restore-layout', 'self-test'])
    parser.add_argument('--build', type=Path)
    args = parser.parse_args()
    if args.action in ['switch', 'preflight']:
        if not args.build:
            parser.error(f'{args.action} requires --build')
        if args.action == 'switch':
            stage('candidate', args.build)
        else:
            preflight(args.build)
    elif args.action == 'restore':
        stage('original')
    else:
        {'status': status, 'restore-layout': restore_layout, 'self-test': self_test}[args.action]()


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'evdi-module-trial: {error}', file=sys.stderr)
        sys.exit(1)
