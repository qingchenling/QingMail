//! 主窗口右侧的写信工作区；复用编辑、草稿、附件和发送状态机。
use super::*;
use crate::ui::{icon, motion, quiet, tool};
use gpui_kit::{ClickEvent, Focusable};

impl Composer {
    pub(super) fn edit_disabled(&self) -> bool {
        self.loading
            || self.closing
            || self.deleting
            || self.store.is_none()
            || self.stored.is_none()
            || self.send_locked()
    }
    fn format_active(&self, command: &str, cx: &App) -> bool {
        let session = self.session.borrow();
        let range = self.input.read(cx).selected_range();
        let offset = session
            .draft
            .scalar_range(range.clone())
            .map(|r| r.start)
            .unwrap_or(0);
        let attr = session
            .typing
            .as_ref()
            .filter(|(i, _)| range.is_empty() && *i == range.start)
            .map(|(_, a)| a)
            .or_else(|| session.draft.attributes().get(offset));
        attr.is_some_and(|a| match command {
            "bold" => a.bold,
            "italic" => a.italic,
            "bullet" => a.block == Block::Bullet,
            "quote" => a.block == Block::Quote,
            _ => false,
        })
    }
    fn more_menu(&self, compact: bool, cx: &mut Context<Self>) -> MaterialMenu {
        let entity = cx.entity();
        let mut items = Vec::new();
        if compact {
            for (command, label) in [("bullet", "项目列表"), ("quote", "引用")] {
                let entity = entity.clone();
                items.push(
                    MaterialMenuItem::new(label)
                        .disabled(self.edit_disabled())
                        .on_click(move |_, w, cx| {
                            entity.update(cx, |this, cx| this.format(command, w, cx));
                        }),
                );
            }
        }
        for (command, label) in [
            ("paragraph", "普通段落"),
            ("signature", "签名格式"),
            ("unlink", "移除链接"),
        ] {
            let entity = entity.clone();
            items.push(
                MaterialMenuItem::new(label)
                    .disabled(self.edit_disabled())
                    .on_click(move |_, w, cx| {
                        entity.update(cx, |this, cx| this.format(command, w, cx));
                    }),
            );
        }
        let paste = entity.clone();
        items.push(
            MaterialMenuItem::new("粘贴 HTML（安全转换）")
                .disabled(self.edit_disabled())
                .on_click(move |_, w, cx| {
                    paste.update(cx, |this, cx| this.paste_html(w, cx));
                }),
        );
        let export = entity.clone();
        items.push(
            MaterialMenuItem::new("另存草稿…")
                .disabled(self.loading)
                .on_click(move |_, _, cx| {
                    export.update(cx, |this, cx| this.save_as(cx));
                }),
        );
        items.push(
            MaterialMenuItem::new("刷新发送记录").on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| this.refresh_send_tasks(cx));
            }),
        );
        MaterialMenu::new("compose-more-menu").items(items)
    }
    fn toolbar(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let disabled = self.edit_disabled();
        let mut row = div()
            .min_h(px(64.))
            .flex_none()
            .px_3()
            .py_2()
            .flex()
            .items_center()
            .gap_1()
            .flex_wrap()
            .border_b_1()
            .border_color(theme::color(ColorRole::OUTLINE_VARIANT));
        for (id, symbol, label) in [
            ("bold", "format_bold", "粗体 · Ctrl+B"),
            ("italic", "format_italic", "斜体 · Ctrl+I"),
        ] {
            row = row.child(
                tool(id, symbol, label)
                    .toggle(true)
                    .selected(self.format_active(id, cx))
                    .button_size(ButtonSize::ExtraSmall)
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, w, cx| this.format(id, w, cx))),
            );
        }
        row = row.child(
            tool("link", "link", "添加链接")
                .toggle(true)
                .selected(self.show_link)
                .button_size(ButtonSize::ExtraSmall)
                .disabled(disabled)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_link = !this.show_link;
                    cx.notify();
                })),
        );
        if !compact {
            for (id, symbol, label) in [
                ("bullet", "format_list_bulleted", "项目列表"),
                ("quote", "format_quote", "引用"),
            ] {
                row = row.child(
                    tool(id, symbol, label)
                        .toggle(true)
                        .selected(self.format_active(id, cx))
                        .button_size(ButtonSize::ExtraSmall)
                        .disabled(disabled)
                        .on_click(cx.listener(move |this, _, w, cx| this.format(id, w, cx))),
                );
            }
        }
        row.child(
            tool("undo", "undo", "撤销 · Ctrl+Z")
                .button_size(ButtonSize::ExtraSmall)
                .disabled(disabled || self.session.borrow().undo.is_empty())
                .on_click(cx.listener(|this, _, w, cx| this.history(false, w, cx))),
        )
        .child(
            tool("redo", "redo", "重做 · Ctrl+Shift+Z")
                .button_size(ButtonSize::ExtraSmall)
                .disabled(disabled || self.session.borrow().redo.is_empty())
                .on_click(cx.listener(|this, _, w, cx| this.history(true, w, cx))),
        )
        .child(
            tool("add-draft-attachment", "attach_file", "添加附件")
                .button_size(ButtonSize::ExtraSmall)
                .disabled(disabled || self.importing)
                .on_click(cx.listener(|this, _, _, cx| this.add_attachment(cx))),
        )
        .child(
            MenuPopup::new(
                "compose-more",
                tool("compose-more-button", "more_vert", "更多写信操作").on_click(cx.listener(
                    |this, event: &ClickEvent, _, cx| {
                        if matches!(event, ClickEvent::Keyboard(_)) {
                            this.more_open = !this.more_open;
                            cx.notify();
                        }
                    },
                )),
                self.more_menu(compact, cx),
            )
            .open(self.more_open)
            .close_on_activate(true)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.more_open = *open;
                cx.notify();
            })),
        )
        .child(div().flex_1())
        .child(
            MaterialButton::new("check-send")
                .variant(ButtonVariant::Filled)
                .icon(icon("send"))
                .child("发送")
                .button_size(ButtonSize::ExtraSmall)
                .disabled(
                    disabled
                        || self.importing
                        || self.account_scope.is_none()
                        || self
                            .send_tasks
                            .iter()
                            .any(|t| t.state != mail_core::SendState::Failed),
                )
                .on_click(cx.listener(|this, _, w, cx| this.check_send(w, cx))),
        )
        .child(
            tool(
                "close-compose",
                "close",
                if self.workspace_folder.is_some() {
                    "保存草稿并关闭编辑"
                } else {
                    "保存草稿并返回阅读"
                },
            )
            .button_size(ButtonSize::ExtraSmall)
            .disabled(self.closing)
            .on_click(cx.listener(|this, _, window, cx| {
                if this.workspace_folder.is_some() {
                    this.close_selection(window, cx);
                } else {
                    this.request_close(cx);
                }
            })),
        )
        .when(self.workspace_folder.is_some(), |row| {
            row.child(
                tool("delete-draft", "delete", "删除本地草稿")
                    .disabled(disabled || self.saving || self.importing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_delete = !this.confirm_delete;
                        cx.notify();
                    })),
            )
        })
    }
    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let disabled = self.edit_disabled() || self.saving || self.importing;
        let mut items = vec![
            MaterialMenuItem::new("新建草稿")
                .disabled(disabled)
                .on_click({
                    let entity = entity.clone();
                    move |_, w, cx| entity.update(cx, |this, cx| this.switch_draft(None, w, cx))
                }),
        ];
        for (index, draft) in self.drafts.iter().enumerate() {
            let entity = entity.clone();
            let title = if draft.document.headers.subject.is_empty() {
                "无主题"
            } else {
                &draft.document.headers.subject
            };
            let selected = self.stored.as_ref().is_some_and(|d| d.id == draft.id);
            items.push(
                MaterialMenuItem::new(format!("{}{}", if selected { "● " } else { "" }, title))
                    .disabled(disabled)
                    .on_click(move |_, w, cx| {
                        entity.update(cx, |this, cx| this.switch_draft(Some(index), w, cx));
                    }),
            );
        }
        let position = self
            .drafts
            .iter()
            .position(|d| {
                self.stored
                    .as_ref()
                    .is_some_and(|current| current.id == d.id)
            })
            .map_or(0, |i| i + 1);
        let revision = self.session.borrow().revision;
        let state = if self.loading {
            "正在恢复"
        } else if self.closing || self.saving {
            "正在保存"
        } else if self.auto_save_blocked {
            "保存失败 · 重试"
        } else if self.saved == Some(revision) {
            "已保存"
        } else {
            "等待保存"
        };
        let state = if self.workspace_folder.is_some() {
            format!("{state} · {}", self.current_sync_label())
        } else {
            state.to_owned()
        };
        let sender = self
            .account_scope
            .as_ref()
            .map(|(_, a)| a.as_str())
            .unwrap_or("本地草稿");
        let senders = crate::lifecycle::own_senders(cx)
            .into_iter()
            .map(|(account, address)| {
                let label = format!("打开 {address} 的草稿");
                MaterialMenuItem::new(label)
                    .disabled(disabled)
                    .on_click(move |_, _, cx| {
                        let account = account.clone();
                        let address = address.clone();
                        cx.defer(move |cx| open_account(account, address, cx));
                    })
            });
        div()
            .min_h(px(56.))
            .flex_none()
            .px_4()
            .py_2()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .border_t_1()
            .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
            .child(
                MenuPopup::new(
                    "compose-sender-popup",
                    quiet("compose-sender", &format!("发件人：{sender}"))
                        .icon(icon("expand_more"))
                        .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                            if matches!(event, ClickEvent::Keyboard(_)) {
                                this.sender_open = !this.sender_open;
                                cx.notify();
                            }
                        })),
                    MaterialMenu::new("compose-sender-menu").items(senders),
                )
                .open(self.sender_open)
                .close_on_activate(true)
                .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                    this.sender_open = *open;
                    cx.notify();
                })),
            )
            .child(div().flex_1())
            .when(self.workspace_folder.is_none(), |row| {
                row.child(
                    MenuPopup::new(
                        "compose-drafts-popup",
                        quiet(
                            "compose-drafts",
                            &format!("草稿 {position} / {}", self.drafts.len()),
                        )
                        .icon(icon("expand_more"))
                        .on_click(cx.listener(
                            |this, event: &ClickEvent, _, cx| {
                                if matches!(event, ClickEvent::Keyboard(_)) {
                                    this.drafts_open = !this.drafts_open;
                                    cx.notify();
                                }
                            },
                        )),
                        MaterialMenu::new("compose-drafts-menu").items(items),
                    )
                    .open(self.drafts_open)
                    .close_on_activate(true)
                    .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                        this.drafts_open = *open;
                        cx.notify();
                    })),
                )
            })
            .child(
                quiet("retry-local-save", &state)
                    .icon(icon(if self.auto_save_blocked {
                        "error_outline"
                    } else {
                        "cloud_done"
                    }))
                    .disabled(self.loading || self.saving || self.closing)
                    .on_click(cx.listener(|this, _, w, cx| {
                        if this.store.is_none() {
                            this.loading = true;
                            this.load_store(w, cx);
                        } else if !this.composing(w, cx) {
                            this.save(cx);
                        }
                    })),
            )
    }
}
impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Account for sidebar + draggable list, rather than the overall window width.
        let compact = if self.workspace_folder.is_some() {
            window.bounds().size.width.as_f32() - 216. - self.list_width - 5. < 590.
        } else {
            self.pane_width < 590.
        };
        let editor = div()
            .id("inline-composer")
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, w, cx| {
                let key = &event.keystroke;
                if key.key == "enter"
                    && !key.modifiers.control
                    && !key.modifiers.shift
                    && !key.modifiers.alt
                    && this.input.focus_handle(cx).is_focused(w)
                    && this.continue_block(w, cx)
                {
                    cx.stop_propagation();
                    w.prevent_default();
                    return;
                }
                if key.modifiers.control && !key.modifiers.alt && key.key == "enter" {
                    this.check_send(w, cx);
                    cx.stop_propagation();
                    w.prevent_default();
                }
                if key.modifiers.control && !key.modifiers.alt && key.key == "s" {
                    if !this.composing(w, cx) {
                        this.save(cx);
                    }
                    cx.stop_propagation();
                    w.prevent_default();
                }
            }))
            .child(TextSelectionLayer)
            .child(self.toolbar(compact, cx))
            .when(self.confirm_delete, |root| {
                root.child(motion::enter(
                    "draft-delete-enter",
                    div().flex_none().child(self.delete_confirmation(cx)),
                    cx,
                ))
            })
            .when(self.load_failed && !self.loading, |root| {
                root.child(
                    div()
                        .p_2()
                        .flex()
                        .gap_2()
                        .child(
                            quiet("save-raw-draft", "另存原始数据…")
                                .on_click(cx.listener(|this, _, _, cx| this.save_raw_draft(cx))),
                        )
                        .child(
                            quiet("discard-bad-draft", "放弃并新建草稿").on_click(
                                cx.listener(|this, _, w, cx| this.discard_bad_draft(w, cx)),
                            ),
                        ),
                )
            })
            .child(self.fields_toolbar(window, cx))
            .when(self.show_link, |root| {
                root.child(motion::enter(
                    "compose-link-enter",
                    div()
                        .px_4()
                        .py_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            TextField::new("link-url", &self.link)
                                .density(TextFieldDensity::Compact)
                                .placeholder("链接地址")
                                .disabled(self.edit_disabled())
                                .flex_1(),
                        )
                        .child(
                            quiet("apply-link", "应用链接")
                                .disabled(self.edit_disabled())
                                .on_click(cx.listener(|this, _, w, cx| {
                                    this.format("link", w, cx);
                                    this.show_link = false;
                                    cx.notify();
                                })),
                        ),
                    cx,
                ))
            })
            .when(
                self.send_review.is_some() || !self.send_tasks.is_empty(),
                |root| {
                    root.child(motion::enter(
                        "compose-send-enter",
                        div().flex_none().px_4().py_2().child(self.send_toolbar(cx)),
                        cx,
                    ))
                },
            )
            .child(
                div()
                    .id("rich-editor")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .px_5()
                    .py_4()
                    .text_size(px(18.))
                    .font_family(self.font_family.clone())
                    .capture_action(cx.listener(|this, _: &Undo, w, cx| {
                        this.history(false, w, cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &Redo, w, cx| {
                        this.history(true, w, cx);
                        cx.stop_propagation();
                    }))
                    .capture_key_down(cx.listener(Self::key))
                    .child(Editor::new(&self.input)),
            )
            .when(!self.attachment_ids.is_empty() || self.importing, |root| {
                root.child(motion::enter(
                    "compose-attachments-enter",
                    div()
                        .id("compose-attachments")
                        .max_h(px(120.))
                        .overflow_y_scroll()
                        .px_4()
                        .py_2()
                        .when(self.importing, |row| row.child("正在添加附件…"))
                        .child(self.attachment_bar(cx)),
                    cx,
                ))
            })
            .child(
                div()
                    .px_4()
                    .py_1()
                    .text_size(px(12.))
                    .text_color(theme::color(
                        if self.auto_save_blocked || self.close_error.is_some() {
                            ColorRole::ERROR
                        } else {
                            ColorRole::ON_SURFACE_VARIANT
                        },
                    ))
                    .child(self.status.clone()),
            )
            .child(self.footer(cx))
            .into_any_element();
        if self.workspace_folder.is_some() {
            self.workspace(editor, window, cx)
        } else {
            editor
        }
    }
}
