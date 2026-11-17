#!/usr/bin/env python3
"""P3 production GUI + independent daemon E2E, isolated data and no mail/keyring access."""
import argparse
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, default=Path('target/release/mail-ui'))
parser.add_argument('--fixture', type=Path, default=Path('target/debug/examples/sync_fixture'))
parser.add_argument('--evidence', type=Path, default=Path('docs/reports/evidence/P3-002-split.json'))
args = parser.parse_args()
binary = args.binary.resolve()
evidence = args.evidence.resolve()
evidence.parent.mkdir(parents=True, exist_ok=True)
checks = []
processes = []
backend_pids = set()


def until(fn, timeout=25):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = fn()
            if last:
                return last
        except (OSError, AssertionError, KeyError, StopIteration, ValueError):
            pass
        time.sleep(.15)
    raise AssertionError(f'timed out: {fn}; last={last}; log={evidence.with_suffix(".log")}')


def record(name, **facts):
    checks.append(dict(check=name, passed=True, **facts))
    evidence.write_text(json.dumps(dict(passed=False, synthetic_only=True, checks=checks), ensure_ascii=False, indent=2)+'\n')


with tempfile.TemporaryDirectory(prefix='qingmail-p3-split-') as directory, evidence.with_suffix('.log').open('w') as log:
    root = Path(directory)
    root.chmod(0o700)
    subprocess.run([str(args.fixture.resolve()), str(root/'data'), '--bodies'], check=True, timeout=15, stdout=log)
    env = dict(os.environ, QINGMAIL_IPC_ROOT=str(root), QINGMAIL_IPC_TEST_TRAY='1', XDG_CONFIG_HOME=str(root/'config'), XDG_CACHE_HOME=str(root/'cache'))
    for key in ['QINGMAIL_P0_RUNTIME_ROOT', 'QINGMAIL_P0_DRAFT_ROOT', 'QINGMAIL_PERF_PROBE']:
        env.pop(key, None)
    env.setdefault('WAYLAND_DISPLAY', 'wayland-1')
    env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')

    def launch(**extra):
        proc = subprocess.Popen([str(binary)], env=dict(env, **extra), stdout=log, stderr=subprocess.STDOUT)
        processes.append(proc)
        return proc

    def rpc(kind, params=None, version=1):
        request = dict(version=version, id=secrets.token_hex(16), command=dict(type=kind))
        if params is not None:
            request['command']['args'] = params
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(6)
            stream.connect(str(root/'runtime/qingmail/daemon.sock'))
            stream.sendall(json.dumps(request).encode()+b'\n')
            response = json.loads(stream.makefile('rb').readline(2*1024*1024))
        return response['result']

    def reply(kind, params=None):
        return rpc(kind, params)['Ok']

    def status():
        return reply('status')['data']

    def windows(proc):
        clients = json.loads(subprocess.check_output(['hyprctl', '-j', 'clients']))
        return [c for c in clients if c['pid'] == proc.pid]

    def close_window(client):
        result = subprocess.run(['hyprctl', 'eval', 'hl.dispatch(hl.dsp.window.close({window=' + json.dumps('address:'+client['address']) + '}))'], capture_output=True, text=True)
        assert result.returncode == 0, result.stdout+result.stderr

    def close_front(proc):
        for client in windows(proc):
            close_window(client)
        until(lambda: proc.poll() is not None)
        assert proc.returncode == 0

    def daemon_ready():
        value = status()
        backend_pids.add(value['pid'])
        return value

    def saved():
        with sqlite3.connect(f'file:{root / "data/state.sqlite3"}?mode=ro', uri=True) as db:
            rows = db.execute('SELECT body_id FROM drafts').fetchall()
            assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
        documents = [json.loads((root/'data/objects'/row[0]).read_text()) for row in rows]
        return next(d for d in documents if d['text'] == '生命周期验证🙂\n关闭前最新草稿')

    try:
        first = launch(QINGMAIL_IPC_TEST_MAILBOX='1', QINGMAIL_IPC_TEST_EDITOR='1')
        backend = until(daemon_ready)['pid']
        clients = until(lambda: windows(first) if len(windows(first)) >= 2 else None)
        assert all(not c['xwayland'] for c in clients)
        assert backend != first.pid
        (root/'edit-trigger').touch()
        until(lambda: 'QINGMAIL_IPC_EDITOR: saved and closed through daemon' in evidence.with_suffix('.log').read_text())
        draft = saved()
        assert all(a['bold'] for a in draft['attributes'][:-1])
        record('production IPC editor immediate close, IME veto, formatted draft saved', backend_pid=backend, frontend_pid=first.pid)
        until(lambda: 'QINGMAIL_MAILBOX_UI_PROBE: PASS' in evidence.with_suffix('.log').read_text())
        record('production IPC cached reading, selection races, folder/search/pages and native M3E rendering', xwayland=False)
        close_front(first)
        assert status()['pid'] == backend
        record('main window close exits GPUI; daemon survives')
        cache = reply('cache_status')['data']
        assert cache['budget_bytes'] == 1073741824
        assert cache['evictable_bytes'] > 0
        cleared = reply('set_cache_budget', dict(bytes=0))['data']
        assert cleared['evictable_bytes'] == 0
        assert cleared['protected_bytes'] > 0
        assert saved()['text'] == draft['text']
        reply('clear_cache')
        reply('set_cache_budget', dict(bytes=1073741824))
        record('cache IPC budget and clear preserve formatted draft, indexes and send safety', protected_bytes=cleared['protected_bytes'])

        tray_state = until(lambda: s if '正在检查' not in (s := status())['tray_status'] else None)['tray_status']
        tray_available = '已注册' in tray_state
        if tray_available:
            subprocess.run(['busctl','--user','call',f'org.kde.StatusNotifierItem-{backend}-1','/StatusNotifierItem','org.kde.StatusNotifierItem','Activate','ii','0','0'],check=True,timeout=5,stdout=log)
            opened = until(lambda: next((c for c in json.loads(subprocess.check_output(['hyprctl','-j','clients'])) if c['pid'] not in {p.pid for p in processes} and f'PPid:\t{backend}\n' in Path(f'/proc/{c["pid"]}/status').read_text()),None))
            tray_pid = opened['pid']
            assert tray_pid != first.pid
            close_window(opened)
            until(lambda: not any(c['pid']==tray_pid for c in json.loads(subprocess.check_output(['hyprctl','-j','clients']))))
            assert status()['pid'] == backend
            record('daemon tray opens frontend after GPUI exit', tray_status=tray_state)
        else:
            record('no tray host exposes explicit reopen and quit fallback', tray_status=tray_state)

        reopened = launch()
        until(lambda: windows(reopened))
        second = launch()
        until(lambda: second.poll() is not None)
        assert second.returncode == 0 and reopened.poll() is None
        assert not windows(second) and status()['pid'] == backend
        record('reopen and second instance activate the existing frontend')
        reopened.kill(); reopened.wait(timeout=10)
        assert status()['pid'] == backend
        # A crashed lease must expire, not authorize a pending shutdown or another sender.
        time.sleep(16)
        recovered = launch()
        until(lambda: windows(recovered))
        assert saved()['text'] == draft['text']
        record('frontend crash preserves backend and saved draft; expired lease can reopen')

        os.kill(backend, signal.SIGKILL)
        time.sleep(1)
        daemon = subprocess.Popen([str(binary.with_name('mail-daemon')), '--isolated-root', str(root)], env=env, stdout=log, stderr=subprocess.STDOUT)
        processes.append(daemon)
        restarted = until(daemon_ready)['pid']
        assert restarted != backend
        # Observe reconnection through an activation consumed by the existing GPUI.
        until(lambda: rpc('activate', dict(target='send_tasks')).get('Ok'))
        until(lambda: len(windows(recovered)) >= 2)
        close_front(recovered)
        assert status()['pid'] == restarted
        assert saved()['text'] == draft['text']
        record('daemon crash/restart reconnects existing UI without resubmitting writes', restarted_pid=restarted)
        assert rpc('new_draft', dict(account=None, document={}), version=99)['Err']['code'] == 'version_mismatch'
        record('incompatible protocol rejects writes before dispatch')

        # Backend loss during editor close must veto both editor and frontend exit.
        (root/'edit-trigger').unlink()
        failure = launch(QINGMAIL_IPC_TEST_EDITOR='1')
        until(lambda: len(windows(failure)) >= 2)
        time.sleep(2)  # Restore the editor before interrupting storage.
        os.kill(restarted, signal.SIGKILL)
        (root/'edit-trigger').touch()
        time.sleep(8)
        assert failure.poll() is None and len(windows(failure)) >= 2
        main = next(c for c in windows(failure) if c['title'] == 'QingMail — 邮件')
        close_window(main)
        time.sleep(1)
        assert failure.poll() is None and len(windows(failure)) >= 2
        assert all(a['bold'] for a in saved()['attributes'][:-1])
        record('save failure retains editor and vetoes main close; last confirmed draft unchanged')
        daemon.wait(timeout=10)
        daemon = subprocess.Popen([str(binary.with_name('mail-daemon')), '--isolated-root', str(root)], env=env, stdout=log, stderr=subprocess.STDOUT)
        processes.append(daemon)
        restarted = until(daemon_ready)['pid']
        until(lambda: rpc('activate', dict(target='mailbox')).get('Ok'))
        time.sleep(2)
        close_front(failure)
        assert not any(a['bold'] for a in saved()['attributes'][:-1])
        record('explicit close after backend recovery saves retained edits')

        pss = next(line.strip() for line in Path(f'/proc/{restarted}/smaps_rollup').read_text().splitlines() if line.startswith('Pss:'))
        def ticks():
            fields = Path(f'/proc/{restarted}/stat').read_text().rsplit(')', 1)[1].split()
            return int(fields[11])+int(fields[12])
        start = time.monotonic(); t0 = ticks(); time.sleep(2); elapsed = time.monotonic()-start
        cpu = (ticks()-t0)/os.sysconf('SC_CLK_TCK')/elapsed*100
        record('backend baseline after frontend close', pss=pss, idle_cpu_percent=round(cpu, 2), interval_seconds=round(elapsed, 2))
        if '已注册' in status()['tray_status']:
            subprocess.run(['busctl','--user','call',f'org.kde.StatusNotifierItem-{restarted}-1','/Menu','com.canonical.dbusmenu','Event','isvu','2','clicked','s','','0'],check=True,timeout=5,stdout=log)
        else:
            reply('prepare_shutdown')
        daemon.wait(timeout=10)
        assert daemon.returncode == 0
        record('explicit shutdown stops daemon and removes socket')
        evidence.write_text(json.dumps(dict(passed=True, synthetic_only=True, mail_network=False, keyring=False, sends=0, checks=checks,
            limits=['No real IMAP/SMTP/Secret Service credentials used', 'Native event/entity integration, not physical pointer typing']),ensure_ascii=False,indent=2)+'\n')
    finally:
        for proc in processes:
            if proc.poll() is None:
                proc.kill(); proc.wait(timeout=10)
        for pid in backend_pids:
            try:
                # PID must still belong to our private backend before cleanup.
                command = Path(f'/proc/{pid}/cmdline').read_bytes()
                if str(root).encode() in command:
                    os.kill(pid, signal.SIGKILL)
            except (OSError, ProcessLookupError):
                pass
print(f'PASS: {evidence}')
