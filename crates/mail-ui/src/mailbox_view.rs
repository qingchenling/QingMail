//! 三栏邮箱：侧栏保持直角，列表和正文各有固定工具栏；业务操作复用 Mailbox。
use super::*;
use crate::ui::{folder_label, icon, motion, quiet, tool};
use gpui_kit::{
    ClickEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Render, div, uniform_list,
};
use gpui_kit_material::{prelude::*, theme};
use motion::MotionToken;

impl Mailbox {
    fn sent_confirmation_view(
        &self,
        confirmation: &super::SentConfirmation,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = confirmation.details.as_ref().map_or_else(
            || "邮件已发送".to_owned(),
            |task| match task.state {
                mail_core::SendState::CopyPending => "邮件已发送，发件箱副本待保存".into(),
                _ => "邮件已发送".into(),
            },
        );
        let recipient = confirmation
            .details
            .as_ref()
            .map(|task| task.recipients.to.join("、"))
            .unwrap_or_else(|| "正在读取…".into());
        let subject = confirmation
            .details
            .as_ref()
            .map(|task| {
                if task.recipients.subject.is_empty() {
                    "（无主题）".to_owned()
                } else {
                    task.recipients.subject.clone()
                }
            })
            .unwrap_or_else(|| "正在读取…".into());
        let state = confirmation
            .details
            .as_ref()
            .map(crate::send_labels::state_label)
            .unwrap_or_else(|| "正在读取发送记录…".into());
        let submitted_at = confirmation.details.as_ref().map_or_else(
            || "刚刚".into(),
            |task| mail_service::sync::display_time(task.created_at),
        );
        let body = confirmation.body_preview.clone().unwrap_or_else(|| {
            confirmation
                .error
                .clone()
                .unwrap_or_else(|| "正在读取邮件内容…".into())
        });
        let account = confirmation.account.clone();
        let task = confirmation.task.clone();
        let surface = theme::color(ColorRole::SURFACE);
        let outline = theme::color(ColorRole::OUTLINE_VARIANT);
        let success = theme::color(ColorRole::PRIMARY);
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .p_4()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
            .child(
                div()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(outline)
                    .bg(surface)
                    .child(
                        div()
                            .h(px(52.))
                            .flex_none()
                            .px_6()
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(outline)
                            .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
                            .text_size(px(12.))
                            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                            .child("发送结果"),
                    )
                    .child(
                        div()
                            .id("sent-confirmation-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_6()
                            .py_6()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .size(px(48.))
                                            .flex_none()
                                            .rounded_full()
                                            .bg(success)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(24.))
                                            .text_color(theme::color(ColorRole::ON_PRIMARY))
                                            .child(icon("check")),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(24.))
                                            .font_weight(gpui_kit::FontWeight::BOLD)
                                            .text_color(theme::color(ColorRole::ON_SURFACE))
                                            .child(title),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .mt_3()
                                    .flex()
                                    .flex_wrap()
                                    .gap_2()
                                    .items_center()
                                    .border_b_1()
                                    .border_color(outline)
                                    .pb_2()
                                    .child(
                                        quiet("sent-recall", "撤回邮件").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                if let Some(confirmation) = &mut this.sent_confirmation {
                                                    confirmation.notice = Some("普通 SMTP 邮件提交后无法保证撤回；如需撤回，请联系收件人处理。".into());
                                                }
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        quiet("sent-show-status", "显示发送状态").on_click(
                                            cx.listener(|this, _, _, cx| {
                                                if let Some(confirmation) = &mut this.sent_confirmation {
                                                    confirmation.status_open = !confirmation.status_open;
                                                }
                                                this.refresh_sync_status(cx);
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        quiet("sent-open-message", "查看已发送邮件").disabled(self.busy).on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.open_local_sent_record(account.clone(), task.clone(), cx);
                                            }),
                                        ),
                                    ),
                            )
                            .when_some(confirmation.notice.clone(), |panel, notice| {
                                panel.child(
                                    div()
                                        .flex_none()
                                        .mt_3()
                                        .rounded(px(8.))
                                        .bg(theme::color(ColorRole::ERROR_CONTAINER))
                                        .px_3()
                                        .py_2()
                                        .text_size(px(12.))
                                        .child(notice),
                                )
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .mt_5()
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .child(sent_detail_row("收件人", recipient))
                                    .child(sent_detail_row("主题", subject))
                                    .child(sent_detail_row("提交时间", submitted_at))
                                    .when(confirmation.status_open, |details| {
                                        details.child(sent_detail_row("发送状态", state))
                                    }),
                            )
                            .child(div().flex_none().my_4().h(px(1.)).bg(outline))
                            .child(
                                div()
                                    .id("sent-confirmation-preview-scroll")
                                    .min_h(px(144.))
                                    .max_h(px(300.))
                                    .flex_none()
                                    .overflow_y_scroll()
                                    .rounded(px(8.))
                                    .border_1()
                                    .border_color(outline)
                                    .px_4()
                                    .py_3()
                                    .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
                                    .child(
                                        div()
                                            .mb_3()
                                            .text_size(px(12.))
                                            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                                            .child("邮件内容"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .text_color(theme::color(ColorRole::ON_SURFACE))
                                            .child(if body.is_empty() { "（正文为空）".into() } else { body }),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .mt_5()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        MaterialButton::new("sent-return-inbox")
                                            .child("返回邮箱")
                                            .disabled(self.busy)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.return_to_inbox_after_send(cx);
                                            })),
                                    )
                                    .child(
                                        MaterialButton::new("sent-continue-writing")
                                            .variant(ButtonVariant::Outlined)
                                            .child("继续写信")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.continue_writing_after_send(window, cx);
                                            })),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                                            .child(confirmation.details.as_ref().map_or_else(
                                                || "发送记录已保存到本地".to_owned(),
                                                |task| {
                                                    if task.copy.is_some() {
                                                        "邮件已保存在“已发送”".to_owned()
                                                    } else if task.state == mail_core::SendState::CopyPending {
                                                        "发件箱副本等待保存；请勿重复发送".to_owned()
                                                    } else {
                                                        "本地发送记录已保存".to_owned()
                                                    }
                                                },
                                            )),
                                    ),
                            )
                    ),
            )
    }
    fn sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut folders = div()
            .id("folder-tree")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        if self.accounts.len() > 1 {
            folders = folders.child(
                quiet("unified-inbox", "统一收件箱")
                    .icon(icon("all_inbox"))
                    .w_full()
                    .rounded_none()
                    .on_click(cx.listener(|this, _, _, cx| this.select_unified(cx))),
            );
        }
        for (account_index, (account_id, settings)) in self.accounts.iter().enumerate() {
            let expanded = !self.closed_accounts.contains(&account_id.0);
            let account_key = account_id.0.clone();
            let account_folders = self
                .catalogs
                .get(&account_id.0)
                .map(|(folders, _)| folders.as_slice())
                .unwrap_or(&[]);
            let angle = motion::value(
                format!("account-chevron-{:?}", account_id),
                if expanded {
                    std::f32::consts::FRAC_PI_2
                } else {
                    0.
                },
                MotionToken::FastSpatial,
                window,
                cx,
            );
            folders = folders.child(
                MaterialButton::new(("account-tree-root", account_index))
                    .variant(ButtonVariant::Text)
                    .tone(ButtonTone::Neutral)
                    .shape(ButtonShape::Square)
                    .rounded_none()
                    .w_full()
                    .h(px(90.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .items_start()
                            .child(
                                div()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(settings.display_name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                                    .truncate()
                                    .child(settings.address.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .truncate()
                                    .child(crate::lifecycle::account_sync_status(account_id, cx)),
                            ),
                    )
                    .child(icon("chevron_right").rotate(angle))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.closed_accounts.remove(&account_key) {
                            this.closed_accounts.insert(account_key.clone());
                        }
                        cx.notify();
                    })),
            );
            {
                let prefix = format!("{}:", account_id.0);
                let collapsed = self
                    .collapsed
                    .iter()
                    .filter_map(|key| key.strip_prefix(&prefix).map(str::to_owned))
                    .collect();
                let visible_rows = tree_rows(account_folders, &collapsed);
                let visible: BTreeSet<_> =
                    visible_rows.iter().map(|row| row.name.clone()).collect();
                let all_rows = tree_rows(account_folders, &BTreeSet::new());
                // Bound disclosure channels for unusually large server catalogs.
                // Keep ordinary trees mounted through their closing animation.
                let rows = if all_rows.len() <= 128 {
                    all_rows
                } else if expanded {
                    visible_rows
                } else {
                    Vec::new()
                };
                for row in rows {
                    let shown = expanded && visible.contains(&row.name);
                    let key = format!("folder-{:?}-{:?}", account_id, row.name);
                    let height = motion::value(
                        format!("{key}-height"),
                        if shown { 44. } else { 0. },
                        MotionToken::DefaultSpatial,
                        window,
                        cx,
                    )
                    .clamp(0., 44.);
                    if height < 0.1 {
                        continue;
                    }
                    let angle = motion::value(
                        format!("{key}-chevron"),
                        if !collapsed.contains(&row.name) {
                            std::f32::consts::FRAC_PI_2
                        } else {
                            0.
                        },
                        MotionToken::FastSpatial,
                        window,
                        cx,
                    );
                    let target_account = account_id.clone();
                    let target = row.name.clone();
                    let toggle = format!("{}:{}", account_id.0, row.name);
                    let branch_key = toggle.clone();
                    let children = row.children;
                    let selectable = row.selectable;
                    let selected = !self.unified
                        && self.account.as_ref().map(|(id, _)| id) == Some(account_id)
                        && self.selected_folder.as_ref() == Some(&row.name);
                    let symbol = match row.name.to_ascii_lowercase().as_str() {
                        "inbox" => "inbox",
                        "drafts" | "draft" => "draft",
                        "sent" => "send",
                        "archive" | "archives" => "archive",
                        "trash" | "deleted" => "delete",
                        _ => "folder",
                    };
                    let background = motion::color(
                        format!("{key}-selection"),
                        if selected {
                            ColorRole::SECONDARY_CONTAINER
                        } else {
                            ColorRole::SURFACE_CONTAINER
                        },
                        window,
                        cx,
                    );
                    folders = folders.child(
                        div().h(px(height)).flex_none().overflow_hidden().child(
                            div()
                                .flex()
                                .items_center()
                                .h(px(44.))
                                .w_full()
                                .pl(px(8. + row.depth as f32 * 14.))
                                .bg(background)
                                .child(div().w(px(28.)).flex_none().when(children, |slot| {
                                    slot.child(
                                        IconButton::new(
                                            format!("{key}-branch"),
                                            icon("chevron_right").rotate(angle),
                                            "展开或折叠文件夹",
                                        )
                                        .variant(IconButtonVariant::Standard)
                                        .button_size(ButtonSize::ExtraSmall)
                                        .disabled(!shown || !children)
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                if !this.collapsed.remove(&toggle) {
                                                    this.collapsed.insert(toggle.clone());
                                                }
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                }))
                                .child(
                                    MaterialButton::new(format!("{key}-button"))
                                        .variant(ButtonVariant::Text)
                                        .tone(if selected {
                                            ButtonTone::Accent
                                        } else {
                                            ButtonTone::Neutral
                                        })
                                        .shape(ButtonShape::Square)
                                        .rounded_none()
                                        .flex_1()
                                        .min_w_0()
                                        .h(px(44.))
                                        .icon(icon(symbol))
                                        .child(div().flex_1().truncate().child(row.label))
                                        .disabled(!shown || (!selectable && !children))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if selectable {
                                                this.switch_account(
                                                    target_account.clone(),
                                                    target.clone(),
                                                    cx,
                                                );
                                            } else if !this.collapsed.remove(&branch_key) {
                                                this.collapsed.insert(branch_key.clone());
                                            }
                                            cx.notify();
                                        })),
                                ),
                        ),
                    );
                }
            }
        }
        if self.accounts.is_empty() {
            folders = folders.child(div().p_4().text_sm().child("添加邮箱，开始收发邮件"));
        }
        div()
            .w(px(216.))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER))
            .border_r_1()
            .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
            .child(
                div()
                    .h(px(72.))
                    .px_5()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        icon("mail")
                            .size(px(28.))
                            .color(theme::color(ColorRole::PRIMARY)),
                    )
                    .child(
                        div()
                            .text_size(px(23.))
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child("QingMail"),
                    ),
            )
            .child(
                div().px_5().pb_4().child(
                    MaterialButton::new("mailbox-compose")
                        .variant(ButtonVariant::Filled)
                        .shape(ButtonShape::Round)
                        .rounded(px(16.))
                        .w_full()
                        .h(px(42.))
                        .icon(icon("edit"))
                        .child("写邮件")
                        .disabled(self.account.is_none())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some((account, settings)) = this.compose_account() {
                                cx.defer(move |cx| {
                                    crate::composer::open_account(account, settings.address, cx)
                                });
                            }
                        })),
                ),
            )
            .child(folders)
            .child(
                div()
                    .px_3()
                    .py_3()
                    .border_t_1()
                    .border_color(theme::color(ColorRole::OUTLINE_VARIANT)),
            )
            .when(crate::settings::prefs(cx).show_log, |sidebar| {
                sidebar.child(
                    div()
                        .px_4()
                        .pb_4()
                        .text_size(px(11.))
                        .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                        .child(crate::lifecycle::sync_status(cx)),
                )
            })
    }
    fn sync_menu(&self, cx: &mut Context<Self>) -> MaterialMenu {
        use mail_protocol::{Command, OperationResolution, OperationState as State};
        let entity = cx.entity();
        let summary = self.sync_state.summary();
        let mut items = Vec::new();
        if summary.pending() > 0 {
            items.push(
                MaterialMenuItem::new(format!(
                    "未完成 {} 项 · 需处理 {} 项",
                    summary.pending(),
                    summary.issues
                ))
                .disabled(true),
            );
        }
        for operation in self
            .sync_state
            .data
            .operations
            .iter()
            .filter(|item| item.state != State::Applied)
            .take(6)
        {
            let summary = match &operation.operation {
                mail_protocol::MailOperation::Flags { seen, flagged } => {
                    format!("标记：已读 {seen:?} · 星标 {flagged:?}")
                }
                mail_protocol::MailOperation::Move { .. } => "移动或删除邮件".into(),
                mail_protocol::MailOperation::Trash { .. } => "移到服务器垃圾箱".into(),
                mail_protocol::MailOperation::Delete => "永久删除服务器邮件".into(),
            };
            items.push(
                MaterialMenuItem::new(format!(
                    "{} · {} · UID {}",
                    crate::operations::state_label(operation.state),
                    summary,
                    operation.location.uid
                ))
                .supporting_text(
                    operation
                        .detail
                        .as_ref()
                        .map(|e| e.message.clone())
                        .unwrap_or_default(),
                ),
            );
            if operation.state == State::Failed {
                let id = operation.id.clone();
                let retry_entity = entity.clone();
                items.push(
                    MaterialMenuItem::new("重试此操作").on_click(move |_, _, cx| {
                        let entity = retry_entity.clone();
                        let id = id.clone();
                        entity.update(cx, |this, cx| {
                            this.run_sync_operation(Command::RetryOperation { operation: id }, cx)
                        });
                    }),
                );
            }
            if matches!(
                operation.state,
                State::Failed | State::Conflict | State::Uncertain
            ) {
                let id = operation.id.clone();
                let reconcile_entity = entity.clone();
                items.push(
                    MaterialMenuItem::new("核对服务器状态").on_click(move |_, _, cx| {
                        let id = id.clone();
                        reconcile_entity.update(cx, |this, cx| {
                            this.run_sync_operation(
                                Command::ResolveOperation {
                                    operation: id,
                                    resolution: OperationResolution::Reconcile,
                                },
                                cx,
                            )
                        });
                    }),
                );
                let id = operation.id.clone();
                let keep_entity = entity.clone();
                let confirmed = self.keep_operation_confirm.as_ref() == Some(&id);
                items.push(
                    MaterialMenuItem::new(if confirmed {
                        "确认保留服务器状态"
                    } else {
                        "放弃此本地变更…"
                    })
                    .on_click(move |_, _, cx| {
                        keep_entity.update(cx, |this, cx| {
                            if this.keep_operation_confirm.as_ref() == Some(&id) {
                                this.keep_operation_confirm = None;
                                this.run_sync_operation(
                                    Command::ResolveOperation {
                                        operation: id.clone(),
                                        resolution: OperationResolution::KeepServer,
                                    },
                                    cx,
                                );
                            } else {
                                this.keep_operation_confirm = Some(id.clone());
                                cx.notify();
                            }
                        });
                    }),
                );
            }
        }
        for task in self
            .sync_state
            .data
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.state,
                    mail_core::SendState::Pending
                        | mail_core::SendState::Sending
                        | mail_core::SendState::Failed
                        | mail_core::SendState::Uncertain
                        | mail_core::SendState::CopyPending
                )
            })
            .take(6)
        {
            items.push(
                MaterialMenuItem::new(format!(
                    "{} · {}",
                    task.recipients.subject,
                    crate::send_labels::state_label(task)
                ))
                .supporting_text(format!("收件人：{}", task.recipients.to.join("、"))),
            );
            if matches!(
                task.state,
                mail_core::SendState::Failed | mail_core::SendState::Uncertain
            ) {
                let id = task.id.clone();
                let account = task.account.clone();
                let entity = entity.clone();
                items.push(
                    MaterialMenuItem::new(if task.state == mail_core::SendState::Uncertain {
                        "确认后可能重复发送…"
                    } else {
                        "重试发送…"
                    })
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.sync_open = false;
                            this.confirm_resend = Some((account.clone(), id.clone()));
                            cx.notify();
                        });
                    }),
                );
            }
        }
        for draft in self
            .sync_state
            .data
            .drafts
            .iter()
            .filter(|d| super::sync_status::draft_pending(&d.info))
            .take(6)
        {
            let label = match draft.info.state.as_str() {
                "deleting" => "正在清理云端草稿",
                "failed" => "草稿同步失败",
                "conflict" => "草稿同步冲突",
                "uncertain" => "草稿上传结果待确认",
                _ => "草稿尚未同步",
            };
            items.push(
                MaterialMenuItem::new(format!(
                    "{label} · {}",
                    if draft.info.subject.is_empty() {
                        "无主题"
                    } else {
                        &draft.info.subject
                    }
                ))
                .supporting_text(format!(
                    "{} · {}",
                    draft.address,
                    if draft.info.detail.is_empty() {
                        "等待后台同步"
                    } else {
                        &draft.info.detail
                    }
                )),
            );
        }
        MaterialMenu::new("sync-status-items")
            .density(MenuDensity::Dense)
            .width(240.)
            .max_height(360.)
            .items(items)
    }
    fn sync_status_popup(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (icon_name, label) = self.sync_state.indicator();
        MenuPopup::new(
            "sync-status-popup",
            tool("sync-status-trigger", icon_name, &label).on_click(cx.listener(
                |this, event: &ClickEvent, _, cx| {
                    if matches!(event, ClickEvent::Keyboard(_)) {
                        this.open_sync_status(cx);
                    }
                },
            )),
            self.sync_menu(cx),
        )
        .open(self.sync_open)
        .anchor(gpui_kit::Anchor::BottomRight)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.sync_open = *open;
            if *open {
                this.refresh_sync_status(cx);
            }
            cx.notify();
        }))
    }
    fn resend_confirmation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((account, id)) = self.confirm_resend.clone() else {
            return div();
        };
        let task = self
            .sync_state
            .data
            .tasks
            .iter()
            .find(|task| task.account == account && task.id == id);
        let uncertain = task.is_some_and(|task| task.state == mail_core::SendState::Uncertain);
        let description = if uncertain {
            "原邮件结果不确定，收件人可能已经收到。确认会新建发送任务并再次提交，可能重复发送。"
        } else {
            "将按原邮件内容创建新的发送任务。旧任务会保留；不会自动重发。"
        };
        div()
            .mx_4()
            .my_2()
            .p_3()
            .rounded(px(12.))
            .bg(theme::color(ColorRole::ERROR_CONTAINER))
            .flex()
            .items_center()
            .gap_3()
            .child(div().flex_1().text_sm().child(description))
            .child(
                MaterialButton::new("confirm-send-retry")
                    .child("确认重试")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm_resend(account.clone(), id.clone(), cx)
                    })),
            )
            .child(
                quiet("cancel-send-retry", "取消").on_click(cx.listener(|this, _, _, cx| {
                    this.confirm_resend = None;
                    cx.notify();
                })),
            )
    }
    fn page_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .gap_2()
            .child(
                tool("mailbox-accounts", "manage_accounts", "邮箱账号")
                    .selected(self.page == Page::Accounts)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_page(Page::Accounts, window, cx)
                    })),
            )
            .child(
                tool("mailbox-settings", "settings", "设置")
                    .selected(self.page == Page::Settings)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_page(Page::Settings, window, cx)
                    })),
            )
    }
    fn page_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(72.))
            .flex_none()
            .px_5()
            .flex()
            .items_center()
            .gap_3()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
            .child(tool("page-back", "arrow_back", "返回邮件").on_click(
                cx.listener(|this, _, window, cx| this.show_page(Page::Mail, window, cx)),
            ))
            .child(match self.page {
                Page::Accounts => "邮箱账号",
                Page::Settings => "设置",
                Page::Drafts => "草稿",
                Page::Mail => "邮件",
            })
            .child(div().flex_1())
            .child(self.page_tools(cx))
            .child(
                tool("page-sync", "sync", "同步邮件")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.sync_folder(false, cx))),
            )
            .child(MenuPopup::new(
                "page-menu",
                tool("page-more", "more_vert", "更多选项"),
                MaterialMenu::new("page-menu-items").items([
                    MaterialMenuItem::new("返回邮件")
                        .on_click(|_, _, cx| crate::lifecycle::open_page(Page::Mail, cx)),
                    MaterialMenuItem::new("保存并退出")
                        .on_click(|_, _, cx| crate::lifecycle::quit(cx)),
                ]),
            ))
    }
    fn topbar(&self, title: &str, wide: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let draft_search = self
            .composer
            .as_ref()
            .filter(|_| self.page == Page::Drafts)
            .map(|editor| editor.read(cx).workspace_search());
        let menu = MaterialMenu::new("mailbox-menu").items([
            MaterialMenuItem::new("重载本地账号与目录")
                .disabled(self.busy)
                .on_click({
                    let entity = entity.clone();
                    move |_, _, cx| entity.update(cx, |this, cx| this.reload(cx))
                }),
            MaterialMenuItem::new("刷新服务器文件夹")
                .disabled(self.busy || self.account.is_none())
                .on_click({
                    let entity = entity.clone();
                    move |_, _, cx| entity.update(cx, |this, cx| this.refresh_catalog(cx))
                }),
            MaterialMenuItem::new(if crate::lifecycle::sync_paused(cx) {
                "恢复自动收信"
            } else {
                "暂停自动收信"
            })
            .on_click(|_, _, cx| crate::lifecycle::toggle_sync(cx)),
            MaterialMenuItem::new("查询较早邮件（90天内）")
                .disabled(self.busy || self.selected_folder.is_none())
                .on_click({
                    let entity = entity.clone();
                    move |_, _, cx| entity.update(cx, |this, cx| this.sync_folder(true, cx))
                }),
            MaterialMenuItem::new("加载更早历史（90天前）")
                .disabled(self.busy || self.selected_folder.is_none() || self.history_exhausted)
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.load_older_history(cx))
                }),
        ]);
        div()
            .h(px(72.))
            .flex_none()
            .px_5()
            .flex()
            .items_center()
            .gap_3()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
            .when(wide, |row| {
                row.child(
                    div()
                        .min_w_0()
                        .w(px(200.))
                        .text_size(px(13.))
                        .truncate()
                        .child(format!(
                            "{} / {title}",
                            if self.unified {
                                "全部账号"
                            } else {
                                self.account
                                    .as_ref()
                                    .map(|(_, s)| s.display_name.as_str())
                                    .unwrap_or("邮箱")
                            }
                        )),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .max_w(px(560.))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "enter" && this.page != Page::Drafts {
                            this.run_search(cx);
                        }
                    }))
                    .child(
                        TextField::new(
                            "mail-search-text",
                            draft_search.as_ref().unwrap_or(&self.search_text),
                        )
                        .outlined()
                        .density(TextFieldDensity::Compact)
                        .placeholder(if self.page == Page::Drafts {
                            "搜索草稿"
                        } else {
                            "搜索本地邮件"
                        })
                        .leading_icon(icon("search"))
                        .w_full(),
                    ),
            )
            .child(
                tool("run-search", "search", "搜索本地邮件")
                    .disabled(self.busy || self.page == Page::Drafts)
                    .on_click(cx.listener(|this, _, _, cx| this.run_search(cx))),
            )
            .child(
                tool("search-filters", "filter_list", "搜索筛选")
                    .disabled(self.page == Page::Drafts)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.filters_open = !this.filters_open;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(self.page_tools(cx))
            .child(
                tool(
                    "sync-folder",
                    "sync",
                    if self.unified {
                        "同步所有账号"
                    } else {
                        "同步当前文件夹"
                    },
                )
                .disabled(self.busy || (!self.unified && self.selected_folder.is_none()))
                .on_click(cx.listener(|this, _, _, cx| this.sync_folder(false, cx))),
            )
            .child(
                MenuPopup::new(
                    "mailbox-popup",
                    tool("mailbox-more", "more_vert", "更多邮箱操作").on_click(cx.listener(
                        |this, event: &ClickEvent, _, cx| {
                            if matches!(event, ClickEvent::Keyboard(_)) {
                                this.more_open = !this.more_open;
                                cx.notify();
                            }
                        },
                    )),
                    menu,
                )
                .open(self.more_open)
                .close_on_activate(true)
                .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                    this.more_open = *open;
                    cx.notify();
                })),
            )
    }
    fn search_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_5()
            .py_2()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
            .child(
                TextField::new("mail-search-sender", &self.search_sender)
                    .outlined()
                    .density(TextFieldDensity::Compact)
                    .placeholder("发件人"),
            )
            .child(
                quiet(
                    "search-scope",
                    &match self.search_scope {
                        0 => "范围：当前文件夹".to_string(),
                        1 => format!(
                            "范围：账号 {}",
                            self.account
                                .as_ref()
                                .map(|(_, s)| s.address.as_str())
                                .unwrap_or("未选择")
                        ),
                        _ => "范围：全部本地缓存".to_string(),
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.search_scope = (this.search_scope + 1) % 3;
                    cx.notify();
                })),
            )
            .child(
                quiet(
                    "search-attachments",
                    if self.search_attachments {
                        "✓ 有附件"
                    } else {
                        "有附件"
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.search_attachments = !this.search_attachments;
                    cx.notify();
                })),
            )
            .child(
                quiet("clear-search", "清除搜索")
                    .disabled(self.busy || self.search_results.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx))),
            )
    }
    fn message_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let disabled = self.busy || self.search_results.is_some() || self.targets().is_empty();
        let mut items = vec![];
        for (label, seen, flagged) in [
            ("标为已读", Some(true), None),
            ("标为未读", Some(false), None),
            ("添加星标", None, Some(true)),
            ("取消星标", None, Some(false)),
        ] {
            let entity = entity.clone();
            items.push(MaterialMenuItem::new(label).disabled(disabled).on_click(
                move |_, _, cx| entity.update(cx, |this, cx| this.apply_flags(seen, flagged, cx)),
            ));
        }
        items.push(
            MaterialMenuItem::new("全选本页")
                .disabled(self.busy || self.search_results.is_some())
                .on_click({
                    let entity = entity.clone();
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.checked =
                                this.messages().iter().map(|m| m.location.clone()).collect();
                            this.confirm_delete = false;
                            cx.notify();
                        })
                    }
                }),
        );
        items.push(MaterialMenuItem::new("清除选择").on_click({
            let entity = entity.clone();
            move |_, _, cx| {
                entity.update(cx, |this, cx| {
                    this.checked.clear();
                    this.confirm_delete = false;
                    cx.notify();
                })
            }
        }));
        items.push(
            MaterialMenuItem::new("在独立窗口阅读")
                .disabled(self.inline.is_none())
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        if let Some(inline) = &this.inline {
                            let content = inline.content.clone();
                            let account = this
                                .accounts
                                .iter()
                                .find(|(id, _)| {
                                    inline
                                        .location
                                        .as_ref()
                                        .is_some_and(|loc| *id == loc.account)
                                })
                                .map(|(id, s)| (id.clone(), s.address.clone()))
                                .or_else(|| inline.account.clone());
                            cx.defer(move |cx| crate::reader::open_content(content, account, cx));
                        }
                    })
                }),
        );
        MenuPopup::new(
            "message-popup",
            tool("message-more", "more_vert", "更多邮件操作").on_click(cx.listener(
                |this, event: &ClickEvent, _, cx| {
                    if matches!(event, ClickEvent::Keyboard(_)) {
                        this.message_more = !this.message_more;
                        cx.notify();
                    }
                },
            )),
            MaterialMenu::new("message-menu").items(items),
        )
        .open(self.message_more)
        .close_on_activate(true)
        .on_open_change(cx.listener(|this, open: &bool, _, cx| {
            this.message_more = *open;
            cx.notify();
        }))
    }
    fn message_row(
        &self,
        index: usize,
        message: &CachedMessage,
        search: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = if search {
            self.search_selected == Some(index)
        } else {
            self.selected == Some(index)
        };
        let motion_id = format!("mail-row-{:?}", message.location);
        let background = motion::color(
            format!("{motion_id}-background"),
            if selected {
                ColorRole::SECONDARY_CONTAINER
            } else {
                ColorRole::SURFACE
            },
            window,
            cx,
        );
        let border = motion::color(
            format!("{motion_id}-border"),
            if selected {
                ColorRole::PRIMARY
            } else {
                ColorRole::SURFACE
            },
            window,
            cx,
        );
        let unread = motion::value(
            format!("{motion_id}-unread"),
            if message.seen { 0. } else { 1. },
            MotionToken::FastEffects,
            window,
            cx,
        )
        .clamp(0., 1.);
        let star = motion::color(
            format!("{motion_id}-star"),
            if message.flagged {
                ColorRole::PRIMARY
            } else {
                ColorRole::OUTLINE
            },
            window,
            cx,
        );
        let uid = message.location.clone();
        let checked = self.checked.contains(&uid);
        let time = sync::display_time(message.envelope.received_at);
        let time = time.get(..10).unwrap_or(&time).to_owned();
        div()
            .id((if search { "search-hit" } else { "message-row" }, index))
            .w_full()
            .h(px(if crate::settings::prefs(cx).compact {
                68.
            } else {
                84.
            }))
            .flex()
            .items_center()
            .gap_2()
            .pr_3()
            .cursor_pointer()
            .border_l_4()
            .border_color(border)
            .bg(background)
            .when(!search, |row| {
                row.child(
                    tool(
                        ("check-message", index),
                        if checked {
                            "check_box"
                        } else {
                            "check_box_outline_blank"
                        },
                        "选择邮件",
                    )
                    .button_size(ButtonSize::ExtraSmall)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_checked(uid.clone(), cx);
                    })),
                )
            })
            .child(
                div()
                    .w(px(6.))
                    .h(px(6.))
                    .rounded_full()
                    .bg(theme::color(ColorRole::PRIMARY))
                    .opacity(unread),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .font_weight(if message.seen {
                                        gpui_kit::FontWeight::MEDIUM
                                    } else {
                                        gpui_kit::FontWeight::BOLD
                                    })
                                    .child(format!(
                                        "{}{}{}",
                                        message.envelope.sender,
                                        crate::lifecycle::operation_label(&message.location, cx),
                                        if self.unified || self.search_scope == 2 {
                                            self.accounts
                                                .iter()
                                                .find(|(id, _)| *id == message.location.account)
                                                .map(|(_, s)| format!(" · {}", s.address))
                                                .unwrap_or_default()
                                        } else {
                                            String::new()
                                        }
                                    )),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.))
                                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                                    .child(time),
                            ),
                    )
                    .child(div().text_size(px(13.)).truncate().child(
                        if message.envelope.subject.is_empty() {
                            "（无主题）".to_owned()
                        } else {
                            message.envelope.subject.clone()
                        },
                    )),
            )
            .child(
                icon(if message.flagged {
                    "star"
                } else {
                    "star_outline"
                })
                .size(px(18.))
                .color(star),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                if search {
                    this.open_hit(index, cx);
                } else {
                    this.select_message(index, cx);
                }
                this.narrow_detail = window.bounds().size.width < px(1050.);
                window.focus(&this.focus, cx);
            }))
    }
}

impl Render for Mailbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::settings::advance_theme(window, cx);
        if matches!(self.page, Page::Accounts | Page::Settings) {
            let content = match self.page {
                Page::Accounts => self
                    .account_page
                    .as_ref()
                    .expect("account page initialized")
                    .clone()
                    .into_any_element(),
                Page::Settings => self
                    .settings_page
                    .as_ref()
                    .expect("settings page initialized")
                    .clone()
                    .into_any_element(),
                Page::Mail | Page::Drafts => unreachable!(),
            };
            return div()
                .size_full()
                .flex()
                .bg(theme::color(ColorRole::SURFACE))
                .text_color(theme::color(ColorRole::ON_SURFACE))
                .child(self.sidebar(window, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(self.page_bar(cx))
                        .child(motion::page(
                            format!("mailbox-page-{:?}", self.page),
                            div().flex_1().min_h_0().overflow_hidden().child(content),
                            cx,
                        )),
                )
                .into_any_element();
        }
        let title = self
            .selected_folder
            .as_deref()
            .map(folder_label)
            .unwrap_or_else(|| {
                if self.unified {
                    "统一收件箱".into()
                } else {
                    "邮件".into()
                }
            });
        let count = self
            .search_results
            .as_ref()
            .map_or(self.messages().len(), Vec::len);
        let mut list = div()
            .id("mail-index-focus")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::keys))
            .h_full()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .child(
                div()
                    .h(px(72.))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(22.))
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child(if self.search_results.is_some() {
                                "搜索结果".to_owned()
                            } else {
                                title.clone()
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                            .child(format!("{count} 封")),
                    )
                    .when(self.search_results.is_some(), |row| {
                        row.child(
                            tool("close-search", "close", "清除搜索")
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx))),
                        )
                    })
                    .child(self.message_menu(cx)),
            )
            .child(
                div()
                    .px_4()
                    .pb_3()
                    .text_size(px(12.))
                    .text_color(theme::color(ColorRole::PRIMARY))
                    .child(if self.checked.is_empty() {
                        "最近邮件 · 本地缓存".to_owned()
                    } else {
                        format!("已选择 {} 封", self.checked.len())
                    }),
            );
        if count == 0 {
            list = list.child(
                div()
                    .flex_1()
                    .p_5()
                    .text_sm()
                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child(if self.search_results.is_some() {
                        "没有匹配的本地邮件"
                    } else if self.selected_folder.is_some() {
                        "暂无邮件。同步完成后会自动更新"
                    } else {
                        "选择文件夹，查看邮件"
                    }),
            );
        } else {
            list = list.child(
                uniform_list(
                    "message-index",
                    count,
                    cx.processor(|this, range: std::ops::Range<usize>, window, cx| {
                        if range.end >= this.messages().len().saturating_sub(5)
                            && this.can_load_more()
                        {
                            let entity = cx.weak_entity();
                            cx.defer(move |cx| {
                                let _ = entity.update(cx, |this, cx| this.load_more(cx));
                            });
                        }
                        range
                            .map(|index| {
                                if let Some(results) = &this.search_results {
                                    this.message_row(
                                        index,
                                        &results[index].message,
                                        true,
                                        window,
                                        cx,
                                    )
                                    .into_any_element()
                                } else {
                                    this.message_row(
                                        index,
                                        &this.messages()[index],
                                        false,
                                        window,
                                        cx,
                                    )
                                    .into_any_element()
                                }
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            );
        }
        list = list.child(
            div()
                .h(px(48.))
                .flex_none()
                .px_3()
                .flex()
                .items_center()
                .border_t_1()
                .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(11.))
                        .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                        .child(if self.busy {
                            "正在更新…"
                        } else if self.search_results.is_some() {
                            "本地搜索结果"
                        } else if self.list_load_failed {
                            "加载失败，已保留列表"
                        } else if self.list_exhausted {
                            "已显示全部本地邮件"
                        } else {
                            "向下滚动加载更多"
                        }),
                )
                .when(
                    self.list_load_failed && self.search_results.is_none(),
                    |row| {
                        row.child(
                            quiet("retry-list-load", "重试加载")
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.list_load_failed = false;
                                    this.load_more(cx);
                                })),
                        )
                    },
                ),
        );
        let wide = window.bounds().size.width >= px(1050.);
        let inline = self.inline.as_ref().filter(|inline| {
            super::inline_matches_selection(inline.location.as_ref(), self.selected_location())
        });
        let disabled = self.busy || self.search_results.is_some() || self.targets().is_empty();
        let mut detail = div()
            .h_full()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .bg(theme::color(ColorRole::SURFACE));
        detail = detail.child(
            div()
                .h(px(64.))
                .flex_none()
                .px_4()
                .flex()
                .items_center()
                .gap_1()
                .border_b_1()
                .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                .when(!wide, |row| {
                    row.child(
                        tool("back-to-list", "arrow_back", "返回列表").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.narrow_detail = false;
                                cx.notify();
                            },
                        )),
                    )
                })
                .children(
                    [
                        (
                            "reply-inline",
                            "回复",
                            "reply",
                            crate::reply::ReplyMode::Reply,
                        ),
                        (
                            "reply-all-inline",
                            "回复全部",
                            "reply_all",
                            crate::reply::ReplyMode::ReplyAll,
                        ),
                        (
                            "forward-inline",
                            "转发",
                            "forward",
                            crate::reply::ReplyMode::Forward,
                        ),
                    ]
                    .into_iter()
                    .map(|(id, label, symbol, mode)| {
                        let reader = inline.map(|inline| inline.view.clone());
                        quiet(id, label)
                            .icon(icon(symbol))
                            .disabled(reader.is_none())
                            .on_click(move |_, _, cx| {
                                if let Some(reader) = &reader {
                                    reader.update(cx, |reader, cx| reader.compose(mode, cx));
                                }
                            })
                    }),
                )
                .child(div().flex_1())
                .when(self.undo_flags.is_some(), |row| {
                    row.child(
                        quiet("undo-flags", "撤销标记")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.undo_flags(cx))),
                    )
                })
                .child(
                    tool("mark-seen", "mark_email_read", "标为已读")
                        .disabled(disabled)
                        .on_click(
                            cx.listener(|this, _, _, cx| this.apply_flags(Some(true), None, cx)),
                        ),
                )
                .child(
                    tool("archive-selected", "archive", "归档")
                        .disabled(disabled)
                        .on_click(cx.listener(|this, _, _, cx| this.move_selected(false, cx))),
                )
                .child(
                    tool("delete-selected", "delete", "删除邮件（同步服务器）")
                        .disabled(disabled)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_delete = !this.confirm_delete;
                            cx.notify();
                        })),
                ),
        );
        if self.confirm_delete {
            detail = detail.child(motion::enter(
                "mail-delete-enter",
                div()
                    .px_4()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(theme::color(ColorRole::ERROR_CONTAINER))
                    .child(div().flex_1().text_sm().child(
                        if !self.unified
                            && self.selected_folder.as_deref()
                                == mail_service::folders::trash(&self.folders, None).as_deref()
                        {
                            format!(
                                "从服务器永久删除 {} 封邮件？此操作无法撤销。",
                                self.targets().len()
                            )
                        } else {
                            format!("将 {} 封邮件移到服务器垃圾箱？", self.targets().len())
                        },
                    ))
                    .child(
                        quiet("confirm-delete", "确认删除")
                            .disabled(disabled)
                            .on_click(cx.listener(|this, _, _, cx| this.move_selected(true, cx))),
                    )
                    .child(quiet("cancel-delete", "取消").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.confirm_delete = false;
                            cx.notify();
                        },
                    ))),
                cx,
            ));
        }
        detail = if let Some(inline) = inline {
            detail.child(motion::page(
                format!("reader-{:?}", inline.location),
                div().flex_1().min_h_0().child(inline.view.clone()),
                cx,
            ))
        } else {
            detail.child(
                div()
                    .flex_1()
                    .p_8()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .gap_4()
                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child(icon("drafts").size(px(40.)))
                    .child(if self.selected_location().is_some() {
                        self.reading_status.clone()
                    } else {
                        "选择一封邮件，开始阅读".into()
                    }),
            )
        };
        let mut detail = if let Some(confirmation) = &self.sent_confirmation {
            motion::page(
                format!("sent-confirmation-{}", confirmation.task.0),
                div()
                    .size_full()
                    .child(self.sent_confirmation_view(confirmation, cx)),
                cx,
            )
        } else {
            detail.into_any_element()
        };
        let mut list = list.into_any_element();
        if self.page == Page::Drafts
            && let Some(editor) = &self.composer
        {
            list = editor.update(cx, |editor, cx| editor.workspace_list(window, cx));
        }
        let available = window.bounds().size.width.as_f32() - 216.;
        self.list_width = self.list_width.clamp(300., (available - 420.).max(300.));
        if let Some(editor) = self.composer.as_ref() {
            let width = if wide {
                available - self.list_width - 5.
            } else {
                available
            };
            // Only update the child layout hint; the child renders after this host.
            editor.update(cx, |editor, _| editor.pane_width = width);
            detail = (if self.page == Page::Drafts {
                div().size_full().min_w_0().min_h_0().child(editor.clone())
            } else {
                div().size_full().min_w_0().min_h_0().child(motion::page(
                    "inline-composer-enter",
                    div().size_full().child(editor.clone()),
                    cx,
                ))
            })
            .into_any_element();
        }
        let body = if wide {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .w(px(self.list_width))
                        .h_full()
                        .flex_none()
                        .child(list),
                )
                .child(
                    div()
                        .id("mail-pane-divider")
                        .w(px(5.))
                        .h_full()
                        .flex_none()
                        .cursor_col_resize()
                        .bg(theme::color(ColorRole::OUTLINE_VARIANT))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                this.resize_start =
                                    Some((event.position.x.as_f32(), this.list_width));
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_hidden()
                        .child(div().size_full().child(detail)),
                )
        } else if self.narrow_detail
            || self.composer.as_ref().is_some_and(|editor| {
                self.page != Page::Drafts || !editor.read(cx).workspace_list_visible()
            })
        {
            div().flex_1().min_h_0().child(detail)
        } else {
            div().flex_1().min_h_0().child(list)
        };
        div()
            .size_full()
            .flex()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if event.pressed_button != Some(MouseButton::Left) {
                    this.resize_start = None;
                }
                if let Some((start, width)) = this.resize_start {
                    this.list_width = (width + event.position.x.as_f32() - start).clamp(
                        300.,
                        (window.bounds().size.width.as_f32() - 216. - 420.).max(300.),
                    );
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.resize_start = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.resize_start = None;
                    cx.notify();
                }),
            )
            .child(self.sidebar(window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.topbar(&title, wide, cx))
                    .when(self.filters_open && self.page != Page::Drafts, |column| {
                        column.child(motion::enter(
                            "search-filter-enter",
                            div().flex_none().child(self.search_filters(cx)),
                            cx,
                        ))
                    })
                    .when(self.confirm_resend.is_some(), |column| {
                        column.child(motion::enter(
                            "resend-confirm-enter",
                            div().flex_none().child(self.resend_confirmation(cx)),
                            cx,
                        ))
                    })
                    .child(body)
                    .child(
                        div()
                            .min_h(px(28.))
                            .max_h(px(80.))
                            .flex_none()
                            .px_4()
                            .py_1()
                            .flex()
                            .items_center()
                            .text_size(px(11.))
                            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                            .overflow_hidden()
                            .child(self.status.clone())
                            .child(div().flex_1())
                            .child(self.sync_status_popup(cx)),
                    ),
            )
            .into_any_element()
    }
}

fn sent_detail_row(label: &'static str, value: String) -> impl IntoElement {
    div()
        .flex()
        .items_start()
        .gap_3()
        .text_size(px(13.))
        .child(
            div()
                .w(px(58.))
                .flex_none()
                .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .text_color(theme::color(ColorRole::ON_SURFACE))
                .child(value),
        )
}
