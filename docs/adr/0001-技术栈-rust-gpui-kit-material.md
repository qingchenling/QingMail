# ADR-0001：技术栈采用 Rust + GPUI Kit + gpui-kit-material

- 状态：已接受
- 日期：2026-11-16
- 关联：主文档《项目构建与开发交接文档.md》§1、§2、§8

## 背景

项目需要一个 Linux 原生桌面邮件客户端，优先适配 Arch Linux / Hyprland / Wayland。候选方案中 Electron 与 WebView 类方案与"原生 Wayland、无浏览器引擎"的约束冲突；GPUI（Zed 的 UI 框架，经 longbridge/gpui-kit 分发）提供原生窗口、GPU 绘制与输入，gpui-kit-material（mizorewww/gpui-kit-material）在其上提供 Material Design 3 Expressive 主题与组件。两者均处于活跃变动期，API 可能随快照版本变化。

## 决策

1. 开发语言为 Rust，界面框架锁定 GPUI Kit `=0.9.1`，Material 层为 gpui-kit-material `=0.1.4`（关闭默认 `gpui-fast`，使用上游 snapshot）；GPUI 快照 gpui-pre `0.3.12`，性能 HUD gpui-fps `=0.9.1`。这是 P0 构建与实测后的锁定组合，以 Cargo.toml/Cargo.lock 为准。
2. 不使用 Electron，不使用 WebView 构建应用界面；首版不依赖 gpui-kit-webview。
3. 应用代码只通过 GPUI Kit 的统一门面使用 GPUI 类型，不混用不同来源的同名类型。
4. 锁定完整依赖：提交 Cargo.lock，Git 依赖固定完整修订，不追踪浮动分支。上游已有因浮动 GPUI 快照导致编译失败的先例（gpui-kit #3156），pin 精确快照是硬要求。
5. 升级版本必须记录原因并重新验证关键流程（P0 列表、阅读、编辑、主题切换）。

## 后果

- 获得原生 Wayland 路径与 M3E 视觉体系，符合项目约束。
- 承担预发布框架的 API 变动风险；通过锁版本、UI 适配层和阶段退出条件控制。
- 组件覆盖、可访问性、中文输入、Linux 行为必须在应用实际使用的组合中实测，组件展示页不作为验收依据。
- 早期构建/原生Wayland与动效事实已汇总到 [验证记录](../验证记录与已知限制.md)，API约束见 [开发与验证](../开发与验证.md)；旧逐轮原始记录可查Git历史；重新构建按主文档 §8.2 确认系统依赖。本次仅回写已验证的版本事实，不变更技术栈决策。
