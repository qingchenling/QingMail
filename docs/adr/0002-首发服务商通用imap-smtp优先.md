# ADR-0002：首发服务商为通用 IMAP/SMTP，OAuth（Gmail/Outlook）延后

- 状态：已接受
- 日期：2026-11-16
- 关联：主文档 §1、§4.2、§9、§11

## 背景

原计划首版同时覆盖 Outlook、Gmail 和通用 IMAP/SMTP。核实服务商现状后发现：

1. Gmail IMAP/XOAUTH2 必须申请全量 `mail.google.com` restricted scope。
2. External 用户类型且处于 Testing 状态的 Google Cloud 项目，refresh token 在授权后 7 天过期（授权时起算，与使用频率无关），会周期性打断日常使用；发布到 production 可解除 7 天限制，但 unverified 状态有警告页和 100 用户总量上限（个人及少量用户可接受）。
3. Microsoft 个人账号同样需要 Azure 应用注册流程，且无租户管理员兜底。
4. 通用 IMAP/SMTP 账号密码或授权码路径没有上述外部审核依赖，可立即实现并覆盖大多数自建与企业邮箱。

## 决策

1. 首发仅实现通用 IMAP/SMTP 账号：账号密码或服务商授权码，IMAP 与 SMTP 可分别配置主机、端口和加密方式；全程校验 TLS 证书。
2. Gmail/Outlook 的 OAuth 接入延后，在通用单账号闭环（P2）稳定后按需启动，最早进入 P4。
3. 接入 OAuth 前须在 ADR 或验证报告中重新核实：scope 分类、Testing/production 状态与 token 生命周期、unverified 限制、桌面客户端授权码 + PKCE 的可用性。
4. OAuth 相关的账号状态字段（授权失效、重新认证）在数据模型中保留占位，但不实现流程。

## 后果

- P2 不再依赖任何外部审核，验收路径完全由本地可控条件决定。
- 首版用户（含开发者本人）需使用支持 IMAP/SMTP 的邮箱；Gmail 用户可启用应用密码（需两步验证）走通用路径。
- OAuth 风险（restricted scope 审核、7 天 token、分发资格）从"实现风险"降级为"记录在案的延后风险"，在 §11 风险表中跟踪。
