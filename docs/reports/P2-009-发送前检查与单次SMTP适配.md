# P2-009 · 发送前检查与单次SMTP适配

- 日期：2026-11-16
- 状态：协议实现与合成服务器验证通过；尚未接入UI发送入口或真实服务商。

## 1. 交付

mail-service新增outgoing模块，以冻结OutgoingSnapshot进行发送前检查、构建PreparedMessage及单次SMTP投递。共享AttachmentInfo/OutgoingAttachment/OutgoingSnapshot移入mail-core，mail-storage继续公开原导出路径，避免服务层运行时依赖SQLite。存储结构和schema5不变。

发送前检查定位To/Cc/Bcc第几个地址错误，拒绝无收件人、头部控制字符、异常时间/Message-ID、附件元数据不符、重复附件、超限正文与邮件。无主题必须显式传入已确认决定。本轮仅接受独立ASCII邮箱地址，不支持显示名地址或SMTPUTF8本地部分；Unicode主题、正文和文件名经标准编码支持。最多100个收件人、32个附件、附件总计25MiB、纯文本2MiB、HTML4MiB、完整MIME40MiB。

使用现有lettre 0.11.23的builder构建multipart/mixed及plain+HTML alternative；日期和Message-ID来自持久快照，boundary由任务ID确定，限制RFC长度。正文统一base64，避免用户正文与固定boundary相碰。二进制附件由库base64编码。重复构建同一快照字节一致。密送仅进入SMTP信封，MIME没有Bcc头；仅密送也可构建。收件人信封去掉完全相同的重复地址，保留To/Cc展示。

启用lettre builder feature，锁文件新增email-encoding 0.4.3、httpdate 1.0.3、quoted_printable 0.5.2；未升级lettre、GPUI或Material版本。服务层仅测试依赖mail-storage/mail-content/tempfile，以验证持久状态与协议联动。

## 2. 单次投递边界

生产适配仅支持验证证书的TLS或强制STARTTLS；无跳过验证选项。STARTTLS缺失、证书失败不会认证。认证仅使用加密后的PLAIN/LOGIN。MAIL与所有RCPT均获接受后才提交DATA；一个收件人失败即结束该连接，不部分投递。正文提交前错误归DefinitelyRejected（这里包含尚未投递的连接/认证失败，并非都代表收到服务器负响应）。

DATA前须得到354才发正文，最终250才报告Accepted；最终4xx/5xx为明确失败，正文写入/读取失败、断线或异常正响应为Uncertain。最终250后的QUIT失败不推翻接受。适配不重连、不自动重试、不启动恢复队列；PreparedMessage无Clone且按值消费，但它本身不等于持久Sending许可，未来调用方仍必须先成功begin_smtp，并消费许可finish_smtp。当前生产UI和后台均未调用deliver，没有新增可点击的发送动作。

复用lettre dot-stuffing，提交前去掉一组MIME末尾CRLF，让库追加的终止CRLF不改变正文。服务器还原SMTP framing后核对字节与副本MIME完全一致。副本接口只提供相同原始MIME；IMAP APPEND尚未实现，不把SMTP接受误报为已保存副本。

使用5秒socket操作超时；DNS没有独立超时，整次多收件人投递也没有总deadline或取消机制。本轮不协商SMTP SIZE，不宣称邮件大小适合所有服务商。诊断返回固定枚举，不返回或记录服务器原文与凭据；隐式TLS握手/连接错误当前合并为Connection类别。

## 3. 验证

先跑新增模块单元测试，再集中运行协议/存储联动及收尾门禁；没有重复原生窗口验收。

- [全量88个Rust测试](evidence/P2-009-tests.log)通过；最终SMTP framing修正后重跑[新增7个相关测试](evidence/P2-009-outgoing-tests.log)，全部通过。fmt --check、workspace全目标check --locked、clippy -D warnings、release构建通过。仅保留既有mail-protocol stub manifest提示。
- MIME重新解析核对中文/emoji主题、日期、Message-ID、两种正文、中文附件名和二进制附件内容；固定快照重复构建一致；Bcc仅在信封中，邮件文本无隐含收件地址。
- 预检负例：CRLF注入、损坏To/Cc/Bcc、国际化本地部分、无收件人、未确认空主题、异常Message-ID和时间、附件大小不符/危险文件名/重复附件、收件人与正文超限。
- 合成TLS及STARTTLS服务器：仅一次MAIL/RCPT/DATA提交；认证失败、部分RCPT拒绝、DATA拒绝、最终拒绝、最终断线、异常354响应分类；接受后不返回QUIT成功仍Accepted；缺少STARTTLS和不可信证书不认证。
- 合成服务器+实际SQLite联动：持久化Pending，先校验快照，再成功begin_smtp取得Sending许可，网络提交结果写回；重开后分别是CopyPending/Failed/Uncertain，begin_smtp均拒绝；CopyPending仅允许副本路径。原有崩溃、快照、附件防篡改、禁止自动重发测试继续通过。

测试服务器仅本机随机端口，使用合成证书、密码和example.invalid地址；不连接真实SMTP、不读取系统密钥库、不向外部收件人投递。该集中协议联动不替代真实账号、UI发送流程和完整产品端到端验收。

## 4. 后续

下一轮接入发送前检查反馈、无主题确认、持久任务和明确发送按钮，并展示失败/不确定/副本待保存等状态；实现IMAP已发送副本保存及仅重试副本。真实邮件发送仍需操作者明确授权具体内容和收件人，不能因本轮合成投递成功而宣称P2闭环完成。
