#!/usr/bin/env python3
"""Measure the real daemon with five loopback TLS IMAP accounts and a PRIVATE Secret Service.
No real account access. Defaults: 5min stabilization, 30min measurement, 5s samples.
"""
import argparse
import collections
import json
import os
from pathlib import Path
import secrets
import socket
import socketserver
import ssl
import subprocess
import tempfile
import threading
import time

class Imap(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True
    def __init__(self, cert, key, idle=True):
        self.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.tls.load_cert_chain(cert, key)
        self.idle = idle
        self.counts = collections.Counter()
        self.active = collections.Counter()
        self.lock = threading.Lock()
        super().__init__(('127.0.0.1', 0), Handler)
    def get_request(self):
        conn, addr = super().get_request()
        conn.settimeout(30)
        try: return self.tls.wrap_socket(conn, server_side=True), addr
        except Exception:
            conn.close()
            raise

class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        account = None
        try:
            self.wfile.write(b'* OK controlled QingMail fixture\r\n')
            for raw in self.rfile:
                parts = raw.decode().strip().split(' ', 2)
                if len(parts) < 2: break
                tag, verb = parts[:2]
                rest = parts[2] if len(parts) > 2 else ''
                command = verb.upper()
                with self.server.lock: self.server.counts[command] += 1
                if command == 'LOGIN':
                    account = rest.split()[0].strip('"')
                    assert account in [f'fixture-{i}' for i in range(5)]
                if command == 'CAPABILITY':
                    self.wfile.write(b'* CAPABILITY IMAP4rev1' + (b' IDLE' if self.server.idle else b'') + b'\r\n')
                elif command == 'LIST':
                    self.wfile.write(b'* LIST () "/" "INBOX"\r\n')
                elif command in ('EXAMINE', 'SELECT'):
                    self.wfile.write(b'* 0 EXISTS\r\n* OK [UIDVALIDITY 42] stable\r\n')
                elif command == 'UID' and rest.upper().startswith('SEARCH'):
                    self.wfile.write(b'* SEARCH\r\n')
                elif command == 'IDLE':
                    with self.server.lock: self.server.active[account] += 1
                    self.wfile.write(b'+ idling\r\n')
                    try:
                        done = self.rfile.readline()
                        if done != b'DONE\r\n': return
                    finally:
                        with self.server.lock: self.server.active[account] -= 1
                elif command == 'LOGOUT':
                    self.wfile.write(b'* BYE\r\n' + tag.encode() + b' OK bye\r\n')
                    break
                elif command not in ('LOGIN', 'CAPABILITY', 'LIST', 'EXAMINE', 'SELECT', 'UID'):
                    raise AssertionError('unexpected command: '+command)
                self.wfile.write(tag.encode()+b' OK done\r\n')
        except (OSError, ssl.SSLError): pass
        except Exception:
            with self.server.lock: self.server.counts['ERROR'] += 1

def rpc(root, kind, **args):
    command = dict(type=kind)
    if args: command['args'] = args
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(8)
        stream.connect(str(root/'runtime/qingmail/daemon.sock'))
        stream.sendall(json.dumps(dict(version=2, id=secrets.token_hex(16), command=command)).encode()+b'\n')
        result = json.loads(stream.makefile('rb').readline(2*1024*1024))['result']
        if 'Err' in result: raise RuntimeError(result['Err']['code'])
        return result['Ok']

def tree_sample(pid):
    """Include all descendants; fail if a process vanishes rather than report zero PSS."""
    parents = {}
    for item in Path('/proc').iterdir():
        if item.name.isdecimal():
            try: parents[int(item.name)] = int((item/'stat').read_text().rsplit(')', 1)[1].split()[1])
            except (FileNotFoundError, PermissionError, ProcessLookupError): pass
    pids = {pid}
    while True:
        children = {p for p, parent in parents.items() if parent in pids}
        if children <= pids: break
        pids |= children
    pss = 0
    cpu = 0
    for current in pids:
        base = Path('/proc')/str(current)
        pss += next(int(l.split()[1]) for l in (base/'smaps_rollup').read_text().splitlines() if l.startswith('Pss:'))
        stats = (base/'stat').read_text().rsplit(')', 1)[1].split()
        cpu += (int(stats[11])+int(stats[12]))/os.sysconf('SC_CLK_TCK')
    return dict(pids=sorted(pids), pss_kib=pss, cpu_seconds=cpu)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--daemon', type=Path, default=Path('target/release/mail-daemon'))
    parser.add_argument('--fixture', type=Path, default=Path('target/release/examples/performance_fixture'))
    parser.add_argument('--warmup', type=float, default=300)
    parser.add_argument('--seconds', type=float, default=1800)
    parser.add_argument('--interval', type=float, default=5)
    parser.add_argument('--polling', action='store_true')
    parser.add_argument('--evidence', type=Path, required=True)
    args = parser.parse_args()
    assert args.warmup >= 0 and args.seconds > 0 and args.interval > 0
    binary, fixture = args.daemon.resolve(), args.fixture.resolve()
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    report = dict(complete=False, synthetic_only=True, mode='polling' if args.polling else 'IDLE', requested_seconds=args.seconds, stabilization_seconds=args.warmup, samples=[])
    def persist(): args.evidence.write_text(json.dumps(report, indent=2)+'\n')
    processes = []
    with tempfile.TemporaryDirectory(prefix='qingmail-p4-perf-') as tmp, args.evidence.with_suffix('.log').open('w') as log:
        root = Path(tmp); root.chmod(0o700)
        for name in ('runtime', 'home'): (root/name).mkdir(mode=0o700)
        env = dict(os.environ, HOME=str(root/'home'), XDG_RUNTIME_DIR=str(root/'runtime'), XDG_CONFIG_HOME=str(root/'config'), XDG_DATA_HOME=str(root/'share'), XDG_CACHE_HOME=str(root/'cache'), DBUS_SESSION_BUS_ADDRESS=f'unix:path={root}/bus', SSL_CERT_FILE=str(root/'cert.pem'), QINGMAIL_PERF_PRIVATE_ROOT=str(root))
        for name in ('QINGMAIL_IPC_ROOT', 'QINGMAIL_IPC_TEST_TRAY'): env.pop(name, None)
        report['environment'] = dict(kernel=os.uname().release, logical_cpus=os.cpu_count(), daemon=str(binary), display_session=bool(env.get('WAYLAND_DISPLAY') or env.get('DISPLAY')), private_dbus=True, private_keyring=True)
        subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(root/'key.pem'),'-out',str(root/'cert.pem'),'-days','1','-subj','/CN=localhost','-addext','subjectAltName=DNS:localhost,IP:127.0.0.1'],check=True,stdout=log,stderr=log)
        imap = Imap(root/'cert.pem', root/'key.pem', not args.polling)
        threading.Thread(target=imap.serve_forever, daemon=True).start()
        def launch(cmd, **kw):
            p = subprocess.Popen(cmd, env=env, stdout=log, stderr=log, **kw); processes.append(p); return p
        try:
            bus = launch(['dbus-daemon','--session','--nofork',f'--address=unix:path={root}/bus'])
            deadline = time.monotonic()+8
            while not (root/'bus').exists():
                assert time.monotonic()<deadline; time.sleep(.05)
            (root/'keyring').mkdir(mode=0o700)
            keyring = launch(['gnome-keyring-daemon','--unlock','--components=secrets','--foreground','--control-directory',str(root/'keyring')],stdin=subprocess.PIPE)
            keyring.stdin.write(b'fixture-only\n'); keyring.stdin.close()
            time.sleep(1)
            subprocess.run([str(fixture),str(root/'data'),'0',str(imap.server_address[1])],env=env,check=True,stdout=log,stderr=log,timeout=90)
            # Match production AppPaths without following a data-directory symlink.
            (root/'share').mkdir(exist_ok=True)
            (root/'data').rename(root/'share/qingmail')
            daemon = launch([str(binary),'--production'])
            deadline = time.monotonic()+90
            while True:
                assert daemon.poll() is None, 'daemon exited; inspect log'
                try:
                    states = rpc(root,'snapshot')['data']['status']
                    # Sync state exposes five independently updated account statuses.
                    accounts = rpc(root,'accounts')['data']
                    if len(accounts)==5 and '已更新' in states['sync_status']:
                        with imap.lock: connected = sum(v>0 for v in imap.active.values())
                        if args.polling or connected==5: break
                except (OSError, KeyError, RuntimeError): pass
                assert time.monotonic()<deadline, 'five account initial sync/IDLE did not become ready'
                time.sleep(.25)
            report['ready_at_utc'] = time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()); persist()
            stable = time.monotonic()
            while time.monotonic()-stable < args.warmup:
                assert daemon.poll() is None; time.sleep(min(5,args.warmup-(time.monotonic()-stable)))
            started = time.monotonic()
            while True:
                elapsed = time.monotonic()-started
                sample = dict(elapsed_seconds=elapsed, **tree_sample(daemon.pid))
                sample['status'] = rpc(root,'status')['data']
                sample['fixture_services_alive'] = keyring.poll() is None and bus.poll() is None
                with imap.lock:
                    sample['active_idle_accounts'] = sum(v>0 for v in imap.active.values())
                    sample['imap_commands'] = dict(imap.counts)
                report['samples'].append(sample); persist()
                if elapsed >= args.seconds: break
                time.sleep(min(args.interval,args.seconds-elapsed))
            first,last=report['samples'][0], report['samples'][-1]
            report['elapsed_seconds']=last['elapsed_seconds']-first['elapsed_seconds']
            report['cpu_single_core_percent']=(last['cpu_seconds']-first['cpu_seconds'])/report['elapsed_seconds']*100
            report['max_pss_mib']=max(s['pss_kib'] for s in report['samples'])/1024
            report['duration_qualifies']=args.warmup>=300 and report['elapsed_seconds']>=1800
            report['stable_fixture'] = all(s['fixture_services_alive'] and len(s['status']['account_sync'])==5 and all(a['status'].startswith('已更新') for a in s['status']['account_sync']) for s in report['samples'])
            report['resource_numbers_within_budget'] = report['max_pss_mib']<=80 and report['cpu_single_core_percent']<.5
            report['budget_passed']=report['duration_qualifies'] and report['stable_fixture'] and report['resource_numbers_within_budget']
            report['complete']=True; persist()
            rpc(root,'prepare_shutdown')
            daemon.wait(timeout=40)
        finally:
            imap.shutdown(); imap.server_close()
            for p in reversed(processes):
                if p.poll() is None:
                    p.terminate()
                    try: p.wait(timeout=10)
                    except subprocess.TimeoutExpired: p.kill(); p.wait(timeout=5)
    print(json.dumps({k:v for k,v in report.items() if k!='samples'},indent=2))

if __name__=='__main__': main()
