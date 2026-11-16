"""Generate deterministic synthetic mail; no production accounts or private data."""
from pathlib import Path
import base64
import struct
import zlib

ROOT = Path(__file__).resolve().parents[1] / 'tests/fixtures/mime'
ROOT.mkdir(parents=True, exist_ok=True)
def write(name, body):
    (ROOT / name).write_bytes(body.replace('\r\n', '\n').replace('\n', '\r\n').encode('utf-8'))
def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 32, 20, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress((b'\0' + b'\x32\x80\xc0' * 32) * 20)) + chunk(b'IEND', b'')
encoded = base64.b64encode(png).decode()
headers = 'From: QingMail Demo <demo@example.invalid>\nMIME-Version: 1.0\n'
write('01-plain-utf8.eml', headers + 'Subject: 中文纯文本通知\nContent-Type: text/plain; charset=utf-8\n\n你好，QingMail。\n\n这是一封合成通知。支持选中、复制与中文显示。\nhttps://example.invalid/notice\n')
gbk = base64.b64encode('你好，旧字符集邮件。\n中文附件名测试。'.encode('gb18030')).decode()
subject = base64.b64encode('字符集与折行'.encode()).decode()
write('02-gb18030.eml', headers + f'Subject: =?UTF-8?B?{subject}?=\n =?UTF-8?Q?_demo?=\nContent-Type: text/plain; charset=gb18030\nContent-Transfer-Encoding: base64\n\n{gbk}\n')
html = '''<html><head><style>.card {background-color:#f2f6ff;color:#193257;padding:20px;border-radius:12px} td, th {border:1px solid #cad4e4;padding:8px} h2 {color:#315d9a}</style></head><body style="background-color:#fffaf3;color:#253042"><div class="card"><h2>每周开发简报</h2><p>这是 <strong>合成 HTML 邮件</strong>，包含列表、引用与基础表格。</p><ul><li>邮件解析</li><li>安全阅读副本</li></ul><blockquote>保留原始邮件，派生阅读内容。</blockquote><table><tr><th>事项</th><th>状态</th></tr><tr><td>中文与内嵌图</td><td>待验收</td></tr><tr><td colspan="2"><table><tr><td>嵌套表格</td></tr></table></td></tr></table><p><img src="cid:logo%40demo" alt="QingMail 蓝色示例图片"></p><p><a href="https://example.invalid/details">阅读详情</a> · <a href="mailto:demo@example.invalid">回复示例地址</a></p></div></body></html>'''
write('03-alternative-related.eml', headers + f'''Subject: 每周开发简报 · HTML 与 CID
Content-Type: multipart/mixed; boundary="mixed"

--mixed
Content-Type: multipart/related; boundary="related"

--related
Content-Type: multipart/alternative; boundary="alternative"

--alternative
Content-Type: text/plain; charset=utf-8
Content-Transfer-Encoding: quoted-printable

QingMail weekly digest. Plain alternative.
--alternative
Content-Type: text/html; charset=utf-8

{html}
--alternative--
--related
Content-Type: image/png
Content-ID: <logo@demo>
Content-Transfer-Encoding: base64
Content-Disposition: inline; filename="logo.png"

{encoded}
--related--
--mixed
Content-Type: application/octet-stream
Content-Disposition: attachment; filename*=utf-8''..%2F..%2F%E6%B5%8B%E8%AF%95.txt
Content-Transfer-Encoding: base64

c3ludGhldGljIGF0dGFjaG1lbnQ=
--mixed--
''')
attack = '''<html><head><base href="http://127.0.0.1:18765/base/"><meta http-equiv="refresh" content="0;url=http://127.0.0.1:18765/refresh"><link rel="stylesheet" href="http://127.0.0.1:18765/linked.css"><link rel="preconnect" href="http://127.0.0.1:18765"><style>@import url(http://127.0.0.1:18765/import.css); @font-face {font-family:track;src:url(http://127.0.0.1:18765/font.woff)}.safe {color:#123456;background-color:#f3f7fb;padding:12px}.tracker {background-image:u\\72l(http://127.0.0.1:18765/escaped-css)} p {background:url(http://127.0.0.1:18765/background)}</style><script>window.qingmailAttack=1;fetch('http://127.0.0.1:18765/script')</script></head><body onload="window.qingmailAttack=2;fetch('http://127.0.0.1:18765/onload')"><h2>安全探针正文仍然可读</h2><p class="safe" style="font-weight:bold;background:url(http://127.0.0.1:18765/inline);color:rgb(12,34,56)">安全文字与保留的基础样式</p><img src="cid:logo@demo" alt="valid-cid"><img src="http://127.0.0.1:18765/pixel" onerror="window.qingmailAttack=3"><img src="http://127.0.0.1:18765/redirect"><img src="//127.0.0.1:18765/protocol-relative"><img src="relative.png"><img src="file:///etc/passwd"><img src="data:image/svg+xml,&lt;svg onload='alert(1)'/&gt;"><img srcset="http://127.0.0.1:18765/srcset 1x" src="cid:missing"><iframe src="http://127.0.0.1:18765/frame"></iframe><object data="http://127.0.0.1:18765/object"></object><embed src="http://127.0.0.1:18765/embed"><video poster="http://127.0.0.1:18765/poster"><source src="http://127.0.0.1:18765/video"></video><audio src="http://127.0.0.1:18765/audio"></audio><svg><image href="http://127.0.0.1:18765/svg"></image></svg><form action="http://127.0.0.1:18765/submit"><input type="image" src="http://127.0.0.1:18765/input"><button>提交</button><p>表单说明保留</p></form><a href="jav&#x61;script:window.qingmailAttack=4">混淆脚本链接</a><a href="file:///etc/passwd">本地文件</a><a href="/relative">相对地址</a><a href="https://example.invalid/safe" ping="http://127.0.0.1:18765/ping" target="_blank">安全链接</a><a href="mailto:demo@example.invalid">邮件链接</a></body></html>'''
write('04-adversarial.eml', headers + f'''Subject: 安全探针：主动资源默认阻止
Content-Type: multipart/related; boundary="attack"

--attack
Content-Type: text/html; charset=utf-8

{attack}
--attack
Content-Type: image/png
Content-ID: <logo@demo>
Content-Transfer-Encoding: base64

{encoded}
--attack--
''')
write('05-duplicate-missing-cid.eml', headers + f'''Subject: 重复与缺失 CID
Content-Type: multipart/related; boundary="dup"

--dup
Content-Type: text/html; charset=utf-8

<p>重复 CID 不应选取任意资源。</p><img src="cid:duplicate"><img src="cid:missing"><img src="cid:vector"><img src="cid:oversize">
--dup
Content-Type: image/png
Content-ID: <duplicate>
Content-Transfer-Encoding: base64

{encoded}
--dup
Content-Type: image/png
Content-ID: <duplicate>
Content-Transfer-Encoding: base64

{encoded}
--dup
Content-Type: image/svg+xml
Content-ID: <vector>

<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"></svg>
--dup
Content-Type: image/png
Content-ID: <oversize>
Content-Transfer-Encoding: base64

{base64.b64encode(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 100000, 100000, 8, 2, 0, 0, 0))).decode()}
--dup--
''')
write('06-broken.eml', headers + '''Subject: 损坏边界与编码
Content-Type: multipart/mixed; boundary="broken"

--broken
Content-Type: text/plain; charset=x-unknown
Content-Transfer-Encoding: quoted-printable

Recovered readable text =ZZ with damaged escape.
''')
write('07-html-only-long.eml', headers + 'Subject: HTML-only 长引用\nContent-Type: text/html; charset=utf-8\n\n<h1>HTML-only 营销样本</h1><p>没有纯文本备选，须生成可读文本。</p><blockquote>' + '<p>历史讨论与长中文主题：' + '可读内容。' * 500 + '</p></blockquote><p>结束标记</p>')
write('08-deep.eml', headers + 'Subject: 超限嵌套\nContent-Type: text/html; charset=utf-8\n\n' + '<div>' * 80 + '深层正文仍应可读' + '</div>' * 80)
write('09-large.eml', headers + 'Subject: 大正文\nContent-Type: text/html; charset=utf-8\n\n<p>' + '大正文内容' * 40000 + '</p>')
write('10-nested-message.eml', headers + f'''Subject: 嵌套消息的 CID 不得污染当前邮件
Content-Type: multipart/mixed; boundary="nest"

--nest
Content-Type: text/html; charset=utf-8

<p>外层正文</p><img src="cid:nested-only">
--nest
Content-Type: message/rfc822
Content-Disposition: attachment; filename="forwarded.eml"

From: Nested <nested@example.invalid>
Content-Type: multipart/related; boundary="inside"

--inside
Content-Type: text/html

<p>内层</p>
--inside
Content-Type: image/png
Content-ID: <nested-only>
Content-Transfer-Encoding: base64

{encoded}
--inside--
--nest--
''')
print(f'Generated {len(list(ROOT.glob("*.eml")))} fixtures')
