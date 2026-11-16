#!/usr/bin/env python3
"""集中验证原生写信字段/正文/附件保存，以及删除源文件后的跨进程恢复。"""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile

repo = Path(__file__).resolve().parents[1]
evidence = repo / 'docs/reports/evidence/P2-007-compose.json'
with tempfile.TemporaryDirectory(prefix='qingmail-compose-') as directory:
    root = Path(directory)
    root.chmod(0o700)
    source = root / '中文附件.txt'
    original = '附件原文🙂\n'.encode()
    source.write_bytes(original)
    env = dict(os.environ, QINGMAIL_P0_DRAFT_ROOT=str(root), QINGMAIL_COMPOSE_PROBE_SOURCE=str(source))
    env.setdefault('VK_DRIVER_FILES', '/usr/share/vulkan/icd.d/intel_icd.json')
    logs = []
    for mode, marker in [('--compose-fields-probe', 'QINGMAIL_EDITOR_PROBE: PASS'),
                         ('--compose-restore-probe', 'QINGMAIL_COMPOSE_RESTORE_PROBE: PASS')]:
        result = subprocess.run([str(repo / 'target/release/mail-ui'), '--editor-probe', mode, '--no-tray'], env=env, capture_output=True, timeout=45)
        log = (result.stdout + result.stderr).decode(errors='replace')
        logs.append(log)
        evidence.with_suffix('.log').write_text('\n'.join(logs))
        assert result.returncode == 0 and marker in log, log
        if source.exists():
            source.unlink()
    data = root / 'p2-editor-probe'
    with sqlite3.connect(f'file:{data / "state.sqlite3"}?mode=ro', uri=True) as db:
        row = db.execute('SELECT d.id,d.subject,d.recipients,d.body_id FROM drafts d JOIN local_editor e ON e.draft_id=d.id').fetchone()
        assert row[1] == '中文主题🙂（保存中修改）'
        recipients = json.loads(row[2])
        assert recipients == [['to@example.invalid', 'second@example.invalid'], ['cc@example.invalid'], ['bcc@example.invalid']]
        draft = json.loads((data / 'objects' / row[3]).read_text())
        assert draft['text'].endswith('后台保存期间输入')
        attachment = db.execute('SELECT a.id,a.name FROM attachments a JOIN draft_attachments d ON a.id=d.attachment_id WHERE d.draft_id=?', (row[0],)).fetchone()
        assert attachment[1] == '中文附件.txt'
        assert (data / 'objects' / attachment[0]).read_bytes() == original
        assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0] == 0
    evidence.write_text(json.dumps(dict(passed=True, synthetic_only=True, process_runs=2, sends=0,
        attachment_sha256=hashlib.sha256(original).hexdigest(),
        checks=['native To/Cc/Bcc/subject inputs', 'rich body and header save', 'attachment copied through worker',
                'source deletion leaves attachment intact', 'cross-process field/body/attachment restore',
                '900ms slow save acknowledgement preserves subsequent input']), ensure_ascii=False, indent=2) + '\n')
print(f'PASS: {evidence}')
