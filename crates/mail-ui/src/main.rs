//! QingMail 原生窗口入口：账号、只读邮件索引及既有开发验证窗口。

use gpui_kit::base::input::InputState;
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Bounds, ClickEvent, Context, Entity, FontWeight, IntoElement, Render, TitlebarOptions,
    Window, WindowBounds, WindowOptions, application, div, px, size,
};
use gpui_kit_material::icons::Assets;
use gpui_kit_material::prelude::*;
use gpui_kit_material::theme;

mod accounts;
mod composer;
mod lifecycle;
mod mailbox;
mod perf_probe;
mod reader;

/// 设 QINGMAIL_FPS=1 显示性能 HUD（帧率/帧耗时/GPU/CPU/内存）。
fn show_fps() -> bool {
    std::env::var("QINGMAIL_FPS").is_ok_and(|value| value == "1")
}

/// 合成邮件列表条目（发件人、主题）。
const MAILS: [(&str, &str); 6] = [
    ("Arch Linux 邮件列表", "[announce] 2026-11 稳定版发布公告"),
    ("GitHub", "[qingmail] P0-001 workflow 运行成功"),
    ("王工", "Re: 邮件客户端 P0 验证计划与分工"),
    ("Zulip · GPUI", "gpui-kit 0.9.1 发布说明与升级指引"),
    ("Gmail 团队", "你的存储空间即将达到上限"),
    ("自己", "草稿：MIME 简化阅读的安全边界确认"),
];

struct Home {
    input: Entity<InputState>,
    menu_open: bool,
    /// 目标明暗模式。过渡期间 `theme::is_dark()` 可能停在中间调，
    /// 因此由视图状态而非当前画板决定切换方向。
    dark: bool,
    /// 当前主题过渡目标；过渡期间被新目标覆盖即实现重定向。
    theme_target: Option<gpui_kit_material::color::Scheme>,
    /// 是否已有 pump 回调在帧队列中，保证全局只有一个待推进回调。
    theme_animating: bool,
    selected: Option<usize>,
    /// 仅限时诊断模式使用；普通窗口没有持续帧需求。
    probe_started: Option<std::time::Instant>,
}

impl Home {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        perf_probe::start(window, cx);
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("搜索已缓存邮件（仅演示输入与中文 IME）…")
        });
        Self {
            input,
            menu_open: false,
            dark: false,
            theme_target: None,
            theme_animating: false,
            selected: None,
            probe_started: None,
        }
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dark = !self.dark;
        let mode = if self.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        self.theme_target = Some(ThemeId::Baseline.scheme(mode, Contrast::Standard));
        cx.notify();
        if self.theme_animating {
            // 已有推进回调在飞：只更新目标，下一帧读取最新值完成重定向。
            return;
        }
        self.theme_animating = true;
        // 此处处于本实体的 update 上下文（listener / 菜单项的 entity.update），
        // 不能再对本实体 entity.update，首次推进必须内联执行。
        let target = self.theme_target.unwrap();
        let running = theme::advance_component_theme(target, 1.0, window, cx);
        if running {
            let entity = cx.entity();
            window.on_next_frame(move |window, cx| Self::pump_theme(entity, window, cx));
        } else {
            self.theme_target = None;
            self.theme_animating = false;
        }
    }

    /// 单一推进循环：每帧向最新目标采样一次；目标可被 toggle 更新。
    /// 仅从 on_next_frame 回调调用，此时不存在本实体的活跃 update。
    fn pump_theme(entity: Entity<Self>, window: &mut Window, cx: &mut App) {
        entity.update(cx, |this, cx| {
            let Some(target) = this.theme_target else {
                this.theme_animating = false;
                return;
            };
            let running = theme::advance_component_theme(target, 1.0, window, cx);
            if running {
                let next = entity.clone();
                window.on_next_frame(move |window, cx| Self::pump_theme(next, window, cx));
            } else {
                this.theme_target = None;
                this.theme_animating = false;
                cx.notify();
            }
        });
    }
}

impl Render for Home {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let probe_marker = self.probe_started.map(|started| {
            // 对照组：只靠绘制阶段的 GPUI 帧请求驱动，排除主题 pump。
            window.request_animation_frame();
            div()
                .absolute()
                .bottom(px(28.0))
                .left(px((started.elapsed().as_secs_f32() % 1.0) * 500.0))
                .size(px(12.0))
                .bg(theme::color(ColorRole::PRIMARY))
        });
        let theme_button = MaterialButton::new("theme-toggle")
            .variant(ButtonVariant::Tonal)
            .child("切换主题")
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.toggle_theme(window, cx);
            }));
        let menu_trigger = MaterialButton::new("menu-trigger")
            .variant(ButtonVariant::Filled)
            .child("菜单")
            // 指针点击由 MenuPopup 接管并触发 on_open_change；这里仅响应
            // 键盘激活，避免开关状态被双重翻转（参照上游示例）。
            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                if matches!(event, ClickEvent::Keyboard(_)) {
                    this.menu_open = !this.menu_open;
                    cx.notify();
                }
            }));

        let menu = MaterialMenu::new("main-menu").items([
            MaterialMenuItem::new("切换明暗主题").on_click({
                let entity = cx.entity();
                move |_, window, cx| {
                    entity.update(cx, |this, cx| this.toggle_theme(window, cx));
                }
            }),
            MaterialMenuItem::new("选中第一封邮件").on_click({
                let entity = cx.entity();
                move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.selected = Some(0);
                        cx.notify();
                    });
                }
            }),
            MaterialMenuItem::new("不可用操作（演示禁用态）").disabled(true),
        ]);

        let menu_popup = MenuPopup::new("main-popup", menu_trigger, menu)
            .close_on_activate(true)
            .open(self.menu_open)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.menu_open = *open;
                cx.notify();
            }));

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .py_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(20.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::color(ColorRole::ON_SURFACE))
                            .child("QingMail · P0 交互验证"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                            .child("输入 / 列表 / 菜单 / 明暗主题 — 合成数据"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        MaterialButton::new("mailbox")
                            .child("邮件")
                            .on_click(|_, _, cx| mailbox::open(cx)),
                    )
                    .child(
                        MaterialButton::new("account-settings")
                            .child("添加邮箱")
                            .on_click(|_, _, cx| accounts::open(cx)),
                    )
                    .child(
                        MaterialButton::new("reading-demo")
                            .variant(ButtonVariant::Tonal)
                            .child("阅读验证")
                            .on_click(|_, _, cx| reader::open(cx)),
                    )
                    .child(
                        MaterialButton::new("composer-demo")
                            .variant(ButtonVariant::Tonal)
                            .child("编辑验证")
                            .on_click(|_, _, cx| composer::open(cx)),
                    )
                    .child(
                        MaterialButton::new("quit-app")
                            .child("保存并退出")
                            .on_click(|_, _, cx| lifecycle::quit(cx)),
                    )
                    .child(menu_popup)
                    .child(theme_button),
            );

        let search = div()
            .px_4()
            .pb_2()
            .child(TextField::new("search", &self.input));

        let selected_note = match self.selected {
            Some(i) => format!("已选中：{}", MAILS[i].1),
            None => "未选中邮件（点击列表项测试选中态）".to_string(),
        };

        let list = div()
            .id("mail-list")
            .flex_1()
            .overflow_y_scroll()
            .px_2()
            .pb_3()
            .flex()
            .flex_col()
            .gap_1()
            .children(MAILS.iter().enumerate().map(|(i, (sender, subject))| {
                ListItem::new(i, *subject)
                    .overline(*sender)
                    .supporting("2026-11-15 · 合成样本，用于列表与选中态验证")
                    .selected(self.selected == Some(i))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.selected = if this.selected == Some(i) {
                            None
                        } else {
                            Some(i)
                        };
                        cx.notify();
                    }))
            }));

        let footer = div()
            .px_4()
            .py_2()
            .text_size(px(12.0))
            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
            .child(selected_note)
            .child(div().child(lifecycle::note(cx)));

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
            .child(header)
            .child(search)
            .child(
                div()
                    .mx_4()
                    .mb_2()
                    .h(px(1.0))
                    .bg(theme::color(ColorRole::OUTLINE_VARIANT)),
            )
            .child(list)
            .child(footer)
            .children(probe_marker)
            .when(show_fps(), |d| d.child(gpui_fps::fps_monitor(window, cx)))
    }
}

fn open_home(cx: &mut App) -> Result<gpui_kit::AnyWindowHandle, String> {
    let bounds = Bounds::centered(None, size(px(960.0), px(680.0)), cx);
    let options = WindowOptions {
        low_power_frame_interval: if perf_probe::unthrottled() {
            None
        } else {
            WindowOptions::default().low_power_frame_interval
        },
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — P0 交互验证".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Home::new(window, cx)))
        .map(|(window, _)| window)
        .map_err(|e| e.to_string())
}

fn main() {
    use mail_daemon::{Command, Instance, Launch};
    let args: Vec<_> = std::env::args().skip(1).collect();
    let command = if args.iter().any(|arg| arg == "--quit") {
        Command::Quit
    } else if args.iter().any(|arg| arg == "--status") {
        Command::Status
    } else if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--editor-demo" | "--lifecycle-probe" | "--lifecycle-failure-probe"
        )
    }) {
        Command::Editor
    } else if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--mailbox" | "--mailbox-probe"))
    {
        Command::Mailbox
    } else if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--accounts" | "--accounts-probe"))
    {
        Command::Accounts
    } else if args.iter().any(|arg| arg == "--content-demo") {
        Command::Reader
    } else {
        Command::Open
    };
    let isolated_probe = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--editor-probe" | "--editor-style-demo"))
        || std::env::var_os("QINGMAIL_PERF_PROBE").is_some();
    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--lifecycle-probe" | "--lifecycle-failure-probe"
        )
    }) && (std::env::var_os("QINGMAIL_P0_RUNTIME_ROOT").is_none()
        || std::env::var_os("QINGMAIL_P0_DRAFT_ROOT").is_none())
    {
        eprintln!("生命周期探针需要独立运行目录和草稿目录，未启动。");
        std::process::exit(1);
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--accounts-probe" | "--mailbox-probe"))
        && (std::env::var_os("QINGMAIL_P0_RUNTIME_ROOT").is_none()
            || std::env::var_os("QINGMAIL_P0_DRAFT_ROOT").is_none())
    {
        eprintln!("账号探针需要独立运行与数据目录");
        std::process::exit(1);
    }
    let instance = if isolated_probe {
        None
    } else {
        let root = std::env::var_os("QINGMAIL_P0_RUNTIME_ROOT")
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"));
        let Some(root) = root else {
            eprintln!("QingMail 启动失败：缺少 XDG_RUNTIME_DIR；未启动后台。");
            std::process::exit(1);
        };
        match Instance::launch(std::path::Path::new(&root), command) {
            Ok(Launch::Forwarded(reply)) => {
                print!("{reply}");
                return;
            }
            Ok(Launch::Primary(instance)) => Some(instance),
            Err(e) => {
                eprintln!("QingMail 启动/激活失败：{e}");
                std::process::exit(1);
            }
        }
    };
    application()
        .with_quit_mode(gpui_kit::QuitMode::Explicit)
        .with_assets(Assets::new())
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            gpui_kit_material::init(cx);
            let scheme = ThemeId::Baseline.scheme(ThemeMode::Light, Contrast::Standard);
            theme::init_component_theme(scheme, 1.0, cx);
            if let Err(e) = lifecycle::start(instance, cx) {
                eprintln!("无法启动合成后台：{e}");
                cx.quit();
                return;
            }
            lifecycle::open_main(cx);
            if command == Command::Mailbox {
                mailbox::open(cx);
            }
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--accounts" | "--accounts-probe"))
            {
                accounts::open(cx);
            }
            cx.activate(true);
            if command == Command::Reader {
                reader::open(cx);
            }
            if command == Command::Editor
                || args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "--editor-probe" | "--editor-style-demo"))
            {
                composer::open(cx);
            }
        });
}
