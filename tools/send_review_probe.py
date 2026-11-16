#!/usr/bin/env python3
"""集中原生发送预览探针：无主题、取消、错误定位；不确认、不联网。"""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile

repo=Path(__file__).resolve().parents[1]
evidence=repo/'docs/reports/evidence/P2-010-send-review.json'
with tempfile.TemporaryDirectory(prefix='qingmail-send-review-') as directory:
    root=Path(directory)
    root.chmod(0o700)
    data=root/'drafts/p2-data'
    subprocess.run([str(repo/'target/debug/examples/sync_fixture'),str(data)],check=True,timeout=15)
    env=dict(os.environ,QINGMAIL_P0_RUNTIME_ROOT=str(root),QINGMAIL_P0_DRAFT_ROOT=str(root/'drafts'))
    env.setdefault('VK_DRIVER_FILES','/usr/share/vulkan/icd.d/intel_icd.json')
    with evidence.with_suffix('.log').open('w') as log:
        result=subprocess.run([str(repo/'target/release/mail-ui'),'--send-review-probe','--no-tray'],env=env,stdout=log,stderr=subprocess.STDOUT,timeout=35)
    log=evidence.with_suffix('.log').read_text()
    assert result.returncode==0 and 'QINGMAIL_SEND_REVIEW_PROBE: PASS' in log,log
    with sqlite3.connect(f'file:{data / "state.sqlite3"}?mode=ro',uri=True) as db:
        assert db.execute('SELECT count(*) FROM send_tasks').fetchone()[0]==0
        row=db.execute('SELECT d.subject,d.recipients,a.kind FROM drafts d JOIN accounts a ON a.id=d.account_id').fetchone()
        assert row[0]=='' and row[2]=='remote'
        assert json.loads(row[1])[0]==['partial@']
    evidence.write_text(json.dumps(dict(passed=True,synthetic_only=True,sends=0,
        checks=['mailbox to account composer', 'saved native input to empty-subject review',
                'review cancel leaves no task','invalid recipient remains editable and cannot reach confirmation',
                'no credential vault or network called']),ensure_ascii=False,indent=2)+'\n')
print(f'PASS: {evidence}')
