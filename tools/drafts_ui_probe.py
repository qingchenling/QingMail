#!/usr/bin/env python3
"""草稿三栏的定向隔离验证；只用合成账号，不读取凭据或发送邮件。"""
import argparse
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import tempfile


def run(binary, fixture, evidence):
    evidence.parent.mkdir(parents=True, exist_ok=True)
    log_path = evidence.with_suffix('.log')
    with tempfile.TemporaryDirectory(prefix='qingmail-drafts-ui-') as directory, log_path.open('w') as log:
        root = Path(directory)
        root.chmod(0o700)
        subprocess.run([str(fixture), str(root/'data'), '--bodies'], check=True, timeout=15, stdout=log)
        env = dict(os.environ, QINGMAIL_IPC_ROOT=str(root), QINGMAIL_DRAFT_WORKSPACE_PROBE='1',
                   XDG_CONFIG_HOME=str(root/'config'), XDG_CACHE_HOME=str(root/'cache'))
        for name in ['QINGMAIL_P0_RUNTIME_ROOT', 'QINGMAIL_P0_DRAFT_ROOT', 'QINGMAIL_INLINE_COMPOSE_PROBE',
                     'QINGMAIL_IPC_TEST_EDITOR', 'QINGMAIL_IPC_TEST_MAILBOX']:
            env.pop(name, None)
        env.setdefault('WAYLAND_DISPLAY', 'wayland-1')
        env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')
        proc = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            assert proc.wait(timeout=65) == 0, log_path.read_text()
            assert 'DRAFT_WORKSPACE PASS' in log_path.read_text(), log_path.read_text()
            assert 'SYNC_STATUS PASS' in log_path.read_text(), log_path.read_text()
            with sqlite3.connect(f'file:{root/"data/state.sqlite3"}?mode=ro', uri=True) as db:
                assert db.execute('SELECT count(*) FROM drafts').fetchone()[0] == 0
                assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
            evidence.write_text(json.dumps(dict(passed=True, synthetic_only=True, sends=0,
                checks=['empty Drafts creates no phantom draft', 'new draft uses inline editor',
                        'dirty switch saves before opening other draft', 'close selection and reopen',
                        'delete selects remaining draft', 'delete with unsaved edits',
                        'delete final draft stays empty', 'return to INBOX closes workspace', 'one window',
                        'closed sync icon discovers drafts automatically', 'open sync popup clears automatically'],
                limits=['Native widget integration; physical mouse and visual acceptance remain manual']),
                ensure_ascii=False, indent=2)+'\n')
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=5)
            # Terminate only the daemon launched under this disposable test root.
            for entry in Path('/proc').iterdir():
                if entry.name.isdigit():
                    try:
                        args = (entry/'cmdline').read_bytes().split(b'\0')
                        if str(root).encode() in args and b'--isolated-root' in args:
                            os.kill(int(entry.name), signal.SIGTERM)
                    except (OSError, ProcessLookupError):
                        pass
    print(f'PASS: {evidence}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/mail-ui'))
    parser.add_argument('--fixture', type=Path, default=Path('target/debug/examples/sync_fixture'))
    parser.add_argument('--evidence', type=Path, default=Path('target/drafts-ui-probe.json'))
    args = parser.parse_args()
    run(args.binary.resolve(), args.fixture.resolve(), args.evidence)
