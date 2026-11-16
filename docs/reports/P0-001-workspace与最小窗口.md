# P0 报告 001：Workspace 建立、依赖锁定与最小原生 Wayland 窗口

- 日期：2026-11-16
- 状态：有条件通过（程序侧验证完成；输入/字体/缩放的视觉确认待操作者复核）
- 关联：主文档 §8、§9（P0）、§10.4、§11 任务 1/2；ADR-0001

## 1. 环境

| 项 | 值 |
| --- | --- |
| 发行版 | Arch Linux |
| 会话 | 原生 Wayland（`WAYLAND_DISPLAY=wayland-1`），Hyprland 合成器 |
| Rust | 系统包 `rust 1.100.0 (0ef1bc386 2026-11-09)`（未安装 rustup，`rust-toolchain.toml` 在本机不生效） |
| 硬件 | Raptor Lake-S UHD（iGPU）+ RTX 4070 Max-Q；Vulkan ICD 仅有 `nvidia_icd.json` |
| 编译资源 | 32 核 / 30 GiB 内存 |

## 2. 锁定的依赖

| 包 | 版本 | 说明 |
| --- | --- | --- |
| gpui-kit | =0.9.1 | 门面；GPUI 由精确 pin 的 gpui-pre-* 快照提供，当前解析为 gpui-pre 0.3.12 |
| gpui-kit-material | =0.1.4 | 关闭默认 feature 的 `gpui-fast`（其会转发开启 gpui-kit 的 fast 后端），与默认上游 snapshot 对齐；启用 `full`、`serde`、`embedded-symbols` |
| rust-toolchain.toml | channel 1.100.0 | 本机无 rustup 时不生效，已注明 |

`Cargo.lock` 已生成并随仓库提交。

## 3. 已验证命令与结果

| 命令 | 结果 |
| --- | --- |
| `cargo generate-lockfile` + `cargo fetch --locked` | 通过 |
| `cargo fmt --all` | 通过 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过（exit 0；余 5 条为 stub crate 的 manifest 未用依赖提示，待实现消除） |
| `cargo test --workspace --locked` | 通过（5 个测试） |
| `cargo build -p mail-ui --locked` | 通过（dev profile） |
| `cargo run -p mail-ui`（窗口） | 见下 |

## 4. 窗口运行证据

- 启动命令：`./target/debug/mail-ui`（继承 `WAYLAND_DISPLAY=wayland-1`）
- 进程 6 秒后存活，stderr 无任何输出
- 权威判定（Hyprland）：

```json
{"title": "QingMail", "pid": 24419, "xwayland": false,
 "floating": false, "workspace": {"id": 1, "name": "1"}}
```

`xwayland: false` 且已平铺于工作区 1 → **原生 Wayland 客户端验证通过**，满足主文档 §1 红线。
- 渲染：M3E Baseline 浅色方案初始化成功，窗口无崩溃、无重绘异常报告（长时间静止重绘检查留待后续迭代）。

## 5. 已知问题与限制

1. 本机 Vulkan 仅有 NVIDIA ICD；若 Hyprland 运行在 Intel iGPU 上，后续需确认实际使用的渲染设备，或安装 mesa Vulkan ICD。窗口本次未崩溃，渲染设备归属未单独测量。
2. `rust-toolchain.toml` 在无 rustup 的本机不生效；安装 rustup 前以系统 Rust 版本为准。
3. 未验证项：窗口内文字输入与中文 IME、100%/125%/150%/200% 缩放、列表与菜单交互——当前最小窗口尚无输入组件，待下一迭代（列表 + 输入框 + 菜单演示窗口）补充后按 §10.3 复测。
4. mail-ui 尚未引用 material 组件的交互组件（仅主题初始化），组件实测在下一迭代进行。

## 6. 下一步

1. P0 迭代 2：演示窗口加入列表、文本输入、菜单与主题切换，覆盖 §11 任务 2 的输入/字体/缩放确认项。
2. P0 迭代 3：合成 MIME 样本集 + mail-content 解析/简化阅读/浏览器副本（任务 3）。
3. P0 迭代 4：富文本编辑验证（任务 4，ADR-0006）。
