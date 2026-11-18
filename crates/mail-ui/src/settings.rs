//! 产品设置：主题、阅读字号、列表密度、减少动态；保存在 XDG 配置目录。
use gpui_kit::{
    App, Bounds, Context, Global, Render, TitlebarOptions, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, size,
};
use gpui_kit_material::{prelude::*, theme};
use std::{fs, io::Write, path::PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Preferences {
    pub dark: bool,
    pub reader_font: f32,
    pub compact: bool,
    pub reduce_motion: bool,
    pub show_log: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            dark: false,
            reader_font: 16.0,
            compact: false,
            reduce_motion: false,
            show_log: false,
        }
    }
}
impl Preferences {
    fn path() -> Option<PathBuf> {
        mail_storage::AppPaths::discover()
            .ok()
            .map(|paths| paths.config.join("settings.txt"))
    }
    fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        Self::parse(&text)
    }
    fn parse(text: &str) -> Self {
        let mut preferences = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match (key.trim(), value.trim()) {
                ("theme", value) => preferences.dark = value == "dark",
                ("reader_font", value) => {
                    if let Ok(value) = value.parse::<f32>() {
                        preferences.reader_font = value.clamp(12.0, 28.0);
                    }
                }
                ("compact", value) => preferences.compact = value == "1",
                ("reduce_motion", value) => preferences.reduce_motion = value == "1",
                ("show_log", value) => preferences.show_log = value == "1",
                _ => {}
            }
        }
        preferences
    }
    fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let _ = fs::create_dir_all(parent);
        let temp = parent.join("settings.txt.tmp");
        let content = format!(
            "theme={}\nreader_font={}\ncompact={}\nreduce_motion={}\nshow_log={}\n",
            if self.dark { "dark" } else { "light" },
            self.reader_font,
            u8::from(self.compact),
            u8::from(self.reduce_motion),
            u8::from(self.show_log)
        );
        let result = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .and_then(|mut file| file.write_all(content.as_bytes()))
            .and_then(|()| fs::rename(&temp, &path));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
    }
}

struct GlobalPreferences(Preferences);
impl Global for GlobalPreferences {}

pub(crate) fn init(cx: &mut App) {
    let preferences = Preferences::load();
    apply_theme(&preferences, cx);
    cx.set_global(GlobalPreferences(preferences));
}
pub(crate) fn prefs(cx: &App) -> Preferences {
    cx.global::<GlobalPreferences>().0.clone()
}
/// 保存并应用；主题立即切换，其余窗口重绘。
pub(crate) fn set(cx: &mut App, preferences: Preferences) {
    preferences.save();
    apply_theme(&preferences, cx);
    cx.set_global(GlobalPreferences(preferences));
    cx.refresh_windows();
}
/// 只持久化，不立即改主题（Home 主题动画使用）。
pub(crate) fn persist(cx: &mut App, preferences: Preferences) {
    preferences.save();
    cx.set_global(GlobalPreferences(preferences));
}
fn apply_theme(preferences: &Preferences, cx: &mut App) {
    theme::init_component_theme(
        ThemeId::Baseline.scheme(
            if preferences.dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            Contrast::Standard,
        ),
        1.0,
        cx,
    );
}

pub(crate) fn open(cx: &mut App) {
    if crate::lifecycle::reuse_settings(cx) {
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(720.), px(700.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — 设置".into()),
            ..Default::default()
        }),
        ..crate::ui::window_options()
    };
    match gpui_kit::open_window(options, cx, |_, cx| cx.new(Settings::new)) {
        Ok((window, _)) => crate::lifecycle::register_settings(window, cx),
        Err(error) => eprintln!("无法打开设置窗口：{error}"),
    }
}

struct Settings {
    store: mail_daemon::storage::StorageClient,
    cache: Option<mail_protocol::CacheInfo>,
    desktop: Option<mail_protocol::DesktopSettings>,
    desktop_busy: bool,
    autostart: Option<mail_protocol::AutostartSettings>,
    autostart_busy: bool,
    autostart_error: String,
    desktop_error: String,
    busy: bool,
    confirm_clear: bool,
    status: String,
}
impl Settings {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            store: crate::lifecycle::storage(cx),
            cache: None,
            desktop: None,
            desktop_busy: false,
            autostart: None,
            autostart_busy: false,
            autostart_error: String::new(),
            desktop_error: String::new(),
            busy: false,
            confirm_clear: false,
            status: String::new(),
        };
        this.cache_command(mail_protocol::Command::CacheStatus, cx);
        this.desktop_command(None, cx);
        this.autostart_command(None, cx);
        this
    }
    fn autostart_command(&mut self, enabled: Option<bool>, cx: &mut Context<Self>) {
        if self.autostart_busy {
            return;
        }
        let command = if let Some(enabled) = enabled {
            let Some(settings) = &self.autostart else {
                return;
            };
            mail_protocol::Command::SetAutostart {
                enabled,
                revision: settings.revision,
            }
        } else {
            mail_protocol::Command::AutostartSettings
        };
        self.autostart_busy = true;
        self.autostart_error.clear();
        let store = self.store.clone();
        let task = cx
            .background_executor()
            .spawn(async move { store.control(command) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.autostart_busy = false;
                match result {
                    Ok(mail_protocol::Reply::AutostartSettings(settings)) => {
                        this.autostart = Some(settings)
                    }
                    Ok(_) => this.autostart_error = "自启动设置响应无效，请刷新".into(),
                    Err(e) => this.autostart_error = format!("{e}；请刷新核对实际状态"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn desktop_command(&mut self, change: Option<u8>, cx: &mut Context<Self>) {
        if self.desktop_busy {
            return;
        }
        let command = if let Some(change) = change {
            let Some(settings) = &self.desktop else {
                return;
            };
            let mut preferences = settings.preferences.clone();
            match change {
                0 => preferences.notifications = !preferences.notifications,
                1 => preferences.show_sender_subject = !preferences.show_sender_subject,
                _ => preferences.show_preview = !preferences.show_preview,
            }
            mail_protocol::Command::UpdateDesktopSettings { preferences }
        } else {
            mail_protocol::Command::DesktopSettings
        };
        self.desktop_busy = true;
        self.desktop_error.clear();
        let store = self.store.clone();
        let task = cx
            .background_executor()
            .spawn(async move { store.control(command) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.desktop_busy = false;
                match result {
                    Ok(mail_protocol::Reply::DesktopSettings(settings)) => {
                        this.desktop = Some(settings)
                    }
                    Ok(_) => this.desktop_error = "后台桌面设置响应无效".into(),
                    Err(e) => this.desktop_error = format!("{e}；请刷新设置核对已保存状态"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn cache_command(&mut self, command: mail_protocol::Command, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.confirm_clear = false;
        self.status = "正在更新缓存…".into();
        let store = self.store.clone();
        let task = cx
            .background_executor()
            .spawn(async move { store.control(command) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(mail_protocol::Reply::Cache(info)) => {
                        this.cache = Some(info);
                        this.status.clear();
                    }
                    Ok(_) => this.status = "后台缓存响应无效".into(),
                    Err(error) => this.status = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
impl Render for Settings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preferences = prefs(cx);
        let mut column = div()
            .id("settings-scroll")
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .size_full()
            .bg(theme::color(ColorRole::SURFACE))
            .child(div().text_size(px(22.)).child("设置"))
            .child("设置保存在 XDG 配置目录；主题立即作用于所有窗口。")
            .child(
                div().flex().gap_2().items_center().child("主题").child(
                    MaterialButton::new("settings-theme")
                        .variant(ButtonVariant::Tonal)
                        .child(if preferences.dark { "深色" } else { "浅色" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            let mut preferences = prefs(cx);
                            preferences.dark = !preferences.dark;
                            set(cx, preferences);
                            cx.notify();
                            let _ = this;
                        })),
                ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(format!("阅读字号：{:.0}", preferences.reader_font))
                    .child(
                        MaterialButton::new("settings-font-smaller")
                            .child("−")
                            .on_click(cx.listener(|_, _, _, cx| {
                                let mut preferences = prefs(cx);
                                preferences.reader_font = (preferences.reader_font - 2.0).max(12.0);
                                set(cx, preferences);
                                cx.notify();
                            })),
                    )
                    .child(
                        MaterialButton::new("settings-font-larger")
                            .child("＋")
                            .on_click(cx.listener(|_, _, _, cx| {
                                let mut preferences = prefs(cx);
                                preferences.reader_font = (preferences.reader_font + 2.0).min(28.0);
                                set(cx, preferences);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(format!(
                        "列表密度：{}",
                        if preferences.compact {
                            "紧凑"
                        } else {
                            "舒适"
                        }
                    ))
                    .child(
                        MaterialButton::new("settings-density")
                            .variant(ButtonVariant::Tonal)
                            .child(if preferences.compact {
                                "切换为舒适"
                            } else {
                                "切换为紧凑"
                            })
                            .on_click(cx.listener(|_, _, _, cx| {
                                let mut preferences = prefs(cx);
                                preferences.compact = !preferences.compact;
                                set(cx, preferences);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(format!(
                        "减少动态：{}",
                        if preferences.reduce_motion {
                            "开"
                        } else {
                            "关"
                        }
                    ))
                    .child(
                        MaterialButton::new("settings-motion")
                            .variant(ButtonVariant::Tonal)
                            .child(if preferences.reduce_motion {
                                "关闭"
                            } else {
                                "开启"
                            })
                            .on_click(cx.listener(|_, _, _, cx| {
                                let mut preferences = prefs(cx);
                                preferences.reduce_motion = !preferences.reduce_motion;
                                set(cx, preferences);
                                cx.notify();
                            })),
                    ),
            );
        column = column.child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child("主窗口诊断日志")
                .child(
                    MaterialButton::new("settings-log")
                        .variant(ButtonVariant::Tonal)
                        .child(if preferences.show_log {
                            "关闭"
                        } else {
                            "开启"
                        })
                        .on_click(cx.listener(|_, _, _, cx| {
                            let mut preferences = prefs(cx);
                            preferences.show_log = !preferences.show_log;
                            set(cx, preferences);
                            cx.notify();
                        })),
                ),
        );
        column = column.child(
            div()
                .text_size(px(12.))
                .child("减少动态用于关闭主题过渡；产品窗口本身没有持续动画。"),
        );
        column = column.child(div().text_size(px(20.)).child("桌面通知"));
        if let Some(settings) = &self.desktop {
            let p = &settings.preferences;
            let mut row = div().flex().flex_wrap().gap_2();
            for (id, label, on, change) in [
                ("desktop-notifications", "新信通知", p.notifications, 0),
                ("desktop-sender", "发件人和主题", p.show_sender_subject, 1),
                ("desktop-preview", "已缓存正文预览", p.show_preview, 2),
            ] {
                row = row.child(
                    MaterialButton::new(id)
                        .variant(ButtonVariant::Tonal)
                        .disabled(self.desktop_busy)
                        .child(format!("{label}：{}", if on { "开" } else { "关" }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.desktop_command(Some(change), cx)
                        })),
                );
            }
            column = column
                .child(row)
                .child(settings.notification_status.clone());
        }
        column = column
            .child("默认只显示账号与数量；预览不触发下载。锁屏及勿扰展示由系统决定。")
            .child(
                MaterialButton::new("desktop-refresh")
                    .disabled(self.desktop_busy)
                    .child("刷新通知状态")
                    .on_click(cx.listener(|this, _, _, cx| this.desktop_command(None, cx))),
            )
            .child(self.desktop_error.clone());
        column = column.child(div().text_size(px(20.)).child("登录后后台启动"));
        if let Some(settings) = &self.autostart {
            let desired = settings.desired;
            column = column
                .child(format!(
                    "登录启动项：{}{}",
                    if settings.enabled {
                        "已启用"
                    } else {
                        "未启用"
                    },
                    if settings.pending {
                        " · 设置尚未完成"
                    } else {
                        ""
                    }
                ))
                .child(settings.status.clone())
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            MaterialButton::new("autostart-toggle")
                                .variant(ButtonVariant::Tonal)
                                .disabled(self.autostart_busy)
                                .child(if desired {
                                    "关闭登录启动"
                                } else {
                                    "开启登录启动"
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.autostart_command(Some(!desired), cx)
                                })),
                        )
                        .child(
                            MaterialButton::new("autostart-retry")
                                .disabled(self.autostart_busy)
                                .child("重新应用当前选择")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.autostart_command(Some(desired), cx)
                                })),
                        ),
                );
        }
        column=column.child(MaterialButton::new("autostart-refresh").disabled(self.autostart_busy).child("刷新启动状态")
            .on_click(cx.listener(|this,_,_,cx|this.autostart_command(None,cx))))
            .child(self.autostart_error.clone())
            .child("启动项需要桌面会话支持 XDG 自启动；仅启动后台，不打开主窗口。关闭此选项不会停止当前后台；保存并退出只结束本次会话。")
            .child("没有可见托盘时，后台仍可运行；再次打开 QingMail 返回前台，使用保存并退出停止后台。");
        column = column.child(div().text_size(px(20.)).child("邮件缓存"));
        if let Some(info) = &self.cache {
            let mib = |v: u64| v as f64 / 1048576.;
            column = column.child(format!(
                "预算 {:.0} MiB · 可清理 {:.1} MiB · 受保护 {:.1} MiB · 正在下载 {:.1} MiB",
                mib(info.budget_bytes),
                mib(info.evictable_bytes),
                mib(info.protected_bytes),
                mib(info.downloading_bytes)
            ));
            if info.protected_bytes > info.budget_bytes {
                column = column.child("受保护内容超过预算；请提高预算或自行整理草稿与附件，应用不会自动删除这些内容。");
            }
        }
        let mut budgets = div().flex().gap_2().items_center().child("预算");
        for (id, label, bytes) in [
            ("cache-256", "256 MiB", 256 * 1024 * 1024),
            ("cache-1024", "1 GiB", 1024 * 1024 * 1024),
            ("cache-4096", "4 GiB", 4 * 1024 * 1024 * 1024),
        ] {
            budgets = budgets.child(
                MaterialButton::new(id)
                    .disabled(self.busy)
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cache_command(mail_protocol::Command::SetCacheBudget { bytes }, cx)
                    })),
            );
        }
        column.child(budgets).child(div().flex().gap_2().child(
            MaterialButton::new("cache-refresh").disabled(self.busy).child("刷新用量").on_click(cx.listener(|this, _, _, cx| this.cache_command(mail_protocol::Command::CacheStatus, cx)))
        ).child(MaterialButton::new("cache-clear").disabled(self.busy).child(if self.confirm_clear { "确认清理可下载缓存" } else { "清理邮件缓存" }).on_click(cx.listener(|this, _, _, cx| {
            if this.confirm_clear { this.cache_command(mail_protocol::Command::ClearCache, cx); }
            else { this.confirm_clear = true; cx.notify(); }
        })))).child("只清理可重新下载的正文与附件；保留邮件索引、草稿、发送快照、冲突原件和未完成操作。清理后需要联网重新下载。浏览器副本请在阅读器中单独清理。")
        .child(self.status.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_parse_defaults_clamp_and_ignore_garbage() {
        let preferences = Preferences::parse(
            "theme=dark\nreader_font=99\ncompact=1\nreduce_motion=1\nunknown=x\n",
        );
        assert!(preferences.dark);
        assert_eq!(preferences.reader_font, 28.0);
        assert!(preferences.compact);
        assert!(preferences.reduce_motion);
        let defaults = Preferences::parse("theme=light\nreader_font=abc\n");
        assert!(!defaults.dark);
        assert!(!defaults.show_log);
        assert!(Preferences::parse("show_log=1\n").show_log);
        assert!(!Preferences::parse("show_log=0\n").show_log);
        assert_eq!(defaults.reader_font, 16.0);
    }
}
