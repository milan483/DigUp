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


def visible_window(pid):
    windows = subprocess.check_output(['xdotool', 'search', '--all', '--onlyvisible',
                                       '--pid', str(pid), '--name', '^DigUp$'], text=True).split()
    assert len(windows) == 1, f'expected one visible window: {windows}'
    return windows[0]


def search_response(pid, root, query):
    window = visible_window(pid)
    subprocess.run(['xdotool', 'windowactivate', '--sync', window, 'key', '--clearmodifiers', 'ctrl+a',
                    'type', '--clearmodifiers', query], check=True)
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        replies = root / 'search-replies.jsonl'
        for line in replies.read_text().splitlines() if replies.exists() else []:
            reply = json.loads(line)
            if reply['query'] == query:
                assert reply['response'].get('ok') is not False, reply
                assert isinstance(reply['response'].get('hits'), list), reply
                return reply
        time.sleep(0.1)
    raise AssertionError(f'no successful frontend search response for {query}')


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
    print(f'Isolated fixture: {root}', file=sys.stderr, flush=True)
    env = dict(os.environ, HOME=str(root), XDG_CONFIG_HOME=str(root / 'config'),
               XDG_DATA_HOME=str(root / 'data'), XDG_CACHE_HOME=str(root / 'cache'),
               XDG_STATE_HOME=str(root / 'state'), DIGUP_MODELS=str(root / 'models'),
               HF_HOME=str(root / 'hf'), GDK_BACKEND='x11', XDG_SESSION_TYPE='x11',
               XDG_CURRENT_DESKTOP='', NO_AT_BRIDGE='1', DIGUP_TEST_HELPER=helper)
    env.pop('WAYLAND_DISPLAY', None)
    wrapper = root / 'helper'
    # Proxy only search-service, recording actual request/reply pairs rather than inferring
    # successful search from a live worker. The real bundled engine still answers every request.
    # AppRun exports its bundled Python paths. The system-Python test proxy must
    # ignore those paths without stripping the packaged real helper's environment.
    wrapper.write_text('#!' + sys.executable + ' -E\n' + '''import json, os, subprocess, sys, threading
from pathlib import Path
if sys.argv[1] == 'download-model':
    print('{"event":"error","message":"isolated test: downloads disabled"}')
    sys.exit(0)
if sys.argv[1] == 'speed':
    print('{}')
    sys.exit(0)
if sys.argv[1] != 'search-service':
    os.execv(os.environ['DIGUP_TEST_HELPER'], [os.environ['DIGUP_TEST_HELPER'], *sys.argv[1:]])
child = subprocess.Popen([os.environ['DIGUP_TEST_HELPER'], *sys.argv[1:]],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
requests = {}
def send():
    for line in sys.stdin:
        request = json.loads(line)
        requests[request.get('id')] = request
        child.stdin.write(line)
        child.stdin.flush()
    child.stdin.close()
threading.Thread(target=send, daemon=True).start()
for line in child.stdout:
    response = json.loads(line)
    request = requests.pop(response.get('id'), {})
    if request.get('op') == 'search':
        with (Path(os.environ['HOME']) / 'search-replies.jsonl').open('a') as log:
            log.write(json.dumps({'query': request.get('q'), 'response': response}) + '\\n')
    print(line, end='', flush=True)
sys.exit(child.wait())
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
                results['modes'][str(warm)]['postReopenSearch'] = search_response(
                    app.pid, root, f'reopen-{warm}')
                if not warm:
                    # Exercise both sides of the 30s hidden-page cleanup boundary. D-Bus
                    # activation is off-main; each activation must survive cleanup and answer.
                    boundary = []
                    for number, delay in enumerate((29.8, 30.2, 35.1)):
                        subprocess.run(args + ['--toggle'], env=env, check=True, timeout=15)
                        time.sleep(delay)
                        subprocess.run(args + ['--toggle'], env=env, check=True, timeout=15)
                        time.sleep(2)
                        boundary.append(search_response(app.pid, root, f'boundary-{number}'))
                    results['modes'][str(warm)]['cleanupBoundarySearches'] = boundary
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
                    results['modes'][str(warm)]['mainReopenSearch'] = search_response(
                        app.pid, root, 'main-reopen')
            finally:
                app.terminate()
                try:
                    app.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait()
        # The frontend's existing debug-report command invokes the same quit() as GUI Quit, not SIGTERM.
        with (root / f'quit-{warm}.log').open('w') as log:
            quit_app = subprocess.Popen(args + ['--debug-window', f'quit-{warm}', '--debug-quit'],
                                        env=env, stdout=log, stderr=log)
            tracked = set()
            deadline = time.monotonic() + 45
            while quit_app.poll() is None and time.monotonic() < deadline:
                tracked.update(row['pid'] for row in snapshot(quit_app.pid)['processes'])
                time.sleep(0.05)
            try:
                assert quit_app.wait(timeout=1) == 0, 'frontend quit path failed'
            finally:
                if quit_app.poll() is None:
                    quit_app.kill()
                    quit_app.wait()
            report = (root / f'quit-{warm}.log').read_text()
            assert any(json.loads(line).get('query') == f'quit-{warm}'
                       for line in report.splitlines() if line.startswith('{')), report
            assert any(reply['query'] == f'quit-{warm}'
                       and reply['response'].get('ok') is not False
                       and isinstance(reply['response'].get('hits'), list)
                       for reply in map(json.loads, (root / 'search-replies.jsonl').read_text().splitlines())), \
                'frontend Quit did not follow a successful real engine reply'
            deadline = time.monotonic() + 10
            while any(Path(f'/proc/{pid}').exists() for pid in tracked) and time.monotonic() < deadline:
                time.sleep(0.1)
            remaining = [pid for pid in tracked if Path(f'/proc/{pid}').exists()]
            assert not remaining, f'descendants survived actual frontend Quit: {remaining}'
            results['modes'][str(warm)].update(frontendQuitExit=quit_app.returncode,
                                              quitTrackedPids=sorted(tracked), quitSurvivors=remaining)
    (root / 'results.json').write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2), flush=True)
    cold = results['modes']['False']
    assert not any(row['name'] == 'digup' or 'WebKitWeb' in row['name'] for row in cold['idle']['processes']), 'idle worker/page survived'
    assert any(row['name'] == 'digup' for row in cold['reopened']['processes']), 'search did not restart'
    assert any('WebKitWeb' in row['name'] for row in cold['reopened']['processes']), 'page did not reopen'
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
