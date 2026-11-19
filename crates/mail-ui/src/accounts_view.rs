//! Embedded account page; authentication and persistence remain in Accounts.
use super::*;
use crate::ui::{icon, motion, tool};
use gpui_kit::{Render, div, px};
use gpui_kit_material::{prelude::*, theme};

impl Accounts {
    fn new_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.pending.is_some() {
            return;
        }
        self.editing = None;
        self.authentication = Authentication::Password;
        self.form_initialized = true;
        self.checked = None;
        self.impact = None;
        self.folders.clear();
        self.security = [ConnectionSecurity::Tls; 2];
        for (i, input) in self.inputs.iter().enumerate() {
            input.update(cx, |input, cx| {
                input.set_value(
                    match i {
                        3 => "993",
                        7 => "465",
                        _ => "",
                    },
                    window,
                    cx,
                )
            });
        }
        self.imap = "尚未检查".into();
        self.smtp = "尚未检查".into();
        self.status = "填写邮箱服务商提供的 IMAP / SMTP 配置".into();
        cx.notify();
    }
    fn provider_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut choices = div().flex().flex_wrap().gap_2();
        for (id, label, authentication) in [
            ("generic", "通用邮箱", Authentication::Password),
            (
                "outlook",
                "Outlook / Hotmail",
                Authentication::MicrosoftPersonal,
            ),
            (
                "microsoft365",
                "Microsoft 365 工作或学校",
                Authentication::Microsoft365,
            ),
        ] {
            choices = choices.child(
                MaterialButton::new(id)
                    .variant(if self.authentication == authentication {
                        ButtonVariant::Tonal
                    } else {
                        ButtonVariant::Outlined
                    })
                    .disabled(self.busy || self.pending.is_some())
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.authentication = authentication;
                        this.checked = None;
                        this.folders.clear();
                        this.imap = "尚未检查".into();
                        this.smtp = "尚未检查".into();
                        for i in [5, 9] {
                            this.inputs[i].update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        this.status = if authentication == Authentication::Password {
                            "填写邮箱服务商提供的 IMAP / SMTP 配置"
                        } else {
                            "填写邮箱，然后在浏览器登录"
                        }
                        .into();
                        cx.notify();
                    })),
            );
        }
        div().flex().flex_col().gap_3().child(choices)
            .when(self.authentication != Authentication::Password, |panel| panel
                .child(div().text_sm().text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child("使用系统浏览器完成微软授权，无需填写密码。"))
                .child(MaterialButton::new("microsoft-login-help").variant(ButtonVariant::Text)
                    .child("查看微软邮箱帮助")
                    .on_click(cx.listener(|_,_,_,cx| cx.open_url("https://support.microsoft.com/zh-cn/outlook/pop-imap-and-smtp-settings-for-outlook-com")))))
    }
    fn field(&self, i: usize) -> TextField {
        let field = TextField::new(("account-input", i), &self.inputs[i])
            .outlined()
            .density(TextFieldDensity::Compact)
            .w_full()
            .label(match i {
                2 | 6 => "服务器",
                3 | 7 => "端口",
                4 | 8 => "用户名",
                5 | 9 => "密码 / 应用专用密码",
                _ => LABELS[i],
            })
            .disabled(self.busy || self.pending.is_some());
        if i == 5 || i == 9 {
            field.input_type(TextFieldInputType::Password)
        } else {
            field
        }
    }
    fn server_card(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let offset = 2 + index * 4;
        let mut security = div()
            .flex()
            .p_1()
            .rounded(px(12.))
            .bg(theme::color(ColorRole::SURFACE_CONTAINER_HIGH));
        for (id, label, value) in [
            ("tls", "TLS", ConnectionSecurity::Tls),
            ("starttls", "STARTTLS", ConnectionSecurity::StartTls),
        ] {
            security = security.child(
                MaterialButton::new((id, index))
                    .flex_1()
                    .shape(ButtonShape::Square)
                    .button_size(ButtonSize::Small)
                    .variant(if self.security[index] == value {
                        ButtonVariant::Tonal
                    } else {
                        ButtonVariant::Text
                    })
                    .disabled(self.busy || self.pending.is_some())
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.security[index] == value {
                            return;
                        }
                        this.security[index] = value;
                        this.checked = None;
                        this.imap = "待重新检查".into();
                        this.smtp = "待重新检查".into();
                        this.folders.clear();
                        this.status = "加密方式已修改，请重新输入密码并检查连接".into();
                        cx.notify();
                    })),
            );
        }
        let result = if index == 0 { &self.imap } else { &self.smtp };
        let passed = result.starts_with("连接与认证通过");
        let unchecked = matches!(result.as_str(), "尚未检查" | "待重新检查");
        div()
            .w_full()
            .min_w_0()
            .p_4()
            .flex()
            .flex_col()
            .gap_4()
            .rounded(px(16.))
            .border_1()
            .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon(if index == 0 { "mail" } else { "send" }))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(17.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(if index == 0 {
                                "收信 · IMAP"
                            } else {
                                "发信 · SMTP"
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(theme::color(if passed {
                                ColorRole::PRIMARY
                            } else if unchecked {
                                ColorRole::ON_SURFACE_VARIANT
                            } else {
                                ColorRole::ERROR
                            }))
                            .child(
                                icon(if passed {
                                    "check_circle"
                                } else if unchecked {
                                    "info"
                                } else {
                                    "error"
                                })
                                .size(px(16.)),
                            )
                            .child(if passed {
                                "检查通过"
                            } else if unchecked {
                                "待检查"
                            } else {
                                "连接异常"
                            }),
                    ),
            )
            .child(security)
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().flex_1().min_w_0().child(self.field(offset)))
                    .child(div().w(px(110.)).child(self.field(offset + 1))),
            )
            .child(self.field(offset + 2))
            .child(self.field(offset + 3))
    }
}
impl Render for Accounts {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.loaded && !self.form_initialized && !self.busy && self.pending.is_none() {
            self.form_initialized = true;
            if let Some((id, settings)) = self.saved.first().cloned() {
                self.begin_edit(id, &settings, window, cx);
            }
        }
        let wide = window.bounds().size.width.as_f32() >= 1120.;
        let mut cards = div().flex().flex_wrap().gap_3();
        for (index, (id, settings)) in self.saved.iter().enumerate() {
            let selected = self.editing.as_ref() == Some(id);
            let avatar_bg = motion::color(
                format!("account-avatar-{:?}", id),
                if selected {
                    ColorRole::PRIMARY
                } else {
                    ColorRole::SECONDARY_CONTAINER
                },
                window,
                cx,
            );
            let avatar_text = motion::color(
                format!("account-avatar-text-{:?}", id),
                if selected {
                    ColorRole::ON_PRIMARY
                } else {
                    ColorRole::ON_SECONDARY_CONTAINER
                },
                window,
                cx,
            );
            let edit_id = id.clone();
            let edit_settings = settings.clone();
            cards = cards.child(
                div()
                    .w(px(if wide {
                        (window.bounds().size.width.as_f32() - 216. - 48. - 12.) / 2.
                    } else {
                        (window.bounds().size.width.as_f32() - 216. - 48.).max(240.)
                    }))
                    .min_w_0()
                    .child(
                        MaterialButton::new(("account", index))
                            .w_full()
                            .h(px(88.))
                            .shape(ButtonShape::Square)
                            .tone(if selected {
                                ButtonTone::Accent
                            } else {
                                ButtonTone::Neutral
                            })
                            .variant(if selected {
                                ButtonVariant::Outlined
                            } else {
                                ButtonVariant::Tonal
                            })
                            .disabled(self.busy || self.pending.is_some())
                            .child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .w(px(44.))
                                            .h(px(44.))
                                            .flex_none()
                                            .rounded_full()
                                            .bg(avatar_bg)
                                            .text_color(avatar_text)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(
                                                settings
                                                    .display_name
                                                    .chars()
                                                    .next()
                                                    .unwrap_or('?')
                                                    .to_string(),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .items_start()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .w_full()
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .font_weight(
                                                                gpui_kit::FontWeight::SEMIBOLD,
                                                            )
                                                            .truncate()
                                                            .child(settings.display_name.clone()),
                                                    )
                                                    .when(
                                                        self.default_account.as_ref() == Some(id),
                                                        |row| {
                                                            row.child(
                                                            div()
                                                                .px_2()
                                                                .py_1()
                                                                .rounded(px(6.))
                                                                .bg(theme::color(
                                                                    ColorRole::PRIMARY_CONTAINER,
                                                                ))
                                                                .text_color(theme::color(
                                                                    ColorRole::ON_PRIMARY_CONTAINER,
                                                                ))
                                                                .text_xs()
                                                                .child("默认发件"),
                                                        )
                                                        },
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .w_full()
                                                    .text_sm()
                                                    .text_color(theme::color(
                                                        ColorRole::ON_SURFACE_VARIANT,
                                                    ))
                                                    .truncate()
                                                    .child(settings.address.clone()),
                                            )
                                            .child(
                                                div()
                                                    .w_full()
                                                    .text_xs()
                                                    .text_color(theme::color(
                                                        ColorRole::ON_SURFACE_VARIANT,
                                                    ))
                                                    .truncate()
                                                    .child(if self.paused_accounts.contains(id) {
                                                        "同步已暂停".to_owned()
                                                    } else {
                                                        crate::lifecycle::account_sync_status(
                                                            id, cx,
                                                        )
                                                    }),
                                            ),
                                    ),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.begin_edit(edit_id.clone(), &edit_settings, window, cx)
                            })),
                    ),
            );
        }
        let mut form = div()
            .id("account-form")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_size(px(26.)).child("邮箱账号"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                                    .child("管理你的邮箱与收发配置"),
                            ),
                    )
                    .child(
                        MaterialButton::new("add-account")
                            .variant(ButtonVariant::Filled)
                            .icon(icon("add"))
                            .child("添加邮箱")
                            .disabled(self.busy || self.pending.is_some())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_account(window, cx)),
                            ),
                    )
                    .child(
                        tool("refresh-accounts", "refresh", "刷新账号列表")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(cards);
        let selected_name = self.editing.as_ref().and_then(|id| {
            self.saved
                .iter()
                .find(|(saved_id, _)| saved_id == id)
                .map(|(_, settings)| settings.display_name.as_str())
        });
        let mut heading = div().flex().items_center().flex_wrap().gap_3().child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(22.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(selected_name.unwrap_or("添加邮箱账号").to_owned()),
        );
        if let Some(id) = &self.editing {
            let is_default = self.default_account.as_ref() == Some(id);
            let paused = self.paused_accounts.contains(id);
            let default_id = id.clone();
            let pause_id = id.clone();
            heading = heading
                .child(
                    MaterialButton::new("default-account")
                        .variant(ButtonVariant::Outlined)
                        .disabled(self.busy || self.pending.is_some() || is_default)
                        .child(if is_default {
                            "默认发件账号"
                        } else {
                            "设为默认"
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.change_option(
                                Command::SetDefaultAccount {
                                    account: default_id.clone(),
                                },
                                cx,
                            )
                        })),
                )
                .child(
                    MaterialButton::new("pause-account")
                        .variant(ButtonVariant::Outlined)
                        .icon(icon(if paused { "play_arrow" } else { "pause" }))
                        .child(if paused {
                            "恢复同步"
                        } else {
                            "暂停同步"
                        })
                        .disabled(self.busy || self.pending.is_some())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.change_option(
                                Command::PauseSync {
                                    account: Some(pause_id.clone()),
                                    paused: !paused,
                                },
                                cx,
                            )
                        })),
                );
        }
        let mut actions = div().flex().items_center().flex_wrap().gap_2();
        if let Some(id) = &self.editing {
            let remove_id = id.clone();
            actions = actions.child(
                MaterialButton::new("remove-account")
                    .variant(ButtonVariant::Outlined)
                    .child(
                        div()
                            .text_color(theme::color(ColorRole::ERROR))
                            .child("移除账号"),
                    )
                    .disabled(self.busy || self.pending.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.request_remove(remove_id.clone(), cx)
                    })),
            );
        }
        if let Some(cancel) = self.oauth_cancel.clone() {
            actions = actions.child(
                MaterialButton::new("cancel-microsoft-login")
                    .variant(ButtonVariant::Outlined)
                    .child("取消登录")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cancel.store(true, std::sync::atomic::Ordering::Release);
                        this.status = "正在取消登录，未保存账号…".into();
                        cx.notify();
                    })),
            );
        }
        actions = actions
            .child(div().flex_1())
            .child(
                tool("check-vault", "key", "检查系统密钥库")
                    .disabled(self.busy || self.pending.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.vault_status(cx))),
            )
            .child(
                MaterialButton::new("check-account")
                    .variant(ButtonVariant::Outlined)
                    .disabled(self.busy || self.pending.is_some())
                    .child(if self.authentication == Authentication::Password {
                        "检查连接"
                    } else {
                        "在浏览器登录并检查"
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.check(window, cx))),
            )
            .child(
                MaterialButton::new("save-account")
                    .disabled(!self.can_save(cx))
                    .child(if self.editing.is_some() {
                        "保存更改"
                    } else {
                        "保存账号"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            );
        let details = div()
            .w_full()
            .min_w_0()
            .p_5()
            .rounded(px(20.))
            .border_1()
            .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
            .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
            .flex()
            .flex_col()
            .gap_5()
            .child(heading)
            .child(self.provider_controls(cx))
            .child(
                div()
                    .flex()
                    .when(!wide, |row| row.flex_col())
                    .gap_4()
                    .child(div().flex_1().min_w_0().child(self.field(0)))
                    .child(div().flex_1().min_w_0().child(self.field(1))),
            )
            .when(self.authentication == Authentication::Password, |panel| panel.child(
                div()
                    .flex()
                    .when(!wide, |row| row.flex_col())
                    .gap_4()
                    .child(div().flex_1().min_w_0().child(self.server_card(0, cx)))
                    .child(div().flex_1().min_w_0().child(self.server_card(1, cx))),
            ))
            .when(self.authentication != Authentication::Password, |panel| panel.child(
                div().p_4().rounded(px(16.)).bg(theme::color(ColorRole::SURFACE_CONTAINER_HIGH))
                    .flex().flex_col().gap_2()
                    .child("收信：outlook.office365.com · 993 · TLS · OAuth")
                    .child(if self.authentication == Authentication::MicrosoftPersonal { "发信：smtp-mail.outlook.com · 587 · STARTTLS · OAuth" } else { "发信：smtp.office365.com · 587 · STARTTLS · OAuth" })
                    .child("Outlook 网页设置需开启 IMAP；工作或学校账号还需管理员允许 IMAP 与 SMTP AUTH。")))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child(icon("lock").size(px(16.)))
                    .child("密码或授权令牌仅保存在系统密钥库 · 连接检查不会发送邮件"),
            )
            .child(actions);
        form = form.child(motion::enter(format!("account-form-{:?}", self.editing), details, cx))
            .when(self.pending.is_some(), |form| form.child(
                div().flex().flex_col().gap_2()
                .child("请先刷新并核对账号状态；结束查询不会取消或重新执行原请求。")
                .child(MaterialButton::new("query-account-save")
                    .child("查询保存结果")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.save_result(None, cx))))
                .child(MaterialButton::new("finish-account-query")
                    .child("已核对账号，结束查询")
                    .disabled(self.busy || crate::lifecycle::account_backend(cx).saving())
                    .on_click(cx.listener(|this, _, _, cx| this.finish_query(cx))))
            ))
            .when(crate::settings::prefs(cx).show_account_check_details, |form| {
                form.child(
                    div()
                        .p_4()
                        .rounded(px(12.))
                        .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(format!("IMAP：{}", self.imap))
                        .child(format!("SMTP：{}", self.smtp))
                        .child(format!("系统密钥库：{}", self.vault))
                        .when(!self.folders.is_empty(), |panel| {
                            let mut folders = panel.child("本次检查发现的文件夹（只读列表，尚未同步）");
                            for folder in &self.folders {
                                folders = folders.child(format!(
                                    "{}{}",
                                    folder.display_name,
                                    if folder.selectable { "" } else { "（不可选）" }
                                ));
                            }
                            folders
                        }),
                )
            })
            .when(self.impact.is_some(), |form| {
                let (_, impact) = self.impact.as_ref().expect("impact");
                form.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_2()
                        .child(format!(
                            "本地移除影响：缓存 {} 封 · 草稿 {} 份 · 发送任务 {} 条（待处理 {}）· 本地对象 {} 字节",
                            impact.cached_messages,
                            impact.drafts,
                            impact.tasks,
                            impact.pending_tasks,
                            impact.object_bytes
                        ))
                        .child("只删除本地账号与上述数据；不删除远端邮件。密钥库条目会一并尝试删除。")
                        .when(impact.pending_tasks > 0, |panel| {
                            panel.child(
                                div()
                                    .text_color(theme::color(ColorRole::ERROR))
                                    .child(format!(
                                        "有 {} 条待处理发送任务；移除会删除其记录，请先确认已无在途发送",
                                        impact.pending_tasks
                                    )),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    MaterialButton::new("confirm-remove-account")
                                        .child("确认移除")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_remove(cx);
                                        })),
                                )
                                .child(
                                    MaterialButton::new("cancel-remove-account")
                                        .child("取消")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.impact = None;
                                            this.status = "已取消移除".into();
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
            })
            .child(div().text_sm().text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                .child(self.status.clone()));
        form
    }
}
