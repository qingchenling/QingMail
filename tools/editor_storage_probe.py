#!/usr/bin/env python3
"""在隔离数据目录运行原生编辑协议探针，并核对 SQLite 草稿对象。"""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, default=Path('target/release/mail-ui'))
parser.add_argument('--evidence', type=Path, default=Path('dist/verification/editor.json'))
args = parser.parse_args()
args.evidence.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='qingmail-p2-editor-') as directory:
    root = Path(directory)
    root.chmod(0o700)
    env = dict(os.environ, QINGMAIL_P0_DRAFT_ROOT=str(root))
    env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')
    with args.evidence.with_suffix('.log').open('w') as log:
        result = subprocess.run([str(args.binary.resolve()), '--editor-probe', '--no-tray'], env=env,
                                stdout=log, stderr=subprocess.STDOUT, timeout=45)
    assert result.returncode == 0, result.returncode
    log_text = args.evidence.with_suffix('.log').read_text()
    assert 'QINGMAIL_EDITOR_PROBE: PASS' in log_text, log_text
    data = root / 'p2-editor-probe'
    with sqlite3.connect(f'file:{data / "state.sqlite3"}?mode=ro', uri=True) as db:
        assert db.execute('PRAGMA user_version').fetchone()[0] == 5
        row = db.execute('SELECT d.revision,d.body_id,a.kind FROM drafts d JOIN accounts a ON a.id=d.account_id JOIN local_editor e ON e.draft_id=d.id').fetchone()
        assert row[2] == 'local'
        assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
    draft = json.loads((data / 'objects' / row[1]).read_text())
    assert draft['text'].endswith('后台保存期间输入')
    evidence = dict(passed=True, schema=5, revision=row[0], local_only=True, sends=0,
                    checks=['native IME/emoji/format/history probe',
                            'input remains usable before delayed save acknowledgement',
                            'SQLite-backed body equals latest native editor document'])
    args.evidence.write_text(json.dumps(evidence, ensure_ascii=False, indent=2) + '\n')
    print(f'PASS: native editor and SQLite storage; {args.evidence}')
