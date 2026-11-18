#!/usr/bin/env python3
"""Explicitly authorized real IMAP receive/draft check via production IPC. Never invokes SMTP.
Does not print addresses, credentials, message subjects or private bodies. Test drafts remain for review.
"""
import argparse
import json
import os
from pathlib import Path
import secrets
import socket
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--resume-draft', help='Existing test draft ID; queries only, never creates another version')
parser.add_argument('--draft-folder', help='Explicit user-selected existing folder; never guessed')
parser.add_argument('--drafts', action='store_true', help='Create labelled server test drafts (requires user authorization)')
parser.add_argument('--evidence', type=Path, default=Path('dist/verification/real-accounts.json'))
args = parser.parse_args()
runtime = Path(os.environ['XDG_RUNTIME_DIR']) / 'qingmail/daemon.sock'
# Strict allowlist: no send/review/confirmation/flag/move/removal command can be called.
allowed = {'accounts', 'tasks', 'business', 'job_result', 'folder_cache', 'new_draft', 'save_draft', 'draft_sync_status', 'server_drafts', 'read_resource', 'cancel_resource', 'draft_sync_folder'}
allowed_business = {'refresh_catalog', 'sync_folder', 'read', 'scan_server_drafts', 'import_server_draft', 'read_server_draft'}
def rpc(kind, **params):
    assert kind in allowed
    if kind == 'business':
        assert params['operation']['type'] in allowed_business
    command = dict(type=kind)
    if params:
        command['args'] = params
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(8)
        stream.connect(str(runtime))
        stream.sendall(json.dumps(dict(version=1, id=secrets.token_hex(16), command=command)).encode()+b'\n')
        response = json.loads(stream.makefile('rb').readline(2*1024*1024))['result']
    if 'Err' in response:
        # Errors might contain server details; lasting report records a fixed code only.
        raise RuntimeError('IPC error: '+response['Err']['code'])
    return response['Ok']

def business(kind, **params):
    reply = rpc('business', operation=dict(type=kind, **params))
    if reply['type'] != 'pending':
        return reply
    deadline = time.monotonic()+180
    while time.monotonic() < deadline:
        result = rpc('job_result', task=reply['data']['task'])['data']['result']
        if result is not None:
            if 'Err' in result:
                raise RuntimeError('business error: '+result['Err']['code'])
            return result['Ok']
        time.sleep(.3)
    raise TimeoutError('business timeout')

report = dict(smtp_commands=0, real=True, accounts=[], passed=False, dual_account_passed=False)
args.evidence.parent.mkdir(parents=True, exist_ok=True)
def persist():
    args.evidence.write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
accounts = rpc('accounts')['data']
report['configured_accounts'] = len(accounts)
persist()
for number, (account, _) in enumerate(accounts, 1):
    item = dict(account_number=number, checks=[])
    report['accounts'].append(item)
    before = rpc('tasks', account=account)['data']
    try:
        business('refresh_catalog', account=account)
        item['checks'].append('real credential access and folder listing')
        business('sync_folder', account=account, name='INBOX', before=None, history=False)
        cache = rpc('folder_cache', account=account, name='INBOX', before=None)['data']
        item['indexed_messages'] = len(cache['messages']) if cache else 0
        item['checks'].append('INBOX server synchronization')
        if cache and cache['messages']:
            view = business('read', location=cache['messages'][0]['location'], folder='INBOX')['data']
            assert view['document']['state'] == 'ready'
            item['checks'].append('selected message sanitized reading resources')
            for resource in [view['document'], view['plain']] + [i['resource'] for i in view['images']]:
                rpc('cancel_resource', resource=resource['id'])
        if args.draft_folder:
            rpc('draft_sync_folder', account=account, folder=args.draft_folder)
        index = business('scan_server_drafts', account=account)['data']
        item['draft_folder_selected'] = index['folder'] is not None
        item['checks'].append('server Drafts discovery')
        if args.drafts and index['folder']:
            token = 'QingMail P3-007 compatibility '+secrets.token_hex(4)
            text = '仅用于草稿同步验收，不发送。 '+token
            doc = dict(headers=dict(subject=token, to=[], cc=[], bcc=[]), body=dict(version=1, text=text, attributes=[dict(bold=False, italic=False, link=None, block='Paragraph') for _ in range(len(text)+1)]), attachments=[])
            draft = dict(id=args.resume_draft) if args.resume_draft else rpc('new_draft', account=account, document=doc)['data']
            # Persist the label promptly, so interrupted runs can be reviewed without ambiguous retries.
            item['test_draft_label'] = 'existing test draft' if args.resume_draft else token
            item['test_draft_id'] = draft['id']
            persist()
            deadline = time.monotonic()+150
            state = None
            while time.monotonic() < deadline:
                states = rpc('draft_sync_status', account=account)['data']
                state = next((s for s in states if s['draft'] == draft['id']), None)
                if state and state['state'] == 'synced':
                    break
                if state and state['state'] in {'conflict','failed'}:
                    raise RuntimeError('draft state: '+state['state'])
                time.sleep(1)
            assert state and state['state'] == 'synced', 'draft synchronization timeout'
            item['test_draft_label'] = state['subject']
            # Re-fetch the exact uploaded UID through real IMAP, not merely its local upload snapshot.
            originals = business('scan_server_drafts', account=account)['data']
            assert originals['folder'] == index['folder']
            sources = rpc('server_drafts', account=account)['data']['items']
            source = next(s for s in sources if s['id'] == state['source'])
            fetched = business('import_server_draft', account=account, folder=source['folder'], uidvalidity=source['uidvalidity'], uid=source['uid'])['data']
            assert fetched['id'] == source['id']
            item['checks'].append('new local draft APPEND and independent IMAP re-fetch verified')
            item['draft_synced'] = True
        after = rpc('tasks', account=account)['data']
        assert before == after, 'send tasks changed during receive/draft probe'
        item['checks'].append('send tasks unchanged')
        item['passed'] = True
    except Exception as error:
        item['passed'] = False
        item['failure'] = str(error) if isinstance(error, (RuntimeError, AssertionError)) else type(error).__name__
    persist()
    print('account', number, 'passed' if item['passed'] else 'failed', flush=True)
report['passed'] = bool(accounts) and all(i['passed'] and (not args.drafts or i.get('draft_synced', False)) for i in report['accounts'])
report['dual_account_passed'] = len(accounts) >= 2 and report['passed']
report['limits'] = ['No SMTP or mail flag/move/delete command invoked', 'Test drafts intentionally retained', 'Independent second-client editing/conflicts require separate compatibility acceptance']
persist()
print('evidence:', args.evidence)
