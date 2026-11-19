# Outlook / Microsoft 365 邮箱接入

日期：2026-11-19。本说明对应1.0.1（schema21 / IPC v4），1.0.0包不包含此功能。升级前保存并退出旧程序，配套更新前后台。

## 登录方式

QingMail 已内置应用客户端 ID **`fa0a5ba2-d5bb-49af-9683-58bc3cb517e7`**。账号页不提供 ID、客户端密钥或授权端点配置，也不读取 ID 环境变量。普通用户只需填写邮箱并在系统浏览器登录；不在 QingMail 中填写微软密码。

微软要求 IMAP / SMTP 使用 OAuth；邮箱密码或应用专用密码不能替代此处的浏览器授权。访问及刷新令牌只进入系统 Secret Service 密钥库。

## 应用维护者需核对的注册项

以下是微软门户内的应用配置，属于维护者职责，不是 QingMail 的用户设置。本轮已收到应用 ID，但未登录微软门户核实其权限、账号类型与回调配置。

1. 在 [Microsoft Entra 管理中心](https://entra.microsoft.com/) 打开上述应用。若同时支持个人 Outlook / Hotmail 和 Microsoft 365，支持的账号类型应包含“任何组织目录中的账号和个人 Microsoft 账号”。
2. 在“身份验证”添加“移动和桌面应用程序”平台，登记重定向 URI **`http://localhost/oauth2/callback`**。不是 Web 机密客户端，不创建客户端密码。QingMail 每次只监听本机 IPv4 随机端口；微软对 localhost URI 的端口匹配忽略端口，但路径必须一致。不要登记成只有根路径的 `http://localhost`，也不要同时添加仅端口不同的 URI。
3. API 权限应包含 **Office 365 Exchange Online 的委托权限** `IMAP.AccessAsUser.All` 和 `SMTP.Send`。QingMail 另请求 `offline_access` 以便刷新令牌。Microsoft Graph 的 `Mail.Read` / `Mail.Send` 不能代替这些协议权限。组织策略要求时由管理员同意。

## 添加邮箱

1. 打开主窗口右上角的账号页，选择“添加邮箱”。
2. 个人 Outlook / Hotmail 选择“Outlook / Hotmail”；工作或学校邮箱选择“Microsoft 365 工作或学校”。填写邮箱和名称。
3. 点击“在浏览器登录并检查”，在系统浏览器完成登录和同意。授权只持续约五分钟；可以从账号页取消。关闭前台也会停止等待，后台短期项由清理机制回收。
4. 返回 QingMail 等待 IMAP / SMTP 连接检查；检查不会发送邮件。通过后点击“保存账号”，随后沿用普通账号的同步、阅读、草稿和显式发送流程。

| 类型 | IMAP | SMTP | 登录入口 |
| --- | --- | --- | --- |
| 个人 Outlook / Hotmail | `outlook.office365.com:993`，TLS | `smtp-mail.outlook.com:587`，STARTTLS | `consumers` |
| Microsoft 365 | `outlook.office365.com:993`，TLS | `smtp.office365.com:587`，STARTTLS | `organizations` |

这些服务器固定，不向其他主机发送微软令牌。浏览器中应登录填写的邮箱账号；若选择其他账号，邮件协议检查可能拒绝访问原地址，不会静默换账号。

## 失败与重新登录

- Outlook.com 网页设置中需允许应用使用 IMAP；POP 不在本客户端范围。
- 工作或学校账号需有 Exchange Online 邮箱；组织可禁用 IMAP、禁用 SMTP AUTH 或要求管理员批准。浏览器登录成功不等于邮件协议已获允许。QingMail 不绕过租户策略；两个连接均通过才能保存。
- 授权取消、超时或检查失败不覆盖原凭据。详细连接诊断仍默认隐藏，但检查失败会在账号页状态中显示收发两项原因。
- 访问令牌接近到期时，后台在使用前刷新；同一凭据串行刷新、不同账号互不锁定。刷新成功后先将新令牌写入并读回密钥库，确认后才交给邮件操作。网络失败保留旧项并提示重试；授权被撤销则提示到账号页重新登录。
- 重新授权：选择已有微软账号，再次浏览器登录、检查、保存。成功前保留旧配置及密钥关联。删除账号沿用现有密钥清理。
- 断网恢复、令牌刷新、程序重启均不自动重发邮件；发送结果不确定时仍需先核对服务器。
- “发送后保存副本”默认关闭。Outlook / Exchange 可能自行保存发送副本，客户端开关不控制服务器行为。

## 验证边界

已使用合成凭据、本机 TLS 服务器和隔离账号页验证实现。本轮已内置用户提供的应用 ID，但**未进行真实微软登录、令牌刷新、收信或发送兼容验收**，也未核实门户注册项。维护者核对配置后可先登录、收信与测试草稿；真实发送仍需另行明确授权。Gmail 不在本次实现范围，公开分发资格也未由此完成。

参考：[微软 IMAP / SMTP OAuth](https://learn.microsoft.com/en-us/exchange/client-developer/legacy-protocols/how-to-authenticate-an-imap-pop-smtp-application-by-using-oauth)、[授权码与 PKCE](https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-auth-code-flow)、[本机回调规则](https://learn.microsoft.com/en-us/entra/identity-platform/reply-url)、[Outlook.com 服务器设置](https://support.microsoft.com/zh-cn/outlook/pop-imap-and-smtp-settings-for-outlook-com)。
