#!/usr/bin/env python3
"""使用合成的 SQLite 邮件头，验证原生文件夹树与缓存列表，不访问邮箱。"""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, default=Path('target/release/mail-ui'))
parser.add_argument('--fixture', type=Path, default=Path('target/debug/examples/sync_fixture'))
parser.add_argument('--evidence', type=Path, default=Path('dist/verification/mailbox-cache.json'))
args = parser.parse_args()
args.evidence.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='qingmail-p2-mailbox-') as directory:
    root = Path(directory)
    root.chmod(0o700)
    data = root / 'drafts/p2-data'
    subprocess.run([str(args.fixture.resolve()), str(data)], check=True, timeout=15)
    env = dict(os.environ, QINGMAIL_P0_RUNTIME_ROOT=str(root), QINGMAIL_P0_DRAFT_ROOT=str(root / 'drafts'))
    env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')
    with args.evidence.with_suffix('.log').open('w') as log:
        result = subprocess.run([str(args.binary.resolve()), '--mailbox-probe', '--no-tray'], env=env,
                                stdout=log, stderr=subprocess.STDOUT, timeout=30)
    assert result.returncode == 0, args.evidence.with_suffix('.log').read_text()
    assert 'QINGMAIL_MAILBOX_PROBE: PASS' in args.evidence.with_suffix('.log').read_text()
    with sqlite3.connect(f'file:{data / "state.sqlite3"}?mode=ro', uri=True) as db:
        assert db.execute('PRAGMA user_version').fetchone()[0] == 8
        assert db.execute('SELECT count(*) FROM remote_folders').fetchone()[0] == 4
        assert db.execute('SELECT count(*) FROM messages').fetchone()[0] == 240
        assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
    args.evidence.write_text(json.dumps(dict(passed=True, schema=8, synthetic_only=True,
        folders=4, cached_messages=240, visible_page=200, sends=0,
        checks=['cached account folder hierarchy', 'UID ordering and bounded page', '200/40 cache pages and return to newest',
                'selection changes detail', 'account collapse preserves selected folder',
                'no network or keyring used']), ensure_ascii=False, indent=2) + '\n')
    print(f'PASS: native mailbox cache; {args.evidence}')
