#!/usr/bin/env python3
"""合成邮件：缓存跨进程读取到原生阅读窗口，不访问凭据或网络。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile

root_repo = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--evidence', type=Path, default=root_repo / 'dist/verification/body-reading.json')
args = parser.parse_args()
evidence = args.evidence
evidence.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='qingmail-body-') as directory:
    root = Path(directory)
    root.chmod(0o700)
    data = root / 'drafts/p2-data'
    subprocess.run([str(root_repo / 'target/debug/examples/sync_fixture'), str(data), '--bodies'], check=True, timeout=15)
    env = dict(os.environ, QINGMAIL_P0_RUNTIME_ROOT=str(root), QINGMAIL_P0_DRAFT_ROOT=str(root / 'drafts'), XDG_CONFIG_HOME=str(root / 'config'), XDG_CACHE_HOME=str(root / 'cache'))
    env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')
    logs = []
    for _ in range(2):
        result = subprocess.run([str(root_repo / 'target/release/mail-ui'), '--body-probe', '--no-tray'], env=env, capture_output=True, timeout=30)
        log = (result.stdout + result.stderr).decode(errors='replace')
        logs.append(log)
        assert result.returncode == 0 and 'QINGMAIL_BODY_PROBE: PASS' in log, log
    evidence.with_suffix('.log').write_text('\n'.join(logs))
    with sqlite3.connect(f'file:{data / "state.sqlite3"}?mode=ro', uri=True) as db:
        assert db.execute('PRAGMA user_version').fetchone()[0] == 8
        rows = db.execute('SELECT uid, object_id FROM message_bodies ORDER BY uid DESC').fetchall()
        assert len(rows) == 3
        raw = (data / 'objects' / rows[0][1]).read_bytes()
        assert raw == (root_repo / 'tests/fixtures/mime/03-alternative-related.eml').read_bytes()
        assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
    copies = list((root / 'drafts/reading-cache').rglob('*.html'))
    assert copies
    evidence.write_text(json.dumps(dict(passed=True, synthetic_only=True, schema=8, process_runs=2,
        body_sha256=hashlib.sha256(raw).hexdigest(), cached_bodies=3, sends=0,
        checks=['SQLite worker to native Reader', 'offline reopen without credentials', 'CID and attachments',
                'plain toggle and HTML restore', 'browser copy created; browser resource policy unchanged']), ensure_ascii=False, indent=2) + '\n')
print(f'PASS: {evidence}')
