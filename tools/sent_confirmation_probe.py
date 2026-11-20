#!/usr/bin/env python3
"""发送确认页的隔离原生窗口冒烟测试；不访问真实凭据或发送邮件。"""
import argparse
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import tempfile
import time


def run(binary, fixture, evidence, *, fixture_args=('--sent-confirmation',),
        probe_env='QINGMAIL_SENT_CONFIRMATION_PROBE', marker='SENT_CONFIRMATION PASS', checks=None):
    evidence.parent.mkdir(parents=True, exist_ok=True)
    log_path = evidence.with_suffix('.log')
    with tempfile.TemporaryDirectory(prefix='qingmail-sent-confirmation-') as directory, log_path.open('w') as log:
        root = Path(directory)
        root.chmod(0o700)
        subprocess.run([str(fixture), str(root / 'data'), *fixture_args], check=True, stdout=log)
        env = dict(os.environ, QINGMAIL_IPC_ROOT=str(root),
                   XDG_CONFIG_HOME=str(root / 'config'), XDG_CACHE_HOME=str(root / 'cache'))
        env[probe_env] = '1'
        env.setdefault('WAYLAND_DISPLAY', 'wayland-1')
        env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')
        backend = None
        proc = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 40
            while proc.poll() is None and time.monotonic() < deadline:
                if backend is None:
                    try:
                        with socket.socket(socket.AF_UNIX) as stream:
                            stream.settimeout(2)
                            stream.connect(str(root / 'runtime/qingmail/daemon.sock'))
                            request = dict(version=4, id=secrets.token_hex(16), command=dict(type='status'))
                            stream.sendall(json.dumps(request).encode() + b'\n')
                            backend = json.loads(stream.makefile('rb').readline(2 * 1024 * 1024))['result']['Ok']['data']['pid']
                    except (OSError, KeyError):
                        pass
                time.sleep(.1)
            assert proc.poll() == 0, f'frontend failed or timed out; see {log_path}'
            assert marker in log_path.read_text(), f'probe did not finish; see {log_path}'
            evidence.write_text(json.dumps(dict(passed=True, synthetic_only=True, network_sends=0,
                checks=checks or ['saved copy selected by stable identity', 'pending copy and local-only states',
                        'narrow pane renders', 'local frozen record opens', 'continue writing uses same account',
                        'return to account inbox', 'one native window'],
                limits=['Native state/rendering smoke test; physical clicks and visual acceptance remain manual']),
                ensure_ascii=False, indent=2) + '\n')
        finally:
            if proc.poll() is None:
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
    parser.add_argument('--binary', type=Path, default=Path('target/debug/mail-ui'))
    parser.add_argument('--fixture', type=Path, default=Path('target/debug/examples/sync_fixture'))
    parser.add_argument('--evidence', type=Path, default=Path('dist/verification/sent-confirmation.json'))
    args = parser.parse_args()
    run(args.binary.resolve(), args.fixture.resolve(), args.evidence)
