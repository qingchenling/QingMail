# QingMail

Linux 原生邮件客户端，Rust + GPUI Kit + gpui-kit-material（Material Design 3 Expressive），优先支持 Arch Linux / Hyprland / Wayland。使用通用 IMAP/SMTP，不依赖 Electron 或 WebView。

已实现多账号与统一收件箱、本地搜索、按需阅读与附件下载、离线操作、本地及服务器草稿、显式确认发送、后台常驻、桌面通知和可选自启动。当前功能开发已完成；集中桌面验收与持续使用尚未全部完成，不等同于公开发行版。

## 构建与运行

已验证 Rust 1.100.0；依赖由 Cargo.lock 锁定。系统构建依赖和运行条件见 [Arch 使用说明](packaging/arch/README.md)。

```sh
cargo build --release --locked -p mail-ui -p mail-daemon
./target/release/mail-ui
```

前后台需放在同一目录。首次启动自动连接或启动后台，每次打开应用请求一次合并同步。关闭主窗口会保存编辑内容并退出前台，后台继续；使用“保存并退出”停止两个进程。需要 Wayland、匹配 GPU 的 Vulkan 驱动、会话 D-Bus 和 Secret Service。普通运行沿用系统图形及电源配置。

默认使用正式 XDG 数据目录，配置账号后可真实收发邮件。隔离验证按 [开发与验证](docs/开发与验证.md) 操作，勿将正式数据目录用作测试夹具。

## 本地 Arch 安装包

```sh
python3 tools/build_arch_package.py --output dist/arch-preview
```

输出目录须不存在。工具只构建，不自动安装。升级前先保存并退出前后台；卸载保留用户数据与凭据。详细步骤见 [安装与升级说明](packaging/arch/README.md)。

## 文档入口

| 文档 | 用途 |
| --- | --- |
| [项目状态](docs/项目状态.md) | 当前交付、待验收边界与后续范围 |
| [主交接文档](项目构建与开发交接文档.md) | 需求、架构约束、构建与验收基线 |
| [模块复用说明](docs/项目全貌与模块复用说明.md) | 代码入口、职责和扩展方法 |
| [开发与验证](docs/开发与验证.md) | 测试工具、隔离方式、GPUI 使用约束 |
| [验证记录与已知限制](docs/验证记录与已知限制.md) | 实测来源、尚未覆盖的场景与产品限制 |
| [ADR](docs/adr/) | 长期有效的技术决策；冲突时优先于基线 |
| [多账号与 IPC 规格](docs/design/P3-001-多账号离线与IPC设计.md) | 离线、资源、草稿及缓存边界 |
| [桌面集成规格](docs/design/P4-001-日常使用与桌面集成设计.md) | 通知、自启动、性能与安装规范 |

旧阶段待办、原型样稿、逐轮报告及旧测试包已清理；历史材料可通过 Git 历史查阅，不再作为当前实现说明。

仓库尚未选定项目许可证；当前包仅供本地预览，公开发布与许可证审查未完成。
