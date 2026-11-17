#!/usr/bin/env python3
"""原生邮箱集中回归；隔离数据/配置，使用缓存 MIME，不访问网络或凭据。"""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, default=Path('target/release/mail-ui'))
parser.add_argument('--evidence', type=Path, default=Path('docs/reports/evidence/UI-001-mailbox.json'))
parser.add_argument('--screenshots', action='store_true', help='使用 grim/hyprctl 捕获探针窗口')
args = parser.parse_args()
args.evidence.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='qingmail-mailbox-ui-') as directory:
    root = Path(directory)
    root.chmod(0o700)
    data = root / 'drafts/p2-data'
    subprocess.run([str(Path('target/debug/examples/sync_fixture').resolve()), str(data), '--bodies'], check=True, timeout=15)
    env = dict(os.environ, QINGMAIL_P0_RUNTIME_ROOT=str(root), QINGMAIL_P0_DRAFT_ROOT=str(root / 'drafts'),
               XDG_CONFIG_HOME=str(root / 'config'), XDG_CACHE_HOME=str(root / 'cache'))
    env.setdefault('WAYLAND_DISPLAY', 'wayland-1')
    env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')
    log_path = args.evidence.with_suffix('.log')
    captures = []
    with log_path.open('w') as log:
        proc = subprocess.Popen([str(args.binary.resolve()), '--mailbox-ui-probe', '--no-tray'], env=env, stdout=log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 35
        floating = False
        try:
            while proc.poll() is None:
                assert time.monotonic() < deadline, 'native UI probe timed out'
                if args.screenshots:
                    clients = json.loads(subprocess.check_output(['hyprctl', '-j', 'clients']))
                    client = next((c for c in clients if c['pid'] == proc.pid), None)
                    if client and not floating:
                        address = 'address:' + client['address']
                        result = subprocess.run(['hyprctl', 'eval', 'hl.dispatch(hl.dsp.window.float({window=' + json.dumps(address) + ',action="on"}))'], capture_output=True, text=True)
                        assert result.returncode == 0, result.stdout + result.stderr
                        result = subprocess.run(['hyprctl', 'eval', 'hl.dispatch(hl.dsp.window.center({window=' + json.dumps(address) + '}))'], capture_output=True, text=True)
                        assert result.returncode == 0, result.stdout + result.stderr
                        floating = True
                    for mode in ['light', 'dark', 'narrow']:
                        if client and mode not in captures and f'QINGMAIL_UI_CAPTURE: {mode}' in log_path.read_text():
                            # 等待下一次合成后捕获，只操作本探针 PID 对应的窗口。
                            address = 'address:' + client['address']
                            if mode == 'narrow':
                                result = subprocess.run(['hyprctl', 'eval', 'hl.dispatch(hl.dsp.window.resize({window=' + json.dumps(address) + ',x=900,y=760}))'], capture_output=True, text=True)
                                assert result.returncode == 0, result.stdout + result.stderr
                            time.sleep(0.5)
                            clients = json.loads(subprocess.check_output(['hyprctl', '-j', 'clients']))
                            client = next(c for c in clients if c['pid'] == proc.pid)
                            x, y = client['at']; w, h = client['size']
                            if mode == 'narrow':
                                assert w < 1050, f'narrow capture requires narrow viewport: {w}'
                            print(mode, dict(floating=client['floating'], size=client['size']), flush=True)
                            subprocess.run(['grim', '-g', f'{x},{y} {w}x{h}', str(args.evidence.with_name('UI-001-' + mode + '.png'))], check=True)
                            captures.append(mode)
                time.sleep(0.05)
            assert proc.returncode == 0, log_path.read_text()
        finally:
            if proc.poll() is None:
                proc.terminate()
                proc.wait(timeout=5)
    assert 'QINGMAIL_MAILBOX_UI_PROBE: PASS' in log_path.read_text(), log_path.read_text()
    with sqlite3.connect(f'file:{data / "state.sqlite3"}?mode=ro', uri=True) as db:
        assert db.execute('PRAGMA user_version').fetchone()[0] == 8
        assert db.execute('SELECT count(*) FROM message_bodies').fetchone()[0] == 3
        assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
    if args.screenshots:
        assert captures == ['light', 'dark', 'narrow'], captures
    args.evidence.write_text(json.dumps(dict(passed=True, synthetic_only=True, schema=8, sends=0,
        screenshots=captures, checks=['default cached INBOX', 'click directly reads MIME', 'rapid selection shows latest only',
        'folder switch rejects stale body', 'search result directly reads', 'clear search restores selected reading',
        'page change clears previous body', 'light/dark/narrow native render'],
        boundary='Application entity integration and native rendering; does not assert real mailbox or pointer interaction.'), ensure_ascii=False, indent=2) + '\n')
print(f'PASS: {args.evidence}')
