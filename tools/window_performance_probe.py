#!/usr/bin/env python3
"""Sample the real mailbox's idle drawing with 5x10k cached indexes. Isolated, no credentials/network."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from performance_probe import rpc, tree_sample

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,default=Path('target/release/mail-ui'))
    parser.add_argument('--fixture',type=Path,default=Path('target/release/examples/performance_fixture'))
    parser.add_argument('--evidence',type=Path,required=True)
    parser.add_argument('--mode',choices=('idle','interaction'),default='idle')
    args=parser.parse_args();args.evidence.parent.mkdir(parents=True,exist_ok=True)
    binary=args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix='qingmail-p4-window-') as tmp,args.evidence.with_suffix('.log').open('w') as log:
        root=Path(tmp);root.chmod(0o700)
        subprocess.run([str(args.fixture.resolve()),str(root/'data'),'10000'],check=True,stdout=log,stderr=log,timeout=90)
        env=dict(os.environ,QINGMAIL_IPC_ROOT=str(root),QINGMAIL_WINDOW_PERF=args.mode,QINGMAIL_FPS='0',XDG_CONFIG_HOME=str(root/'config'),XDG_CACHE_HOME=str(root/'cache'))
        for key in ('QINGMAIL_PERF_PROBE','QINGMAIL_P0_RUNTIME_ROOT','QINGMAIL_P0_DRAFT_ROOT','QINGMAIL_IPC_TEST_TRAY'):env.pop(key,None)
        env.setdefault('WAYLAND_DISPLAY','wayland-1')
        env.setdefault('VK_DRIVER_FILES','/usr/share/vulkan/icd.d/intel_icd.json')
        process=subprocess.Popen([str(binary)],env=env,stdout=log,stderr=log)
        samples=[];backend=None
        try:
            deadline=time.monotonic()+60
            while process.poll() is None:
                if time.monotonic()>deadline:raise TimeoutError('mailbox probe timeout')
                try:
                    samples.append(dict(at=time.monotonic(),**tree_sample(process.pid)))
                except (FileNotFoundError, ProcessLookupError):
                    # Normal detach can race /proc sampling; never count disappearance as zero.
                    try: process.wait(timeout=.5)
                    except subprocess.TimeoutExpired: raise RuntimeError('live process tree changed during measurement')
                    break
                try:backend=rpc(root,'status')['data']['pid']
                except OSError:pass
                time.sleep(.25)
            assert process.returncode==0,'UI exited unsuccessfully; inspect log'
            assert backend is not None
            # UI detached through normal lifecycle, so the independent daemon must remain.
            assert rpc(root,'status')['data']['pid']==backend
            assert not Path(f'/proc/{process.pid}').exists()
            rpc(root,'prepare_shutdown')
        finally:
            if process.poll() is None:process.terminate();process.wait(timeout=10)
            if backend:
                try:rpc(root,'prepare_shutdown')
                except OSError:pass
        text=args.evidence.with_suffix('.log').read_text()
        lines=[line for line in text.splitlines() if line.startswith(('MAILBOX_PERF','MAILBOX_ACTION'))]
        assert (len(lines)==2 if args.mode=='idle' else any('MAILBOX_ACTION PASS' in line for line in lines)),text[-2000:]
        report=dict(passed=True,synthetic_only=True,accounts=5,messages_per_account=10000,window_samples=samples,frame_observation=lines,ui_exited_backend_retained=True,display=dict(wayland=env.get('WAYLAND_DISPLAY'),driver=env.get('VK_DRIVER_FILES')))
        args.evidence.write_text(json.dumps(report,indent=2)+'\n')
        print('\n'.join(lines))
if __name__=='__main__':main()
