# ADR-0010：微软 OAuth、凭据边界与 IPC v4

- 日期：2026-11-19
- 状态：接受，用户要求增加 Outlook；已提供应用 ID 并要求内置、移除配置入口
- 关联：ADR-0002、0005、0007、0009。更新 ADR-0002 对 Outlook 的延后决定；Gmail 继续延后。

## 决策

1. 支持 Outlook.com / Hotmail 个人账号与 Microsoft 365 组织账号。复用通用 IMAP / SMTP 业务；固定微软服务器，认证仅 XOAUTH2，失败不退回 LOGIN / PLAIN。邮件协议前继续验证证书与加密，不引入 Graph 邮件实现。
2. 原生 public client 使用系统浏览器、授权码、PKCE S256、随机 state、本机随机端口回调；只监听 IPv4 loopback。回调有期限、读取上限、Host / path / state / 重复参数验证，完成或取消后关闭。注册路径为 `http://localhost/oauth2/callback`，按微软 localhost 匹配规则使用动态端口，无 client_secret。内置用户提供的公开 client ID `fa0a5ba2-d5bb-49af-9683-58bc3cb517e7`；无UI或环境变量覆盖入口，不冒用其他应用。
3. 个人使用 consumers、组织使用 organizations；委托权限为 `https://outlook.office.com/IMAP.AccessAsUser.All`、`https://outlook.office.com/SMTP.Send`、`offline_access`。组织可禁止协议或要求管理员同意；不承诺绕过策略。暂不支持主权云或自定义授权端点。
4. daemon 完成授权后只向 UI 交接 Secret Service 临时引用，继续走 CheckAccount → CommitAccount，只有双协议检查通过和用户保存才更新账号。令牌/刷新令牌不进 SQLite、IPC、普通文件和日志；授权失败不覆盖旧凭据。现有密钥项增加自描述编码，旧密码项仍可读取。
5. 到期前120秒按凭据键串行刷新；新 refresh token 持久化并读回确认后才使用。不同凭据键互不锁定；不可确认保存、刷新网络失败或授权失效均不回退密码认证。第三方库可能产生内存副本，不承诺全部清零。刷新不授权 SMTP 重发。
6. schema20 顺序迁移至21，只保存认证类型，旧账号默认 password。保留迁移前备份、旧数据和未来版本拒绝。严格 DTO 新增认证类型及登录命令，IPC v3 升至 v4，旧端在写操作前拒绝，前后台必须一起升级。
7. 复用已锁定的 reqwest / url / sha2 / serde / base64，未升级版本。账号页复用已有表单、连接检查、保存结果查询与 M3E 控件；不新增登录子窗口或 WebView。

## 验证与限制

相关回调、令牌、XOAUTH2、存储迁移、协议和账号工作流测试及原生表单检查见[实现报告](../reports/Outlook-OAuth接入.md)。已收到应用 ID，但未核实门户注册项；模拟测试不代表微软真实兼容；应用注册、权限及操作步骤见[接入说明](../Outlook邮箱接入.md)。公开分发与租户许可另行核实。
