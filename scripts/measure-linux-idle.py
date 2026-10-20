#!/usr/bin/env python3
"""Isolated graphical lifecycle/PSS check. Requires Linux, X11, dbus-run-session and a built shell/CLI.

Usage: python3 scripts/measure-linux-idle.py /absolute/path/digup-shell /absolute/path/digup
Model downloads and indexing are disabled; no personal configuration or index is opened.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def snapshot(pid):
    processes = {}
    for path in Path('/proc').glob('[0-9]*/status'):
        try:
            fields = dict(line.split(':', 1) for line in path.read_text().splitlines() if ':' in line)
            processes[int(path.parent.name)] = (int(fields['PPid']), fields['Name'].strip())
        except (OSError, ValueError):
            pass
    descendants = {pid}
    while True:
        more = {p for p, (parent, _) in processes.items() if parent in descendants}
        if more <= descendants:
            break
        descendants |= more
    rows = []
    for p in sorted(descendants):
        try:
            values = dict(line.split(':', 1) for line in Path(f'/proc/{p}/smaps_rollup').read_text().splitlines() if ':' in line)
            rows.append({'pid': p, 'name': processes[p][1], 'pssMiB': round(int(values['Pss'].split()[0]) / 1024, 2)})
        except (OSError, KeyError):
            pass
    return {'totalMiB': round(sum(row['pssMiB'] for row in rows), 2), 'processes': rows}


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    if '--private-session' not in sys.argv:
        return subprocess.run(['dbus-run-session', '--', sys.executable, str(Path(__file__).resolve()),
                               *sys.argv[1:], '--private-session']).returncode
    binary, helper = (str(Path(arg).resolve()) for arg in sys.argv[1:3])
    temp = '/tmp/opencode' if Path('/tmp/opencode').is_dir() else None
    root = Path(tempfile.mkdtemp(prefix='digup-idle-', dir=temp))
    env = dict(os.environ, HOME=str(root), XDG_CONFIG_HOME=str(root / 'config'),
               XDG_DATA_HOME=str(root / 'data'), XDG_CACHE_HOME=str(root / 'cache'),
               XDG_STATE_HOME=str(root / 'state'), DIGUP_MODELS=str(root / 'models'),
               HF_HOME=str(root / 'hf'), GDK_BACKEND='x11', XDG_SESSION_TYPE='x11',
               XDG_CURRENT_DESKTOP='', NO_AT_BRIDGE='1', DIGUP_TEST_HELPER=helper)
    env.pop('WAYLAND_DISPLAY', None)
    wrapper = root / 'helper'
    wrapper.write_text('''#!/bin/sh
case "$1" in
download-model) echo '{"event":"error","message":"isolated test: downloads disabled"}'; exit 0;;
speed) echo '{}'; exit 0;;
esac
exec "$DIGUP_TEST_HELPER" "$@"
''')
    wrapper.chmod(0o755)
    results = {'fixture': str(root), 'modes': {}}
    for warm in (True, False):
        settings = root / f'settings-{warm}.json'
        settings.write_text(json.dumps({'setUp': True, 'keepWarm': warm, 'updateChecksOff': True}))
        args = [binary, '--settings', str(settings), '--index', str(root / 'index'), '--helper', str(wrapper)]
        with (root / f'{warm}.log').open('w') as log:
            app = subprocess.Popen(args + ['--at-login'], env=env, stdout=log, stderr=log)
            try:
                time.sleep(6)
                assert app.poll() is None, 'shell failed to start'
                subprocess.run(args + ['--toggle'], env=env, check=True, timeout=15)
                time.sleep(5)
                opened = snapshot(app.pid)
                subprocess.run(args + ['--toggle'], env=env, check=True, timeout=15)
                time.sleep(40)
                idle = snapshot(app.pid)
                subprocess.run(args + ['--toggle'], env=env, check=True, timeout=15)
                time.sleep(6)
                reopened = snapshot(app.pid)
                results['modes'][str(warm)] = {'opened': opened, 'idle': idle, 'reopened': reopened}
                if not warm:
                    # Normal desktop activation opens the main window, not the panel. Closing that
                    # window must also release its cached WebKit process, then support activation again.
                    subprocess.run(args + ['--toggle'], env=env, check=True, timeout=15)
                    subprocess.run(args, env=env, check=True, timeout=15)
                    time.sleep(4)
                    windows = subprocess.check_output(['xdotool', 'search', '--all', '--onlyvisible',
                                                       '--pid', str(app.pid), '--name', '^DigUp$'], text=True).split()
                    assert len(windows) == 1, f'expected one visible main window: {windows}'
                    subprocess.run(['xdotool', 'windowactivate', '--sync', windows[-1],
                                    'key', '--clearmodifiers', 'ctrl+w'], check=True)
                    time.sleep(40)
                    main_idle = snapshot(app.pid)
                    (root / 'main-idle.json').write_text(json.dumps(main_idle, indent=2))
                    visible = subprocess.run(['xdotool', 'search', '--all', '--onlyvisible', '--pid', str(app.pid),
                                              '--name', '^DigUp$'], capture_output=True, text=True)
                    assert visible.returncode == 1, f'Ctrl+W did not close the main window: {visible.stdout}'
                    assert not any(row['name'] == 'digup' or 'WebKitWeb' in row['name'] for row in main_idle['processes']), f'closed main window retained resources: {main_idle}'
                    subprocess.run(args, env=env, check=True, timeout=15)
                    time.sleep(5)
                    main_reopened = snapshot(app.pid)
                    assert any('WebKitWeb' in row['name'] for row in main_reopened['processes']), 'main window did not reopen'
                    results['modes'][str(warm)].update(mainIdle=main_idle, mainReopened=main_reopened)
            finally:
                app.terminate()
                try:
                    app.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait()
        # The frontend's existing debug-report command invokes the same quit() as GUI Quit, not SIGTERM.
        quit_run = subprocess.run(args + ['--debug-window', '', '--debug-quit'], env=env,
                                  capture_output=True, text=True, timeout=45)
        (root / f'quit-{warm}.log').write_text(quit_run.stdout + quit_run.stderr)
        assert quit_run.returncode == 0 and quit_run.stdout.strip(), 'frontend quit path failed'
        results['modes'][str(warm)]['frontendQuitExit'] = quit_run.returncode
    (root / 'results.json').write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2), flush=True)
    cold = results['modes']['False']
    assert not any(row['name'] == 'digup' or 'WebKitWeb' in row['name'] for row in cold['idle']['processes']), 'idle worker/page survived'
    assert any(row['name'] == 'digup' for row in cold['reopened']['processes']), 'search did not restart'
    assert any('WebKitWeb' in row['name'] for row in cold['reopened']['processes']), 'page did not reopen'
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
