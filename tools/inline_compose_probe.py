#!/usr/bin/env python3
"""主窗口写信的集中隔离验证；覆盖保存失败，不访问真实邮箱或发送邮件。"""
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


def run(binary, fixture, evidence):
    evidence.parent.mkdir(parents=True, exist_ok=True)
    log_path = evidence.with_suffix('.log')
    proc = None
    backend = None
    with tempfile.TemporaryDirectory(prefix='qingmail-inline-compose-') as directory, log_path.open('w') as log:
        root = Path(directory)
        root.chmod(0o700)
        subprocess.run([str(fixture), str(root/'data'), '--bodies'], check=True, timeout=15, stdout=log)
        env = dict(os.environ, QINGMAIL_IPC_ROOT=str(root), QINGMAIL_INLINE_COMPOSE_PROBE='1',
                   XDG_CONFIG_HOME=str(root/'config'), XDG_CACHE_HOME=str(root/'cache'))
        for name in ['QINGMAIL_P0_RUNTIME_ROOT', 'QINGMAIL_P0_DRAFT_ROOT', 'QINGMAIL_IPC_TEST_EDITOR', 'QINGMAIL_IPC_TEST_MAILBOX']:
            env.pop(name, None)
        env.setdefault('WAYLAND_DISPLAY', 'wayland-1')
        env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')

        def rpc(kind):
            with socket.socket(socket.AF_UNIX) as stream:
                stream.settimeout(5)
                stream.connect(str(root/'runtime/qingmail/daemon.sock'))
                stream.sendall(json.dumps(dict(version=4, id=secrets.token_hex(16), command=dict(type=kind))).encode()+b'\n')
                return json.loads(stream.makefile('rb').readline(2*1024*1024))['result']['Ok']['data']

        def until(fn, timeout=50):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if proc.poll() is not None:
                    raise AssertionError(f'frontend exited early ({proc.returncode}); see {log_path}')
                try:
                    if result := fn():
                        return result
                except (OSError, KeyError):
                    pass
                time.sleep(.1)
            raise AssertionError(f'probe timed out; see {log_path}')

        try:
            proc = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=subprocess.STDOUT)
            backend = until(lambda: rpc('status'))['pid']
            until(lambda: 'INLINE_COMPOSE READY_FAILURE' in log_path.read_text())
            (root/'data/objects').chmod(0o500)
            (root/'fail-save').touch()
            until(lambda: 'INLINE_COMPOSE FAILURE_RETAINED' in log_path.read_text(), timeout=20)
            assert proc.poll() is None
            (root/'data/objects').chmod(0o700)
            (root/'recover-save').touch()
            assert proc.wait(timeout=25) == 0, log_path.read_text()
            assert 'INLINE_COMPOSE PASS' in log_path.read_text(), log_path.read_text()
            assert rpc('status')['pid'] == backend, 'daemon must survive frontend close'
            with sqlite3.connect(f'file:{root / "data/state.sqlite3"}?mode=ro', uri=True) as db:
                rows = db.execute('SELECT subject,body_id FROM drafts').fetchall()
                assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
            documents = {subject: json.loads((root/'data/objects'/body).read_text()) for subject, body in rows}
            first = documents['内嵌草稿一 · 最新']
            assert first['text'] == '内嵌写信 Chinese 🙂即时关闭失败保留退出保存', first
            assert first['attributes'][0]['bold'] and first['attributes'][0]['italic']
            assert documents['内嵌草稿二']['text'] == '第二份草稿'
            evidence.write_text(json.dumps(dict(passed=True, binary=str(binary), synthetic_only=True, sends=0,
                checks=['one main window through compose/read/reopen', 'native IME veto and format/history',
                        'explicit send review cancel creates no send task', 'dirty draft switch persists before replacement',
                        'mail selection saves latest text before leaving compose', 'narrow embedded workspace',
                        'filesystem save failure retains current content', 'retry and main close flush latest input',
                        'daemon survives frontend exit'],
                limits=['Native input/entity integration; physical mouse and real IME still need user acceptance']), ensure_ascii=False, indent=2)+'\n')
        finally:
            (root/'data/objects').chmod(0o700)
            if proc and proc.poll() is None:
                proc.kill()
                proc.wait(timeout=5)
            if backend:
                try:
                    if str(root).encode() in Path(f'/proc/{backend}/cmdline').read_bytes():
                        os.kill(backend, signal.SIGTERM)
                except OSError:
                    pass
    print(f'PASS: {evidence}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/mail-ui'))
    parser.add_argument('--fixture', type=Path, default=Path('target/debug/examples/sync_fixture'))
    parser.add_argument('--evidence', type=Path, default=Path('dist/verification/inline-compose.json'))
    args = parser.parse_args()
    run(args.binary.resolve(), args.fixture.resolve(), args.evidence)
