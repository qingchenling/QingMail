#!/usr/bin/env python3
"""M3E动效的定向隔离验证；只用合成账号，不读取凭据或发送邮件。"""
import argparse
import json
import os
import re
from pathlib import Path
import signal
import sqlite3
import subprocess
import tempfile


def run(binary, fixture, evidence):
    evidence.parent.mkdir(parents=True, exist_ok=True)
    log_path = evidence.with_suffix('.log')
    with tempfile.TemporaryDirectory(prefix='qingmail-motion-ui-') as directory, log_path.open('w') as log:
        root = Path(directory)
        root.chmod(0o700)
        subprocess.run([str(fixture), str(root/'data'), '--bodies'], check=True, timeout=15, stdout=log)
        env = dict(os.environ, QINGMAIL_IPC_ROOT=str(root), QINGMAIL_MOTION_PROBE='1',
                   XDG_CONFIG_HOME=str(root/'config'), XDG_CACHE_HOME=str(root/'cache'))
        for name in ['QINGMAIL_P0_RUNTIME_ROOT', 'QINGMAIL_P0_DRAFT_ROOT', 'QINGMAIL_INLINE_COMPOSE_PROBE', 'QINGMAIL_DRAFT_WORKSPACE_PROBE',
                     'QINGMAIL_IPC_TEST_EDITOR', 'QINGMAIL_IPC_TEST_MAILBOX']:
            env.pop(name, None)
        env.setdefault('WAYLAND_DISPLAY', 'wayland-1')
        env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')
        proc = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            assert proc.wait(timeout=65) == 0, log_path.read_text()
            assert 'MOTION_UI PASS' in log_path.read_text(), log_path.read_text()
            samples = [(float(t), float(v), r == 'true') for t, v, r in
                       re.findall(r'MOTION_SAMPLE target=([\d.]+) value=([\d.]+) reduced=(true|false)', log_path.read_text())]
            assert any(t == 0 and 0 < v < 44 and not r for t, v, r in samples), 'no closing frames'
            assert any(t == 44 and 0 < v < 44 and not r for t, v, r in samples), 'no opening frames'
            reduced = [(t, v) for t, v, r in samples if r]
            assert reduced and all(t == v for t, v in reduced), 'reduced motion must adopt endpoint'
            with sqlite3.connect(f'file:{root/"data/state.sqlite3"}?mode=ro', uri=True) as db:
                assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
            evidence.write_text(json.dumps(dict(passed=True, synthetic_only=True, sends=0,
                checks=['real disclosure opening and closing intermediate frames',
                        'mail selection and reader switch', 'filter open and close',
                        'settings and account navigation', 'theme settles after rapid retarget',
                        'reduced disclosure samples equal endpoint', 'reduced theme switches immediately',
                        'animation resumes after preference disabled'],
                limits=['Native integration; subjective smoothness and physical IME remain manual']),
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
    parser.add_argument('--evidence', type=Path, default=Path('target/motion-ui-probe.json'))
    args = parser.parse_args()
    run(args.binary.resolve(), args.fixture.resolve(), args.evidence)
