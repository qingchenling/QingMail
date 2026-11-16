# P0 合成 MIME 样本

全部为虚构内容，不接入真实邮箱，不含账号或凭据。
修改生成器后运行 `python3 tools/generate_mime_fixtures.py` 可确定性重建。
`example.invalid` 为不可用的示例域名；攻击样本中的 127.0.0.1:18765 仅供测试探针。

| 样本 | 覆盖 |
| --- | --- |
| 01 | UTF-8 中文纯文本 |
| 02 | GB18030、base64、编码折行主题 |
| 03 | mixed / related / alternative、quoted-printable、百分号编码 CID、PNG、RFC2231 中文危险附件名、嵌套表格 |
| 04 | HTML-only、脚本、事件、脚本 URL、刷新、base、外链 CSS、CSS import/URL/字体、追踪像素、重定向、相对与 file URL、SVG、表单、主动媒体 |
| 05 | 重复/缺失 CID、SVG、超大图片尺寸 |
| 06 | 未闭合 MIME、未知字符集、损坏编码 |
| 07 | HTML-only 长中文引用 |
| 08 | HTML 超过 64 层嵌套 |
| 09 | HTML 超过 512 KiB |
| 10 | 嵌套 message/rfc822 中的 CID 不得用于外层消息 |

这些样本用于技术原型验证；不代表所有服务商和任意恶意邮件均已覆盖。
