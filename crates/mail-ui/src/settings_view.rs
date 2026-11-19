//! Preferences arranged as responsive cards inside the mailbox host.
use super::*;
use crate::ui::{page_card, quiet, tool};
use gpui_kit::{Render, Window, div, px};
use gpui_kit_material::theme;

fn setting_row(label: &str, control: impl IntoElement) -> gpui_kit::Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .min_h(px(40.))
        .child(div().flex_1().text_sm().child(label.to_owned()))
        .child(control)
}
fn note(text: impl Into<gpui_kit::SharedString>) -> gpui_kit::Div {
    div()
        .text_sm()
        .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
        .child(text.into())
}
impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preferences = prefs(cx);
        let mut themes = div().flex().gap_3();
        for (id, label, dark) in [("theme-light", "浅色", false), ("theme-dark", "深色", true)]
        {
            let preview = div()
                .w_full()
                .h(px(58.))
                .p_2()
                .rounded(px(10.))
                .flex()
                .gap_2()
                .bg(gpui_kit::rgb(if dark { 0x202026 } else { 0xf5f1fa }))
                .child(
                    div()
                        .w(px(24.))
                        .h_full()
                        .rounded(px(5.))
                        .bg(gpui_kit::rgb(if dark { 0x4c445d } else { 0xe0d5f2 })),
                )
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .rounded(px(5.))
                        .bg(gpui_kit::rgb(if dark { 0x36333e } else { 0xffffff })),
                );
            themes = themes.child(
                div().flex_1().min_w_0().child(
                    MaterialButton::new(id)
                        .w_full()
                        .h(px(106.))
                        .variant(if preferences.dark == dark {
                            ButtonVariant::Tonal
                        } else {
                            ButtonVariant::Outlined
                        })
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(preview)
                                .child(label),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let mut p = prefs(cx);
                            p.dark = dark;
                            set(cx, p);
                            cx.notify();
                        })),
                ),
            );
        }
        let mut density = div().flex().gap_1();
        for (id, label, compact) in [
            ("density-comfortable", "舒适", false),
            ("density-compact", "紧凑", true),
        ] {
            density = density.child(
                MaterialButton::new(id)
                    .button_size(ButtonSize::Small)
                    .variant(if preferences.compact == compact {
                        ButtonVariant::Tonal
                    } else {
                        ButtonVariant::Text
                    })
                    .child(label)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        let mut p = prefs(cx);
                        p.compact = compact;
                        set(cx, p);
                        cx.notify();
                    })),
            );
        }
        let appearance = page_card("外观与阅读")
            .child(themes)
            .child(setting_row(
                "阅读字号",
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        tool("settings-font-smaller", "remove", "减小字号")
                            .disabled(preferences.reader_font <= 12.)
                            .on_click(cx.listener(|_, _, _, cx| {
                                let mut p = prefs(cx);
                                p.reader_font = (p.reader_font - 2.).max(12.);
                                set(cx, p);
                                cx.notify();
                            })),
                    )
                    .child(format!("{:.0}", preferences.reader_font))
                    .child(
                        tool("settings-font-larger", "add", "增大字号")
                            .disabled(preferences.reader_font >= 28.)
                            .on_click(cx.listener(|_, _, _, cx| {
                                let mut p = prefs(cx);
                                p.reader_font = (p.reader_font + 2.).min(28.);
                                set(cx, p);
                                cx.notify();
                            })),
                    ),
            ))
            .child(setting_row("列表密度", density))
            .child(setting_row(
                "减少动态",
                MaterialSwitch::new("settings-motion", "减少动态")
                    .checked(preferences.reduce_motion)
                    .on_change(cx.listener(|_, on: &bool, _, cx| {
                        let mut p = prefs(cx);
                        p.reduce_motion = *on;
                        set(cx, p);
                        cx.notify();
                    })),
            ))
            .child(note(
                "减少动态会关闭页面、展开、选中与主题过渡；同时遵循系统减少动态和省电设置。",
            ));
        let mut desktop = page_card("桌面与启动");
        if let Some(settings) = &self.desktop {
            let p = &settings.preferences;
            for (id, label, on, change) in [
                ("desktop-notifications", "新邮件通知", p.notifications, 0),
                (
                    "desktop-sender",
                    "显示发件人和主题",
                    p.show_sender_subject,
                    1,
                ),
                ("desktop-preview", "显示已缓存正文预览", p.show_preview, 2),
            ] {
                desktop = desktop.child(setting_row(
                    label,
                    MaterialSwitch::new(id, label)
                        .checked(on)
                        .disabled(self.desktop_busy)
                        .on_change(cx.listener(move |this, _, _, cx| {
                            this.desktop_command(Some(change), cx)
                        })),
                ));
            }
            desktop = desktop.child(note(settings.notification_status.clone()));
        } else {
            desktop = desktop.child(note("正在读取通知设置…"));
        }
        desktop = desktop
            .child(note("预览不会触发下载；锁屏与勿扰展示由系统决定。"))
            .child(
                quiet("desktop-refresh", "刷新通知状态")
                    .disabled(self.desktop_busy)
                    .on_click(cx.listener(|this, _, _, cx| this.desktop_command(None, cx))),
            )
            .when(!self.desktop_error.is_empty(), |card| {
                card.child(note(self.desktop_error.clone()))
            });
        if let Some(settings) = &self.autostart {
            let desired = settings.desired;
            desktop = desktop
                .child(setting_row(
                    "登录后启动后台",
                    MaterialSwitch::new("autostart-toggle", "登录后启动后台")
                        .checked(desired)
                        .disabled(self.autostart_busy)
                        .on_change(cx.listener(|this, on: &bool, _, cx| {
                            this.autostart_command(Some(*on), cx)
                        })),
                ))
                .child(note(format!(
                    "{}{} · {}",
                    if settings.enabled {
                        "已启用"
                    } else {
                        "未启用"
                    },
                    if settings.pending {
                        " · 设置尚未完成"
                    } else {
                        ""
                    },
                    settings.status
                )))
                .child(
                    quiet("autostart-retry", "重新应用当前选择")
                        .disabled(self.autostart_busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.autostart_command(Some(desired), cx)
                        })),
                );
        }
        desktop = desktop
            .child(
                quiet("autostart-refresh", "刷新启动状态")
                    .disabled(self.autostart_busy)
                    .on_click(cx.listener(|this, _, _, cx| this.autostart_command(None, cx))),
            )
            .when(!self.autostart_error.is_empty(), |card| {
                card.child(note(self.autostart_error.clone()))
            })
            .child(note(
                "自启动只打开后台。再次打开 QingMail 返回主窗口；保存并退出会结束本次后台会话。",
            ));
        let mut cache = page_card("邮件缓存");
        if let Some(info) = &self.cache {
            let mib = |v: u64| v as f64 / 1048576.;
            cache = cache
                .child(div().text_size(px(24.)).child(format!(
                    "{:.1} MiB",
                    mib(info.evictable_bytes.saturating_add(info.protected_bytes))
                )))
                .child(note(format!(
                    "预算 {:.0} MiB · 可清理 {:.1} MiB · 受保护 {:.1} MiB · 下载中 {:.1} MiB",
                    mib(info.budget_bytes),
                    mib(info.evictable_bytes),
                    mib(info.protected_bytes),
                    mib(info.downloading_bytes)
                )))
                .when(info.protected_bytes > info.budget_bytes, |card| {
                    card.child(note(
                        "受保护内容已超过预算；请提高预算或整理草稿，应用不会自动删除这些内容。",
                    ))
                });
        }
        let mut budgets = div().flex().flex_wrap().gap_2();
        for (id, label, bytes) in [
            ("cache-256", "256 MiB", 256 * 1024 * 1024),
            ("cache-1024", "1 GiB", 1024 * 1024 * 1024),
            ("cache-4096", "4 GiB", 4 * 1024 * 1024 * 1024),
        ] {
            budgets = budgets.child(
                MaterialButton::new(id)
                    .button_size(ButtonSize::Small)
                    .disabled(self.busy)
                    .variant(
                        if self.cache.as_ref().is_some_and(|c| c.budget_bytes == bytes) {
                            ButtonVariant::Tonal
                        } else {
                            ButtonVariant::Outlined
                        },
                    )
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cache_command(mail_protocol::Command::SetCacheBudget { bytes }, cx)
                    })),
            );
        }
        cache=cache.child(budgets).child(note("只清理可重新下载的正文和附件，保留索引、草稿、发送快照、冲突原件与未完成操作。浏览器副本在阅读器中单独清理。"))
            .child(div().flex().flex_wrap().gap_2()
                .child(quiet("cache-refresh","刷新用量").disabled(self.busy).on_click(cx.listener(|this,_,_,cx|this.cache_command(mail_protocol::Command::CacheStatus,cx))))
                .child(MaterialButton::new("cache-clear").variant(ButtonVariant::Outlined).disabled(self.busy)
                    .child(if self.confirm_clear{"确认清理可下载缓存"}else{"清理邮件缓存"})
                    .on_click(cx.listener(|this,_,_,cx|{if this.confirm_clear {this.cache_command(mail_protocol::Command::ClearCache,cx);}else{this.confirm_clear=true;cx.notify();}})))
                .when(self.confirm_clear, |row|row.child(quiet("cache-cancel","取消").on_click(cx.listener(|this,_,_,cx|{this.confirm_clear=false;cx.notify();})))))
            .when(!self.status.is_empty(),|card|card.child(note(self.status.clone())));
        let diagnostics = page_card("诊断与关于")
            .child(setting_row(
                "主窗口诊断日志",
                MaterialSwitch::new("settings-log", "主窗口诊断日志")
                    .checked(preferences.show_log)
                    .on_change(cx.listener(|_, on: &bool, _, cx| {
                        let mut p = prefs(cx);
                        p.show_log = *on;
                        set(cx, p);
                        cx.notify();
                    })),
            ))
            .child(note("默认关闭；开启后在左下角显示同步状态。"))
            .child(setting_row(
                "邮箱连接检查详情",
                MaterialSwitch::new("settings-account-check-details", "连接检查详情")
                    .checked(preferences.show_account_check_details)
                    .on_change(cx.listener(|_, on: &bool, _, cx| {
                        let mut p = prefs(cx);
                        p.show_account_check_details = *on;
                        set(cx, p);
                        cx.notify();
                    })),
            ))
            .child(note(
                "默认隐藏账号页中的服务器检查结果、密钥库状态和文件夹清单。",
            ))
            .child(note(concat!("QingMail ", env!("CARGO_PKG_VERSION"))));
        let wide = window.bounds().size.width.as_f32() >= 1120.;
        div()
            .id("settings-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_5()
            .child(div().text_size(px(26.)).child("设置"))
            .child(note("按你的习惯调整外观、桌面行为与邮件缓存"))
            .child(
                div()
                    .flex()
                    .when(!wide, |row| row.flex_col())
                    .gap_4()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(appearance)
                            .child(cache),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(desktop)
                            .child(diagnostics),
                    ),
            )
    }
}
