"""Real Firefox/BiDi resource probe. Requires Python websockets and Firefox.
Run after cargo build -p mail-content --example reading_copy --locked.
All network probes are confined to a local synthetic server.
"""
import base64
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from websockets.sync.client import connect

ROOT = Path(__file__).resolve().parents[1]
EVIDENCE = ROOT / 'docs/reports/evidence'
EVIDENCE.mkdir(parents=True, exist_ok=True)
HITS = []
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        HITS.append(self.path)
        if self.path == '/redirect':
            self.send_response(302)
            self.send_header('Location', '/redirect-target')
            self.end_headers()
            return
        self.send_response(200)
        self.send_header('Content-Type', 'image/png' if self.path in ['/pixel', '/redirect-target'] else 'text/plain')
        self.send_header('Access-Control-Allow-Origin', '*')
        self.end_headers()
        self.wfile.write(b'synthetic-resource')
    def log_message(self, *args):
        pass

class Bidi:
    def __init__(self, ws):
        self.ws, self.counter = ws, 0
    def call(self, method, params):
        self.counter += 1
        self.ws.send(json.dumps(dict(id=self.counter, method=method, params=params)))
        while True:
            reply = json.loads(self.ws.recv(timeout=20))
            if reply.get('id') == self.counter:
                if reply.get('type') == 'error':
                    raise RuntimeError(reply)
                return reply['result']
    def evaluate(self, context, expression):
        result = self.call('script.evaluate', {'expression': expression, 'target': {'context': context}, 'awaitPromise': True})
        if result['type'] != 'success':
            raise RuntimeError(result)
        return result['result'].get('value')

server = ThreadingHTTPServer(('127.0.0.1', 18765), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
report = {'scope': 'Firefox only; local file copies; no CSP and CSP variants; synthetic loopback resource probes', 'cases': []}
with tempfile.TemporaryDirectory(prefix='qingmail-browser-probe-') as temporary:
    temporary = Path(temporary)
    profile = temporary / 'profile'
    profile.mkdir()
    (profile / 'user.js').write_text('user_pref("app.normandy.enabled", false);\nuser_pref("datareporting.healthreport.uploadEnabled", false);\n')
    with socket.socket() as port:
        port.bind(('127.0.0.1', 0))
        debug_port = port.getsockname()[1]
    log = (EVIDENCE / 'P0-005-firefox.log').open('w')
    browser = subprocess.Popen(['firefox', '--headless', '--no-remote', '--profile', str(profile), '--remote-debugging-port', str(debug_port)], stdout=log, stderr=log)
    try:
        ws = None
        for _ in range(100):
            try:
                ws = connect(f'ws://127.0.0.1:{debug_port}/session', open_timeout=1)
                break
            except OSError:
                time.sleep(0.1)
        assert ws is not None, 'Firefox BiDi did not start'
        with ws:
            bidi = Bidi(ws)
            report['capabilities'] = bidi.call('session.new', {'capabilities': {}})['capabilities']
            context = bidi.call('browsingContext.create', {'type': 'tab'})['context']
            def visit(path):
                bidi.call('browsingContext.navigate', {'context': context, 'url': path.as_uri(), 'wait': 'complete'})
                time.sleep(1.5)
                return json.loads(bidi.evaluate(context, '''JSON.stringify({
                    url: location.href, title: document.title, attack: window.qingmailAttack || 0,
                    readable: document.body.innerText.includes('安全探针正文仍然可读'),
                    images: [...document.images].filter(i => i.getAttribute('src')).map(i => ({src:i.src.slice(0,32),width:i.naturalWidth,height:i.naturalHeight,complete:i.complete})),
                    links: [...document.querySelectorAll('a[href]')].map(a => a.getAttribute('href')),
                    styles: [...document.querySelectorAll('.safe')].map(p => ({color:getComputedStyle(p).color,background:getComputedStyle(p).backgroundColor})),
                    activeNodes: document.querySelectorAll('script,iframe,object,embed,svg,form,input,video,audio,base,link').length
                })'''))
            parent = temporary / 'copies'
            parent.mkdir(mode=0o700)
            fixture = ROOT / 'tests/fixtures/mime/04-adversarial.eml'
            copy = Path(subprocess.check_output([str(ROOT / 'target/debug/examples/reading_copy'), str(fixture), str(parent)], text=True).strip())
            safe = copy.read_text()
            import re
            no_csp = temporary / 'sanitized-no-csp.html'
            no_csp.write_text(re.sub(r'<meta http-equiv="Content-Security-Policy" content="[^"]*">', '', safe, count=1))
            assert 'Content-Security-Policy' not in no_csp.read_text()
            for label, path in [('sanitized-with-csp', copy), ('sanitized-without-csp', no_csp)]:
                HITS.clear()
                state = visit(path)
                assert not HITS, (label, HITS)
                assert state['attack'] == 0 and state['readable'] and state['activeNodes'] == 0, state
                assert len(state['images']) == 1 and state['images'][0]['width'] == 32 and state['images'][0]['height'] == 20, state
                assert state['links'] == ['https://example.invalid/safe', 'mailto:demo@example.invalid'], state
                assert state['styles'][0]['color'] == 'rgb(12, 34, 56)', state
                report['cases'].append({'name':label, 'requests':list(HITS), 'state':state, 'passed':True})
                if label == 'sanitized-with-csp':
                    screenshot = bidi.call('browsingContext.captureScreenshot', {'context':context, 'origin':'document'})['data']
                    (EVIDENCE / 'P0-005-browser-safety.png').write_bytes(base64.b64decode(screenshot))
            # Positive control confirms both script execution and actual network probes work.
            control = temporary / 'unsafe-control.html'
            control.write_text('''<!doctype html><script>window.qingmailAttack=9;fetch('http://127.0.0.1:18765/script')</script><img src="http://127.0.0.1:18765/pixel"><img src="http://127.0.0.1:18765/redirect">''')
            HITS.clear()
            state = visit(control)
            assert state['attack'] == 9 and all(endpoint in HITS for endpoint in ['/script','/pixel','/redirect','/redirect-target']), (state,HITS)
            report['cases'].append({'name':'unsafe-positive-control', 'requests':list(HITS), 'attack':state['attack'], 'passed':True})
            # Independently demonstrate CSP blocks accidentally reintroduced active content.
            csp_control = temporary / 'csp-defense.html'
            csp_control.write_text(safe.replace('</body>', '<script>window.qingmailAttack=8;fetch("http://127.0.0.1:18765/csp-script")</script><img src="http://127.0.0.1:18765/csp-pixel"></body>'))
            HITS.clear()
            state = visit(csp_control)
            assert state['attack'] == 0 and not HITS, (state, HITS)
            report['cases'].append({'name':'csp-independent-defense', 'requests':list(HITS), 'attack':state['attack'], 'passed':True})
            # Ordinary absolute HTTP link works only after explicit activation.
            link_mail = temporary / 'link.eml'
            link_mail.write_text('Content-Type: text/html; charset=utf-8\n\n<p>Normal link</p><a href="http://127.0.0.1:18765/clicked">Open probe</a>')
            link_copy = Path(subprocess.check_output([str(ROOT / 'target/debug/examples/reading_copy'),str(link_mail),str(parent)],text=True).strip())
            HITS.clear()
            bidi.call('browsingContext.navigate', {'context':context,'url':link_copy.as_uri(),'wait':'complete'})
            assert not HITS
            bidi.evaluate(context, "document.querySelector('a[href]').click(); true")
            time.sleep(1)
            assert '/clicked' in HITS, HITS
            report['cases'].append({'name':'explicit-http-link', 'requests':list(HITS), 'passed':True})
            # Layout smoke test on the ordinary marketing/related sample.
            friendly = ROOT / 'tests/fixtures/mime/03-alternative-related.eml'
            friendly_copy = Path(subprocess.check_output([str(ROOT / 'target/debug/examples/reading_copy'), str(friendly), str(parent)], text=True).strip())
            bidi.call('browsingContext.navigate', {'context':context,'url':friendly_copy.as_uri(),'wait':'complete'})
            state = json.loads(bidi.evaluate(context, "JSON.stringify({text:document.body.innerText.includes('每周开发简报'),tables:document.querySelectorAll('table').length,background:getComputedStyle(document.querySelector('.card')).backgroundColor,images:[...document.images].map(i=>i.naturalWidth)})"))
            assert state['text'] and state['tables'] == 2 and state['background'] == 'rgb(242, 246, 255)' and state['images'] == [32], state
            report['cases'].append({'name':'marketing-layout', 'state':state, 'passed':True})
            screenshot = bidi.call('browsingContext.captureScreenshot', {'context':context,'origin':'document'})['data']
            (EVIDENCE / 'P0-005-browser-layout.png').write_bytes(base64.b64decode(screenshot))
            bidi.call('session.end', {})
    finally:
        browser.terminate()
        try:
            browser.wait(timeout=10)
        except subprocess.TimeoutExpired:
            browser.kill()
            browser.wait()
        log.close()
        server.shutdown()
report['passed'] = all(case['passed'] for case in report['cases'])
# Keep runtime-generated temporary paths out of the lasting report.
report['capabilities'].pop('moz:profile', None)
report['capabilities'].pop('moz:processID', None)
for case in report['cases']:
    if 'state' in case:
        case['state'].pop('url', None)
(EVIDENCE / 'P0-005-browser-probe.json').write_text(json.dumps(report,ensure_ascii=False,indent=2) + '\n')
print(json.dumps(report,ensure_ascii=False,indent=2))
