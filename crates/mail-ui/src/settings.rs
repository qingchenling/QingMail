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
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            dark: false,
            reader_font: 16.0,
            compact: false,
            reduce_motion: false,
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
            "theme={}\nreader_font={}\ncompact={}\nreduce_motion={}\n",
            if self.dark { "dark" } else { "light" },
            self.reader_font,
            u8::from(self.compact),
            u8::from(self.reduce_motion)
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
            size(px(560.), px(420.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — 设置".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |_, cx| cx.new(Settings::new)) {
        Ok((window, _)) => crate::lifecycle::register_settings(window, cx),
        Err(error) => eprintln!("无法打开设置窗口：{error}"),
    }
}

struct Settings;
impl Settings {
    fn new(_cx: &mut Context<Self>) -> Self {
        Self
    }
}
impl Render for Settings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preferences = prefs(cx);
        let mut column = div()
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
                .text_size(px(12.))
                .child("减少动态用于关闭主题过渡；产品窗口本身没有持续动画。"),
        );
        column
    }
}
