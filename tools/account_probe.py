#!/usr/bin/env python3
"""隔离原生账号表单探针；只使用合成输入，不连接邮箱或写入密钥。"""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, default=Path('target/release/mail-ui'))
parser.add_argument('--evidence', type=Path, default=Path('dist/verification/accounts.json'))
args = parser.parse_args()
args.evidence.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='qingmail-p2-accounts-') as directory:
    root = Path(directory)
    root.chmod(0o700)
    env = dict(os.environ, QINGMAIL_P0_RUNTIME_ROOT=str(root), QINGMAIL_P0_DRAFT_ROOT=str(root / 'drafts'))
    env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')
    with args.evidence.with_suffix('.log').open('w') as log:
        result = subprocess.run([str(args.binary.resolve()), '--accounts-probe', '--no-tray'], env=env,
                                stdout=log, stderr=subprocess.STDOUT, timeout=30)
    assert result.returncode == 0, args.evidence.with_suffix('.log').read_text()
    assert 'QINGMAIL_ACCOUNT_PROBE: PASS' in args.evidence.with_suffix('.log').read_text()
    with sqlite3.connect(f'file:{root / "drafts/p2-data/state.sqlite3"}?mode=ro', uri=True) as db:
        assert db.execute('SELECT count(*) FROM accounts').fetchone()[0] == 0
        assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
    args.evidence.write_text(json.dumps(dict(passed=True, synthetic_only=True, accounts=0, sends=0,
        checks=['empty and invalid form cannot start network check', 'password input returns real value',
                'save binds to checked settings', 'editing discards checked credentials',
                'Outlook and Microsoft365 profiles render in the shared account page',
                'Microsoft login uses the built-in app registration without a client ID input',
                'no account or send task persisted']), ensure_ascii=False, indent=2) + '\n')
    print(f'PASS: native account form; {args.evidence}')
