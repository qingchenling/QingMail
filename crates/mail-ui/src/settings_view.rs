//! Preferences arranged as compact responsive groups in the mailbox host.
use super::*;
use crate::ui::{icon, page_card, quiet, tool};
use gpui_kit::{Render, Window, div, px, relative};
use gpui_kit_material::theme;
use std::time::Duration;

fn setting_row(label: &str, control: impl IntoElement) -> gpui_kit::Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .min_h(px(48.))
        .child(div().flex_1().min_w_0().text_sm().child(label.to_owned()))
        .child(control)
}

fn note(text: impl Into<gpui_kit::SharedString>) -> gpui_kit::Div {
    div()
        .text_sm()
        .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
        .child(text.into())
}

fn described_switch(
    id: &'static str,
    description: &'static str,
    switch: MaterialSwitch,
) -> impl IntoElement {
    MaterialTooltip::plain(format!("{id}-description"), description)
        .show_delay(Duration::from_millis(350))
        .trigger(switch)
}

fn option_group() -> gpui_kit::Div {
    div()
        .flex()
        .gap_1()
        .p_1()
        .rounded_full()
        .bg(theme::color(ColorRole::SURFACE_CONTAINER))
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preferences = prefs(cx);
        let mut themes = option_group();
        for (id, label, symbol, dark) in [
            ("theme-light", "浅色", "light_mode", false),
            ("theme-dark", "深色", "dark_mode", true),
        ] {
            themes = themes.child(
                MaterialButton::new(id)
                    .button_size(ButtonSize::Small)
                    .variant(if preferences.dark == dark {
                        ButtonVariant::Tonal
                    } else {
                        ButtonVariant::Text
                    })
                    .icon(icon(symbol))
                    .child(label)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        let mut p = prefs(cx);
                        p.dark = dark;
                        set(cx, p);
                        cx.notify();
                    })),
            );
        }

        let mut density = option_group();
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
            .child(setting_row("主题", themes))
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
                    .child(div().min_w(px(28.)).text_center().child(format!(
                        "{:.0}",
                        preferences.reader_font
                    )))
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
                "自动加载邮件内图片",
                described_switch(
                    "settings-auto-images",
                    "默认开启。打开邮件时自动加载远程图片。关闭后，可在单封邮件中手动加载。加载远程图片可能让发件人知道你已阅读。",
                    MaterialSwitch::new("settings-auto-images", "自动加载邮件内图片")
                        .checked(preferences.auto_load_images)
                        .on_change(cx.listener(|_, on: &bool, _, cx| {
                            let mut p = prefs(cx);
                            p.auto_load_images = *on;
                            set(cx, p);
                            cx.notify();
                        })),
                ),
            ))
            .child(setting_row(
                "减少动态",
                described_switch(
                    "settings-motion",
                    "关闭页面切换、展开、选中和主题过渡动画。应用也会遵循系统减少动态和省电设置。",
                    MaterialSwitch::new("settings-motion", "减少动态")
                        .checked(preferences.reduce_motion)
                        .on_change(cx.listener(|_, on: &bool, _, cx| {
                            let mut p = prefs(cx);
                            p.reduce_motion = *on;
                            set(cx, p);
                            cx.notify();
                        })),
                ),
            ));

        let mut cache = page_card("邮件缓存");
        if let Some(info) = &self.cache {
            let mib = |value: u64| value as f64 / 1_048_576.;
            cache = cache
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .child(div().text_size(px(25.)).child(format!(
                            "{:.1}",
                            mib(info.evictable_bytes.saturating_add(info.protected_bytes))
                        )))
                        .child(note("MiB 已使用"))
                        .child(div().flex_1())
                        .child(note(format!("上限 {:.0} MiB", mib(info.budget_bytes)))),
                )
                .child(
                    div()
                        .h(px(4.))
                        .rounded_full()
                        .bg(theme::color(ColorRole::SURFACE_CONTAINER))
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(theme::color(ColorRole::PRIMARY))
                                .w(relative(if info.budget_bytes == 0 {
                                    0.
                                } else {
                                    (info.evictable_bytes.saturating_add(info.protected_bytes)
                                        as f32
                                        / info.budget_bytes as f32)
                                        .clamp(0., 1.)
                                })),
                        ),
                )
                .child(note(format!(
                    "可清理 {:.1} MiB · 受保护 {:.1} MiB · 下载中 {:.1} MiB",
                    mib(info.evictable_bytes),
                    mib(info.protected_bytes),
                    mib(info.downloading_bytes)
                )))
                .when(info.protected_bytes > info.budget_bytes, |group| {
                    group.child(note(
                        "受保护内容已超过预算，请提高预算或整理草稿。应用不会自动删除这些内容。",
                    ))
                });
        } else {
            cache = cache.child(note("正在读取缓存用量…"));
        }
        let mut budgets = option_group();
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
                        if self
                            .cache
                            .as_ref()
                            .is_some_and(|info| info.budget_bytes == bytes)
                        {
                            ButtonVariant::Tonal
                        } else {
                            ButtonVariant::Text
                        },
                    )
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cache_command(mail_protocol::Command::SetCacheBudget { bytes }, cx)
                    })),
            );
        }
        cache = cache
            .child(setting_row("缓存上限", budgets))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                    .pt_3()
                    .child(
                        quiet("cache-refresh", "刷新用量")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cache_command(mail_protocol::Command::CacheStatus, cx)
                            })),
                    )
                    .child(
                        MaterialButton::new("cache-clear")
                            .variant(ButtonVariant::Outlined)
                            .disabled(self.busy)
                            .child(if self.confirm_clear {
                                "确认清理可下载缓存"
                            } else {
                                "清理邮件缓存"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.confirm_clear {
                                    this.cache_command(mail_protocol::Command::ClearCache, cx);
                                } else {
                                    this.confirm_clear = true;
                                    cx.notify();
                                }
                            })),
                    )
                    .when(self.confirm_clear, |row| {
                        row.child(quiet("cache-cancel", "取消").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.confirm_clear = false;
                                cx.notify();
                            },
                        )))
                    }),
            )
            .when(!self.status.is_empty(), |group| {
                group.child(note(self.status.clone()))
            });

        let mut sending = page_card("邮件发送");
        if let Some(settings) = &self.desktop {
            let preferences = &settings.preferences;
            sending = sending
                .child(setting_row(
                    "发送后保存到发件箱",
                    described_switch(
                        "save-sent-copy",
                        "默认开启。发送后将副本保存到服务器发件箱。关闭后仍保留本地发送记录，邮箱服务商也可能自行保存副本。",
                        MaterialSwitch::new("save-sent-copy", "发送后保存到发件箱")
                            .checked(preferences.save_sent_copy)
                            .disabled(self.desktop_busy)
                            .on_change(cx.listener(|this, _, _, cx| {
                                this.desktop_command(Some(3), cx)
                            })),
                    ),
                ));
        } else {
            sending = sending.child(note("正在读取发送设置…"));
        }

        let mut desktop = page_card("桌面与启动");
        if let Some(settings) = &self.desktop {
            let preferences = &settings.preferences;
            desktop = desktop
                .child(setting_row(
                    "新邮件通知",
                    described_switch(
                        "desktop-notifications",
                        "收到新邮件时请求桌面通知。是否展示通知由系统通知服务和当前勿扰设置决定。",
                        MaterialSwitch::new("desktop-notifications", "新邮件通知")
                            .checked(preferences.notifications)
                            .disabled(self.desktop_busy)
                            .on_change(cx.listener(|this, _, _, cx| {
                                this.desktop_command(Some(0), cx)
                            })),
                    ),
                ))
                .child(setting_row(
                    "显示发件人和主题",
                    described_switch(
                        "desktop-sender",
                        "通知中显示发件人与主题。关闭后通知只提示收到新邮件。",
                        MaterialSwitch::new("desktop-sender", "显示发件人和主题")
                            .checked(preferences.show_sender_subject)
                            .disabled(self.desktop_busy || !preferences.notifications)
                            .on_change(cx.listener(|this, _, _, cx| {
                                this.desktop_command(Some(1), cx)
                            })),
                    ),
                ))
                .child(setting_row(
                    "显示已缓存正文预览",
                    described_switch(
                        "desktop-preview",
                        "在通知中显示已缓存的正文片段。生成预览不会额外下载邮件，请留意通知中的隐私内容。",
                        MaterialSwitch::new("desktop-preview", "显示已缓存正文预览")
                            .checked(preferences.show_preview)
                            .disabled(self.desktop_busy || !preferences.notifications)
                            .on_change(cx.listener(|this, _, _, cx| {
                                this.desktop_command(Some(2), cx)
                            })),
                    ),
                ));
            if !settings.notification_status.is_empty() {
                desktop = desktop.child(note(settings.notification_status.clone()));
            }
        } else {
            desktop = desktop.child(note("正在读取通知设置…"));
        }
        desktop = desktop.child(
            quiet("desktop-refresh", "刷新通知状态")
                .disabled(self.desktop_busy)
                .on_click(cx.listener(|this, _, _, cx| this.desktop_command(None, cx))),
        );
        if !self.desktop_error.is_empty() {
            desktop = desktop.child(note(self.desktop_error.clone()));
        }

        let mut startup = div().flex().flex_col().gap_2();
        if let Some(settings) = &self.autostart {
            let needs_attention = settings.pending || settings.enabled != settings.desired;
            startup = startup.child(setting_row(
                "登录后启动后台",
                described_switch(
                    "autostart-toggle",
                    "登录桌面后启动后台，但不自动打开主窗口。再次打开 QingMail 可返回主窗口。保存并退出会结束本次后台会话。",
                    MaterialSwitch::new("autostart-toggle", "登录后启动后台")
                        .checked(settings.desired)
                        .disabled(self.autostart_busy)
                        .on_change(cx.listener(|this, on: &bool, _, cx| {
                            this.autostart_command(Some(*on), cx)
                        })),
                ),
            ));
            if needs_attention {
                startup = startup.child(note(format!(
                    "{} · {}",
                    if settings.pending {
                        "启动设置尚未完成"
                    } else {
                        "启动项状态与当前选择不一致"
                    },
                    settings.status
                )));
                let desired = settings.desired;
                startup = startup.child(
                    quiet("autostart-retry", "重新应用当前选择")
                        .disabled(self.autostart_busy)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.autostart_command(Some(desired), cx)
                        })),
                );
            }
        } else {
            startup = startup.child(note("正在读取启动状态…"));
        }
        desktop = desktop.child(startup).child(
            quiet("autostart-refresh", "刷新启动状态")
                .disabled(self.autostart_busy)
                .on_click(cx.listener(|this, _, _, cx| this.autostart_command(None, cx))),
        );

        let diagnostics = page_card("诊断与关于");
        let diagnostics = diagnostics
            .child(setting_row(
                "主窗口诊断日志",
                described_switch(
                    "settings-log",
                    "默认关闭。开启后在主窗口左下角显示同步和诊断信息，方便排查问题。",
                    MaterialSwitch::new("settings-log", "主窗口诊断日志")
                        .checked(preferences.show_log)
                        .on_change(cx.listener(|_, on: &bool, _, cx| {
                            let mut p = prefs(cx);
                            p.show_log = *on;
                            set(cx, p);
                            cx.notify();
                        })),
                ),
            ))
            .child(setting_row(
                "邮箱连接检查详情",
                described_switch(
                    "settings-account-check-details",
                    "默认关闭。开启后显示账号页中的服务器检查结果、密钥库状态和文件夹清单。",
                    MaterialSwitch::new("settings-account-check-details", "邮箱连接检查详情")
                        .checked(preferences.show_account_check_details)
                        .on_change(cx.listener(|_, on: &bool, _, cx| {
                            let mut p = prefs(cx);
                            p.show_account_check_details = *on;
                            set(cx, p);
                            cx.notify();
                        })),
                ),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                    .pt_3()
                    .child(note("QingMail"))
                    .child(note(concat!("版本 ", env!("CARGO_PKG_VERSION")))),
            );

        let wide = window.bounds().size.width.as_f32() >= 1120.;
        div()
            .id("settings-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .gap_3()
                    .child(div().text_size(px(26.)).child("设置"))
                    .when_some(
                        self.status.is_empty().then_some("偏好设置"),
                        |row, status| row.child(note(status)),
                    ),
            )
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
                            .child(sending)
                            .child(desktop)
                            .child(diagnostics),
                    ),
            )
    }
}
