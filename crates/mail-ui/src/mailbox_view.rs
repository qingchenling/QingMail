//! 三栏邮箱：侧栏保持直角，列表和正文各有固定工具栏；业务操作复用 Mailbox。
use super::*;
use crate::ui::{folder_label, icon, quiet, tool};
use gpui_kit::{
    ClickEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Render, div, uniform_list,
};
use gpui_kit_material::{prelude::*, theme};

impl Mailbox {
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(icon(if expanded {
                        "expand_more"
                    } else {
                        "chevron_right"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.closed_accounts.remove(&account_key) {
                            this.closed_accounts.insert(account_key.clone());
                        }
                        cx.notify();
                    })),
            );
            if expanded {
                let prefix = format!("{}:", account_id.0);
                let collapsed = self
                    .collapsed
                    .iter()
                    .filter_map(|key| key.strip_prefix(&prefix).map(str::to_owned))
                    .collect();
                for (folder_index, row) in tree_rows(account_folders, &collapsed)
                    .into_iter()
                    .enumerate()
                {
                    let index = account_index * 8192 + folder_index;
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
                    folders = folders.child(
                        div()
                            .flex()
                            .items_center()
                            .h(px(44.))
                            .w_full()
                            .pl(px(8. + row.depth as f32 * 14.))
                            .bg(theme::color(if selected {
                                ColorRole::SECONDARY_CONTAINER
                            } else {
                                ColorRole::SURFACE_CONTAINER
                            }))
                            .child(div().w(px(28.)).flex_none().when(children, |slot| {
                                slot.child(
                                    tool(
                                        ("branch", index),
                                        if children && !collapsed.contains(&row.name) {
                                            "expand_more"
                                        } else {
                                            "chevron_right"
                                        },
                                        "展开或折叠文件夹",
                                    )
                                    .button_size(ButtonSize::ExtraSmall)
                                    .disabled(!children)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            if !this.collapsed.remove(&toggle) {
                                                this.collapsed.insert(toggle.clone());
                                            }
                                            cx.notify();
                                        },
                                    )),
                                )
                            }))
                            .child(
                                MaterialButton::new(("folder", index))
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
                                    .disabled(!selectable && !children)
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
                div().px_4().pb_4().child(
                    MaterialButton::new("mailbox-compose")
                        .variant(ButtonVariant::Filled)
                        .shape(ButtonShape::Square)
                        .rounded_none()
                        .w_full()
                        .h(px(48.))
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
                    .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                    .child(
                        quiet("mailbox-account-settings", "账号管理")
                            .icon(icon("manage_accounts"))
                            .w_full()
                            .shape(ButtonShape::Square)
                            .rounded_none()
                            .on_click(|_, _, cx| crate::accounts::open(cx)),
                    )
                    .child(
                        quiet("mailbox-send-tasks", "发送任务")
                            .icon(icon("outbox"))
                            .w_full()
                            .shape(ButtonShape::Square)
                            .rounded_none()
                            .on_click(|_, _, cx| crate::send_tasks::open(cx)),
                    )
                    .child(
                        quiet(
                            "mailbox-operations",
                            &crate::lifecycle::operation_summary(cx),
                        )
                        .icon(icon("sync"))
                        .w_full()
                        .shape(ButtonShape::Square)
                        .rounded_none()
                        .on_click(|_, _, cx| crate::operations::open(cx)),
                    )
                    .child(
                        quiet("mailbox-server-drafts", "服务器草稿")
                            .icon(icon("drafts"))
                            .w_full()
                            .shape(ButtonShape::Square)
                            .rounded_none()
                            .on_click(|_, _, cx| crate::server_drafts::open(cx)),
                    )
                    .child(
                        quiet("mailbox-settings", "设置")
                            .icon(icon("settings"))
                            .w_full()
                            .shape(ButtonShape::Square)
                            .rounded_none()
                            .on_click(|_, _, cx| crate::settings::open(cx)),
                    ),
            )
            .child(
                div()
                    .px_4()
                    .pb_4()
                    .text_size(px(11.))
                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child(crate::lifecycle::sync_status(cx)),
            )
    }
    fn topbar(&self, title: &str, wide: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
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
                        if event.keystroke.key == "enter" {
                            this.run_search(cx);
                        }
                    }))
                    .child(
                        TextField::new("mail-search-text", &self.search_text)
                            .outlined()
                            .density(TextFieldDensity::Compact)
                            .placeholder("搜索本地邮件")
                            .leading_icon(icon("search"))
                            .w_full(),
                    ),
            )
            .child(
                tool("run-search", "search", "搜索本地邮件")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.run_search(cx))),
            )
            .child(
                tool("search-filters", "filter_list", "搜索筛选").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.filters_open = !this.filters_open;
                        cx.notify();
                    },
                )),
            )
            .child(div().flex_1())
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
                                .find(|(id, _)| *id == inline.location.account)
                                .map(|(id, s)| (id.clone(), s.address.clone()));
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
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = if search {
            self.search_selected == Some(index)
        } else {
            self.selected == Some(index)
        };
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
            .border_color(theme::color(if selected {
                ColorRole::PRIMARY
            } else {
                ColorRole::SURFACE
            }))
            .bg(theme::color(if selected {
                ColorRole::SECONDARY_CONTAINER
            } else {
                ColorRole::SURFACE
            }))
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
                    .when(!message.seen, |dot| {
                        dot.bg(theme::color(ColorRole::PRIMARY))
                    }),
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
                .color(theme::color(if message.flagged {
                    ColorRole::PRIMARY
                } else {
                    ColorRole::OUTLINE
                })),
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
                            .child(format!("{count} 封 · 本页")),
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
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                if let Some(results) = &this.search_results {
                                    this.message_row(index, &results[index].message, true, cx)
                                        .into_any_element()
                                } else {
                                    this.message_row(index, &this.messages()[index], false, cx)
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
                        } else {
                            "邮件内容按需加载"
                        }),
                )
                .when(self.search_results.is_none(), |row| {
                    row.child(
                        tool("first-cache-page", "chevron_left", "返回最新缓存")
                            .disabled(
                                self.busy
                                    || if self.unified {
                                        self.inbox_before.is_none()
                                    } else {
                                        self.before.is_none()
                                    },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.before = None;
                                this.inbox_before = None;
                                this.load_cache(cx);
                            })),
                    )
                    .child(
                        tool("older-cache-page", "chevron_right", "下一页缓存")
                            .disabled(
                                self.busy
                                    || if self.unified {
                                        self.inbox.as_ref().is_none_or(|p| p.next.is_none())
                                    } else {
                                        self.messages().is_empty()
                                    },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.before = this.messages().last().map(|m| m.location.uid);
                                this.inbox_before =
                                    this.inbox.as_ref().and_then(|p| p.next.clone());
                                this.load_cache(cx);
                            })),
                    )
                }),
        );
        let wide = window.bounds().size.width >= px(1050.);
        let inline = self
            .inline
            .as_ref()
            .filter(|inline| self.selected_location() == Some(&inline.location));
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
                    tool("delete-selected", "delete", "移到已删除")
                        .disabled(disabled)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_delete = !this.confirm_delete;
                            cx.notify();
                        })),
                ),
        );
        if self.confirm_delete {
            detail = detail.child(
                div()
                    .px_4()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(theme::color(ColorRole::ERROR_CONTAINER))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .child(format!("将 {} 封邮件移到已删除？", self.targets().len())),
                    )
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
            );
        }
        detail = if let Some(inline) = inline {
            detail.child(div().flex_1().min_h_0().child(inline.view.clone()))
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
        let available = window.bounds().size.width.as_f32() - 216.;
        self.list_width = self.list_width.clamp(300., (available - 420.).max(300.));
        if let Some(editor) = &self.composer {
            let width = if wide {
                available - self.list_width - 5.
            } else {
                available
            };
            // Only update the child layout hint; the child renders after this host.
            editor.update(cx, |editor, _| editor.pane_width = width);
            detail = div().size_full().min_w_0().min_h_0().child(editor.clone());
        }
        let body = if wide {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(list.w(px(self.list_width)).flex_none())
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
                        .child(detail.w_full()),
                )
        } else if self.narrow_detail || self.composer.is_some() {
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
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.topbar(&title, wide, cx))
                    .when(self.filters_open, |column| {
                        column.child(self.search_filters(cx))
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
                            .child(self.status.clone()),
                    ),
            )
    }
}
