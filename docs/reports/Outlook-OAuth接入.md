# Outlook / Microsoft 365 OAuth 接入

日期：2026-11-19。按用户要求新增微软邮箱支持；随后内置用户提供的应用 ID `fa0a5ba2-d5bb-49af-9683-58bc3cb517e7`，移除 ID 输入、偏好存储与环境变量覆盖入口。保留此前草稿布局、发送副本开关和删除同步改动；没有清理或重置未提交工作。

## 实际实现

- `mail-core::Authentication` 区分密码、微软个人及组织账号；旧序列化配置默认密码。schema20 → 21 只新增账号认证类型，保留迁移前备份、旧数据及未来版本拒绝。IPC v4 新增 BeginMicrosoftLogin / MicrosoftLoginStatus / CancelMicrosoftLogin，控制消息不接收令牌或可覆盖的客户端 ID。
- `mail-service/microsoft.rs`：系统浏览器授权码 + PKCE S256，consumers / organizations，固定微软端点和委托邮件协议权限。回调只监听本机随机端口，五分钟期限，16 KiB及整次两秒读取上限；检查 Host、路径、随机 state、重复参数和授权码控制字符，结束关闭监听。无客户端密钥、无 WebView。
- `credentials.rs`：令牌与刷新令牌只在 Secret Service；自描述编码与旧密码编码共存。访问令牌提前120秒刷新，同凭据键串行、不同账号独立；刷新轮换必须先写入并读回确认。撤销授权、网络失败和密钥库故障有固定中文原因，不向日志输出服务端响应或令牌，不退回密码认证。
- `accounts.rs`、`outgoing.rs`：IMAP AUTHENTICATE XOAUTH2 和 SMTP XOAUTH2，TLS/STARTTLS及证书检查继续沿用已有实现。生产入口验证固定微软服务器，防止向其他主机发送 bearer token。SMTP 一次投递、明确拒绝与结果不确定的语义保留，刷新/重启不自动发送。
- `mail-daemon/microsoft_login.rs`：异步会话，开始响应丢失可按原会话取消，同 ID 不重复开监听；取消和发布临时引用互斥，过期引用沿用临时密钥 TTL 清理。并发未结束的取消工作有上限，停止时有界等待。结果只交接临时密钥引用，继续复用 CheckManager / AccountCommitter，更新失败不覆盖旧账号引用。
- 主窗口账号页复用 M3E 表单，增加 Outlook / Hotmail 与 Microsoft 365 选择；仅名称、邮箱及浏览器登录，无密码或客户端 ID 输入。含取消、重新授权、双协议检查、明确保存与保存结果查询。连接诊断详情仍默认隐藏，检查失败会在状态行给出收发原因。

## 针对性验证

仅运行受影响部分，没有运行工作区全量测试：

| 命令或检查 | 结果 |
| --- | --- |
| 五个相关 crate 的 `microsoft` 过滤测试 | 最终内置ID版本17项通过：PKCE/state、回调注入/主机/重复参数、取消、令牌轮换/旧刷新令牌保留、密钥保存前置/分账号锁、XOAUTH2、schema20迁移、旧协议拒绝、取消与连接检查流 |
| `mail-service accounts::tests` | 13项通过，含通用账号TLS/STARTTLS、错误脱敏及仅检查不投递 |
| `mail-service credentials::` | 6项通过，含旧密码编码、临时引用/TTL、刷新与密钥库失败保护 |
| `mail-service outgoing::tests` | 10项通过，含SMTP接受/拒绝/结果不确定、单次提交及重启不重发 |
| `mail-daemon account_` | 16项单测和1项隔离跨进程测试通过，含旧凭据保留、过期检查、保存结果丢失、多账号重启 |
| `mail-ui accounts::` | 5项通过，含取消、仅引用检查和原保存结果查询 |
| `mail-storage account_credentials::` | 3项通过，含事务/凭据版本及schema9旧关联迁移 |
| `mail-protocol --lib` | 9项通过，含严格字段及旧端拒写 |

发送回归曾发现旧测试默认期待 CopyPending，与此前“发送副本默认关闭”改动不符；现显式开启副本偏好后检验原 CopyPending 分支，重跑通过。默认关闭路径继续由既有存储测试覆盖，不修改发送状态语义迎合测试。

原生账号表单使用合成输入及隔离数据目录，未打开真实授权、未读取或写入真实密钥。证据：[原生检查](evidence/outlook-accounts.json)、[运行日志](evidence/outlook-accounts.log)。覆盖通用/Outlook/Microsoft365页面、内置ID且无配置框、表单绑定及修改后检查失效，账号与发送任务均为0。

最终 `cargo fmt --all -- --check`、工作区 all-targets check、clippy（`-D warnings`）通过；配套 UI / daemon release 构建通过。内置ID版本的 release 原生账号检查通过，证据已更新。没有运行工作区全量测试。

## 交付与边界

- 实现决策见 [ADR-0010](../adr/0010-微软OAuth与凭据边界.md)；账号操作与微软门户所需配置见 [接入说明](../Outlook邮箱接入.md)。主基线、当前状态、模块地图及发行说明已更新。
- 未登录用户微软门户，未确认此应用的账号类型、委托权限或重定向URI。未进行真实微软授权、刷新、收信、草稿或SMTP发送验收，模拟服务器不能代替微软兼容结论。
- 普通应用仍遵循系统省电策略；本轮未重跑性能专项。Gmail、Graph、主权云、自定义OAuth服务器不在本次范围。
- 现有1.0.0 Arch安装包未更新，不包含本轮源码。前后台必须配套升级至IPC v4；不能用旧包daemon配新UI。没有安装、发布或访问真实邮箱。
