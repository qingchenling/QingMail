# P4-005 Arch 安装包与桌面入口

日期：2026-11-18。范围依据 P4-001 §6；不改邮件业务、数据库 schema19、IPC v2，也不增加系统服务、自动更新或默认自启动。

## 实现

- `packaging/arch/PKGBUILD.in`、`tools/build_arch_package.py`：从当前工作区生成有 SHA256 校验的源码包、PKGBUILD、.SRCINFO、Arch 二进制包及摘要；只运行 makepkg，不安装主机软件。源码包含阅读验证窗口编译必需的合成 MIME 样本。
- 包内配套安装 `/usr/bin/mail-ui`、`/usr/bin/mail-daemon`，保留现有同目录启动机制。BUILD-INFO.json 记录源提交、工作区是否有未提交修改、逐文件摘要、时间和 Rust 版本；脏工作区产物不能等同于记录的 HEAD。
- 桌面入口、hicolor scalable 原创信封图标和托盘图标名统一为 `org.qingmail.QingMail`；所有应用窗口复用 `ui::window_options()` 设置 Wayland app_id。GPUI 的应用名称设置不能代替逐窗口 app_id。托盘仍保留原有内嵌像素后备图标。
- `--version` 去除过期 P3 测试包标签，改为 P4 preview / IPC v2。未声明不支持的 mailto 处理。
- 运行依赖覆盖本机实际链接的 glibc、gcc-libs、OpenSSL、xcb、xkbcommon 及动态使用的 Wayland/Vulkan loader/字体/D-Bus/CA/xdg-utils；GPU 驱动、Secret Service 和字体列为可选依赖并明确实际使用条件。安装入口不设置 VK_DRIVER_FILES，不修改电源、缩放、合成器配置。
- 安装使用说明在 `packaging/arch/README.md`：先保存并退出前后台，失败不强杀；配套升级，卸载保留用户数据、配置、缓存、凭据和自启动项，说明 TryExec 与手工清理方式。旧包不读取新版本库的约束继续生效。

## 验证方式与边界

`tools/arch_package_probe.py` 在临时根目录和独立 pacman 数据库执行真实包事务，使用 fakeroot 而非主机 root 安装；跳过依赖和钩子，明确不能替代干净 Arch 虚拟机的依赖求解与运行验证。升级使用相同软件、不同 pkgrel 的包修订，不能据此声称验证了旧 schema 的数据迁移。用户目录与密钥库使用哨兵文件验证保留，不访问真实邮箱和 Secret Service。

窗口探针复用正式 mailbox、IPC 和独立后台，使用五账号合成缓存；`--verify-app-id` 只读 Hyprland 客户端列表，检查实际窗口 class 和原生 Wayland 身份；正常关闭前台后确认后台仍在，再协调退出隔离后台。驱动环境仅属于本机探针，不写入安装包。

## 实际结果与产物

- makepkg 完成 release/locked 配套构建、check、fakeroot 安装布局整理和 zstd 打包；desktop-file-validate 通过。
- 安装包 `dist/arch-p4-005-final/qingmail-0.1.0.p4-1-x86_64.pkg.tar.zst`，52,301,272 bytes；SHA256 `9983dd5b88190c30a5622688c3a5aff9dde417d9eaf67270f4fc25526618b1f2`。生成的 PKGBUILD、.SRCINFO、源码包和 SHA256SUMS 在同目录；未安装至主机。构建来自 8082f0c 后的本轮脏工作区，准确内容以包内 BUILD-INFO.json 和源码摘要为准。
- 隔离 pacman 安装 `0.1.0.p4-1` → makepkg --repackage 生成的同软件修订 `0.1.0.p4-2` → 卸载均成功。工具使用独立无仓库配置、禁签名的本地夹具事务和双 --nodeps，非真实依赖求解；检查两个程序权限、桌面入口、图标、缺少系统服务/自动启动文件、库解析和版本输出。五类用户文件哨兵完整保留。fakeroot 下 pacman 提示无法实际设置 root 所有者，原始日志保留这些警告；本轮验证文件权限与包事务，不据此宣称真实 root 所有权安装已验收。
- 从包解出的 `/usr/bin` 布局启动 UI，成功启动同包 daemon；五账号、每账号万封合成缓存，30次快速选信、中文搜索、分页和窄窗探针通过。Hyprland 实测 class/initialClass 均为 `org.qingmail.QingMail`、xwayland=false；前台正常关闭后后台仍在，再正常退出隔离后台。探针使用本机 Intel 环境，安装入口本身不限定驱动。
- fmt --check、workspace check --locked、workspace all-targets clippy -D warnings 通过；仅运行 8 项 autostart_ 相关测试，全部通过（Unix socket 测试须在允许本地通信的环境运行）；Python 编译检查、PKGBUILD shell 语法及 desktop 验证通过，未运行阶段全量。

原始证据：[构建元信息](evidence/P4-005-build.json)、[隔离包事务](evidence/P4-005-package-transactions.json)、[包内窗口](evidence/P4-005-packaged-window.json)、[窗口日志](evidence/P4-005-packaged-window.log)。

复验隔离事务：`fakeroot -- python3 tools/arch_package_probe.py --package <rel1包> --upgrade <rel2包> --evidence <输出.json>`。升级夹具仅修改生成的 PKGBUILD 的 pkgrel=2，保持原 src 和 CARGO_TARGET_DIR 后运行 makepkg --repackage；不把修订2夹具作为另一版软件交付。

## 交付限制

这是本机 Arch 本地预览，不是 P5 公开分发包。仓库未选定项目许可证，本轮不代用户选许可证；LicenseRef-Unspecified 和 NOTICE 不授予再分发许可，完整许可证及发布审查仍属 P5。当前 Arch 的 OpenSSL ABI 为 4，库 ABI 变化后须重建，不能把本机 ldd 通过推断为其他滚动快照兼容。

真实桌面菜单启动、升级前保存失败交互、重新登录自启动、显示/恢复场景及持续使用与阶段全量继续留 P4-006；本轮未安装至主机，不读真实账号、不发送 SMTP。无 namcap 时不安装新工具冒充额外验证，以 makepkg、desktop-file-validate、实际链接与隔离事务提供本轮证据。
