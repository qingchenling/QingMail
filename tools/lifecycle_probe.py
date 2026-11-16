#!/usr/bin/env python3
"""真实 mail-ui 进程/Wayland 生命周期探针；只在隔离目录操作合成草稿。"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import sqlite3
import socket
import stat
import subprocess
import tempfile
import time


def run(binary, evidence):
    checks = []
    processes = []
    log = evidence.with_suffix('.log')
    with tempfile.TemporaryDirectory(prefix='qingmail-p2-lifecycle-') as directory, log.open('w') as output:
        root = Path(directory)
        root.chmod(0o700)
        drafts = root / 'drafts'
        drafts.mkdir(mode=0o700)
        env = dict(os.environ, QINGMAIL_P0_RUNTIME_ROOT=str(root), QINGMAIL_P0_DRAFT_ROOT=str(drafts))
        env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')

        def launch(*args):
            proc = subprocess.Popen([str(binary), *args], env=env, stdout=output, stderr=subprocess.STDOUT)
            processes.append(proc)
            return proc

        def control(*args):
            result = subprocess.run([str(binary), *args], env=env, text=True, capture_output=True, timeout=5)
            assert result.returncode == 0, (args, result.stderr)
            return result.stdout

        def status():
            reply = control('--status')
            return {key: value for key, value in (item.split('=', 1) for item in reply.split()[1:] if '=' in item)}

        def until(predicate, timeout=15):
            deadline = time.monotonic() + timeout
            last = None
            while time.monotonic() < deadline:
                try:
                    last = predicate()
                    if last:
                        return last
                except (AssertionError, FileNotFoundError, json.JSONDecodeError, ConnectionError):
                    pass
                time.sleep(0.1)
            raise AssertionError(f'timed out; last={last}; see {log}')

        def record(name, **facts):
            checks.append(dict(check=name, passed=True, **facts))
            evidence.write_text(json.dumps({'checks': checks}, ensure_ascii=False, indent=2) + '\n')

        try:
            proc = launch('--lifecycle-probe', '--no-tray')
            closed = until(lambda: s if (s := status()).get('windows') == '0' and s.get('probe_step') == '4' else None)
            assert int(closed['pid']) == proc.pid and proc.poll() is None
            draft_dir = drafts / 'p2-data'
            def saved_draft():
                with sqlite3.connect(f'file:{draft_dir / "state.sqlite3"}?mode=ro', uri=True) as db:
                    row = db.execute('SELECT d.body_id FROM drafts d JOIN local_editor e ON e.draft_id=d.id AND e.account_id=d.account_id').fetchone()
                return json.loads((draft_dir / 'objects' / row[0]).read_text())
            draft = saved_draft()
            assert draft['text'] == '生命周期验证🙂\n关闭前最新草稿'
            assert all(a['bold'] for a in draft['attributes'][:-1])
            before = int(closed['ticks'])
            until(lambda: int(status()['ticks']) >= before + 3)
            record('close flushes latest text and formatting; no-window background continues', pid=proc.pid, ticks_before=before, ticks_after=status()['ticks'])
            assert stat.S_IMODE((root / 'qingmail-p0').stat().st_mode) == 0o700
            assert stat.S_IMODE((root / 'qingmail-p0/activate.sock').stat().st_mode) == 0o600
            record('runtime directory/socket permissions', directory='0700', socket='0600')

            # 同时启动 8 个短命激活进程，不允许创建第二套后台或重复编辑窗口。
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                replies = list(pool.map(lambda _: control('--editor-demo'), range(8)))
            assert all(f'pid={proc.pid}' in reply for reply in replies)
            until(lambda: status().get('editors') == '1')
            assert status()['pid'] == str(proc.pid)
            record('eight concurrent relaunches reuse one process and one editor', pid=proc.pid)
            control()
            until(lambda: status().get('windows') == '2')
            record('relaunch recreates main window alongside restored editor')

            # 非法及超长命令不能阻塞 UI/后台或执行动作。
            for payload in [b'QMP1 OPEN\n', b'QMP0 EXEC /bin/sh\n', b'x'*32]:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                    client.settimeout(1)
                    client.connect(str(root / 'qingmail-p0/activate.sock'))
                    client.sendall(payload)
                    assert client.recv(512).startswith(b'ERROR')
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.connect(str(root / 'qingmail-p0/activate.sock'))
                client.sendall(b'Q')
                time.sleep(0.3)
            assert status()['pid'] == str(proc.pid)
            record('invalid oversized and stalled activation requests are bounded')

            control('--quit')
            assert proc.wait(timeout=8) == 0
            assert not (root / 'qingmail-p0/activate.sock').exists()
            assert saved_draft() == draft
            record('explicit quit drains draft and releases socket/lock')

            # 强制结束只要求保留此前已确认版本，不声称恢复未确认按键。
            proc = launch('--editor-demo', '--no-tray')
            until(lambda: status().get('editors') == '1')
            until(lambda: saved_draft() == draft)
            os.kill(proc.pid, signal.SIGKILL)
            assert proc.wait(timeout=5) == -signal.SIGKILL
            assert (root / 'qingmail-p0/activate.sock').exists()
            proc = launch('--editor-demo', '--no-tray')
            until(lambda: status().get('pid') == str(proc.pid) and status().get('editors') == '1')
            assert saved_draft() == draft
            control('--quit')
            assert proc.wait(timeout=8) == 0
            record('crash leaves stale socket; restart recovers confirmed draft and replaces stale endpoint')

            # 用真实目录写权限故障验证退出取消，随后恢复权限并退出。
            proc = launch('--lifecycle-failure-probe', '--no-tray')
            until(lambda: status().get('editors') == '1')
            time.sleep(1)
            draft_dir = drafts / 'p2-data'
            draft_dir.chmod(0o500)
            (drafts / 'trigger').touch()
            until(lambda: status().get('probe_step') == '3')
            time.sleep(0.8)
            control('--quit')
            until(lambda: status().get('quitting') == 'false' and status().get('editors') == '1')
            assert proc.poll() is None
            assert saved_draft() == draft
            record('filesystem write failure cancels close/quit and preserves last confirmed draft')
            draft_dir.chmod(0o700)
            # 失败后显式重试；允许先消费上次关闭错误再发起重试。
            for _ in range(4):
                if proc.poll() is not None:
                    break
                control('--quit')
                time.sleep(1)
            assert proc.wait(timeout=8) == 0
            record('restoring write permission allows retry and clean shutdown')
        finally:
            for proc in processes:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=3)
            if (drafts / 'p2-data').exists():
                (drafts / 'p2-data').chmod(0o700)
    evidence.write_text(json.dumps({'checks': checks}, ensure_ascii=False, indent=2) + '\n')
    print(f'PASS: {len(checks)} lifecycle checks; {evidence}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/mail-ui'))
    parser.add_argument('--evidence', type=Path, default=Path('docs/reports/evidence/P2-003-lifecycle.json'))
    args = parser.parse_args()
    run(args.binary.resolve(), args.evidence)
