# QingMail Arch 本地预览包

P4 日常使用预览，版本 0.1.0.p4，数据库 schema19、IPC v2。尚非 P5 公开发行包；项目尚未选定许可证，NOTICE 和 LicenseRef-Unspecified 不构成再分发授权。不要发布本地构建产物。

## 构建与安装

在仓库根目录执行 `python3 tools/build_arch_package.py --output dist/arch-preview`。输出目录必须不存在。工具创建带逐文件摘要、提交号及脏工作区标记的源码快照、PKGBUILD、.SRCINFO、源码包、安装包及 SHA256SUMS；然后运行 makepkg，不自动安装依赖或软件。构建需 Arch base-devel、Rust >=1.100.0、clang、cmake、pkgconf、desktop-file-utils，以及 PKGBUILD 中的运行依赖。首次 Cargo 构建需要访问锁文件对应的依赖源。

安装前先在 QingMail 选择“保存并退出”，确保前后台均已退出；保存失败时应先解决，不强行终止。用户自行执行 `sudo pacman -U /绝对路径/qingmail-0.1.0.p4-1-x86_64.pkg.tar.zst`。前台和后台必须一起更新，不能混用旧 P3 包。运行 `mail-ui` 或桌面菜单 QingMail 启动。

包安装 `/usr/bin/mail-ui` 和 `/usr/bin/mail-daemon`，复用程序现有同目录查找机制。桌面入口及图标同名 `org.qingmail.QingMail`，所有 GPUI 窗口使用此 Wayland app_id。没有 mailto 协议处理声明，不新增系统服务、强制 GPU 配置或默认自启动。

## 运行条件

需要 Wayland 桌面、匹配 GPU 的 Vulkan 驱动、会话 D-Bus 和可解锁的 Secret Service（例如已正确配置的 gnome-keyring 或兼容服务）；后者用于凭据，不会把密码写进包或普通文件。中文建议 noto-fonts-cjk，通用文字建议 noto-fonts。依赖中的 Vulkan loader 不代替硬件驱动。通知、托盘和自动启动取决于桌面宿主支持；缺失时沿用程序的降级行为。无托盘时可再次运行 mail-ui 打开前台，退出应使用程序的保存并退出流程。

Arch 滚动发行的二进制仅针对构建时的 ABI。OpenSSL 等库发生 ABI 升级时应重建本包；本地已安装依赖的验证不等于干净虚拟机兼容验收。

## 升级、卸载和保留数据

升级前必须保存并退出前后台，然后用 pacman 同时替换两个程序；本包不使用强杀进程的安装钩子，不支持运行中的热更新。数据库由原有顺序迁移、备份和未来版本拒绝机制处理；升级前建议另行备份用户数据。

卸载同样先保存并退出，再自行执行 `sudo pacman -R qingmail`。卸载保留 `$XDG_DATA_HOME/qingmail`、`$XDG_CONFIG_HOME/qingmail`、`$XDG_CACHE_HOME/qingmail`（未设置时分别位于 ~/.local/share、~/.config、~/.cache）及密钥库凭据。先在设置关闭自启动可清理受管用户启动项；未清理的启动项包含 TryExec，程序不存在时不应执行。若安装后曾选择自启动，应避免再运行其他路径的旧测试包而改变受管路径。

需要完全清理时，先备份并确认草稿、附件、发送记录不再需要，再人工检查并清理上述目录及用户 autostart 中的 QingMail 项；凭据通过 Secret Service 管理器处理。本包不自动删除用户文件或密钥库项。浏览器阅读副本使用程序现有独立清理入口。

## 验证边界

tools/arch_package_probe.py 使用隔离 pacman 根目录验证安装、包修订升级和卸载，不修改本机包数据库；隔离事务跳过依赖和安装脚本，不能代替干净系统运行验证。P4-006 再集中验证真实桌面、登录、恢复、阶段全量及持续使用；不把短时打包冒烟当成完整验收。
