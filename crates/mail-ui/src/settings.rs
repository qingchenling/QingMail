//! 产品设置：主题、阅读字号、列表密度、减少动态；保存在 XDG 配置目录。
use gpui_kit::{App, Context, Global, Window, prelude::*};
use gpui_kit_material::{prelude::*, theme};
#[path = "settings_view.rs"]
mod view;
use std::{fs, io::Write, path::PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Preferences {
    pub dark: bool,
    pub reader_font: f32,
    pub compact: bool,
    pub reduce_motion: bool,
    pub show_log: bool,
    pub show_account_check_details: bool,
    pub auto_load_images: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            dark: false,
            reader_font: 16.0,
            compact: false,
            reduce_motion: false,
            show_log: false,
            show_account_check_details: false,
            auto_load_images: true,
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
                ("auto_load_images", value) => preferences.auto_load_images = value != "0",
                ("show_log", value) => preferences.show_log = value == "1",
                ("show_account_check_details", value) => {
                    preferences.show_account_check_details = value == "1"
                }
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
        let content = self.serialize();
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
    fn serialize(&self) -> String {
        format!(
            "theme={}\nreader_font={}\ncompact={}\nreduce_motion={}\nshow_log={}\nshow_account_check_details={}\nauto_load_images={}\n",
            if self.dark { "dark" } else { "light" },
            self.reader_font,
            u8::from(self.compact),
            u8::from(self.reduce_motion),
            u8::from(self.show_log),
            u8::from(self.show_account_check_details),
            u8::from(self.auto_load_images)
        )
    }
}

struct GlobalPreferences(Preferences);
impl Global for GlobalPreferences {}
// The mailbox root is the sole frame driver; rapid theme changes retarget it.
#[derive(Default)]
struct ThemeTransition(bool);
impl Global for ThemeTransition {}

pub(crate) fn init(cx: &mut App) {
    let preferences = Preferences::load();
    apply_theme(&preferences, cx);
    cx.set_global(GlobalPreferences(preferences));
    cx.set_global(ThemeTransition::default());
}
pub(crate) fn prefs(cx: &App) -> Preferences {
    cx.global::<GlobalPreferences>().0.clone()
}
/// 保存偏好；主题由主窗口绘制阶段统一推进，其余窗口重绘。
pub(crate) fn set(cx: &mut App, preferences: Preferences) {
    preferences.save();
    if preferences.reduce_motion || cx.reduce_motion() {
        apply_theme(&preferences, cx);
        cx.global_mut::<ThemeTransition>().0 = false;
    } else if preferences.dark != prefs(cx).dark {
        cx.global_mut::<ThemeTransition>().0 = true;
    }
    cx.set_global(GlobalPreferences(preferences));
    cx.refresh_windows();
}
/// 只持久化，不立即改主题（Home 主题动画使用）。
pub(crate) fn persist(cx: &mut App, preferences: Preferences) {
    preferences.save();
    cx.set_global(GlobalPreferences(preferences));
}
/// Only the mailbox root calls this, while drawing (never from a listener).
pub(crate) fn theme_running(cx: &App) -> bool {
    cx.global::<ThemeTransition>().0
}
pub(crate) fn advance_theme(window: &mut Window, cx: &mut App) {
    if !theme_running(cx) {
        return;
    }
    let preferences = prefs(cx);
    if crate::ui::motion::reduced(cx) {
        apply_theme(&preferences, cx);
        cx.global_mut::<ThemeTransition>().0 = false;
        return;
    }
    let mode = if preferences.dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    let running = theme::advance_component_theme(
        ThemeId::Baseline.scheme(mode, Contrast::Standard),
        1.0,
        window,
        cx,
    );
    cx.global_mut::<ThemeTransition>().0 = running;
    if running {
        window.request_animation_frame();
    }
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

pub(super) struct Settings {
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
    pub(super) fn saving(&self) -> bool {
        self.busy || self.desktop_busy || self.autostart_busy
    }
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
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
                2 => preferences.show_preview = !preferences.show_preview,
                _ => preferences.save_sent_copy = !preferences.save_sent_copy,
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_images_default_on_and_explicit_off_survives_roundtrip() {
        assert!(Preferences::default().auto_load_images);
        assert!(Preferences::parse("theme=dark\n").auto_load_images);
        let mut preferences = Preferences::parse("auto_load_images=0\n");
        assert!(!preferences.auto_load_images);
        assert_eq!(Preferences::parse(&preferences.serialize()), preferences);
        preferences.auto_load_images = true;
        assert_eq!(Preferences::parse(&preferences.serialize()), preferences);
    }

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
        assert!(!defaults.show_account_check_details);
        assert!(Preferences::parse("show_log=1\n").show_log);
        assert!(!Preferences::parse("show_log=0\n").show_log);
        assert!(Preferences::parse("show_account_check_details=1\n").show_account_check_details);
        assert_eq!(defaults.reader_font, 16.0);
    }
}
