#!/usr/bin/env python3
"""Filtered 5x10k index IPC latency measurement; no network/keyring/UI."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
from performance_probe import rpc

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--daemon',type=Path,default=Path('target/release/mail-daemon'))
    parser.add_argument('--fixture',type=Path,default=Path('target/release/examples/performance_fixture'))
    parser.add_argument('--evidence',type=Path,required=True)
    args=parser.parse_args();args.evidence.parent.mkdir(parents=True,exist_ok=True)
    report=dict(synthetic_only=True,accounts=5,messages_per_account=10000,complete=False,measurements={})
    with tempfile.TemporaryDirectory(prefix='qingmail-p4-index-') as tmp,args.evidence.with_suffix('.log').open('w') as log:
        root=Path(tmp);root.chmod(0o700)
        subprocess.run([str(args.fixture.resolve()),str(root/'data'),'10000'],check=True,stdout=log,stderr=log,timeout=90)
        proc=subprocess.Popen([str(args.daemon.resolve()),'--isolated-root',str(root)],stdout=log,stderr=log)
        try:
            deadline=time.monotonic()+20
            while True:
                try:accounts=rpc(root,'accounts')['data'];break
                except OSError:
                    assert time.monotonic()<deadline;time.sleep(.05)
            account=accounts[0][0]
            def benchmark(label, fn, iterations=100):
                samples=[]
                for _ in range(iterations):
                    started=time.perf_counter();fn();samples.append((time.perf_counter()-started)*1000)
                values=sorted(samples)
                report['measurements'][label]=dict(samples_ms=samples,p50_ms=values[len(values)//2],p95_ms=values[int(len(values)*.95)],max_ms=max(values))
            def page():
                data=rpc(root,'folder_cache',account=account,name='INBOX',before=None)['data']
                assert len(data['messages'])==200
                return data
            benchmark('folder_page_200',page)
            def unified():
                data=rpc(root,'unified_inbox',before=None,limit=200)['data'];assert len(data['messages'])==200
            benchmark('unified_page_200',unified)
            for label,text in [('search_match','needle-4'),('search_miss','not-present-unique')]:
                benchmark(label,lambda:rpc(root,'search',account=None,query=dict(text=text,sender='',has_attachments=False,folder=None)))
            locations=page()['messages'][:50]
            def read(index=[0]):
                location=locations[index[0]%len(locations)]['location'];index[0]+=1
                pending=rpc(root,'business',operation=dict(type='read',location=location,folder='INBOX'))
                deadline=time.monotonic()+10
                while pending['type']=='pending':
                    result=rpc(root,'job_result',task=pending['data']['task'])['data']['result']
                    if result is not None:
                        assert 'Ok' in result;pending=result['Ok'];break
                    assert time.monotonic()<deadline;time.sleep(.005)
                assert pending['type']=='message'
                view=pending['data']
                for resource in [view['document'],view['plain']]+[i['resource'] for i in view['images']]:
                    rpc(root,'cancel_resource',resource=resource['id'])
            benchmark('cached_read_sanitized_resources',read,50)
            assert not rpc(root,'tasks',account=account)['data']
            report['smtp_tasks']=0;report['complete']=True
            rpc(root,'prepare_shutdown');proc.wait(timeout=15)
        finally:
            if proc.poll() is None:proc.terminate();proc.wait(timeout=5)
    args.evidence.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({k:{kk:vv for kk,vv in v.items() if kk!='samples_ms'} for k,v in report['measurements'].items()},indent=2))
if __name__=='__main__':main()
