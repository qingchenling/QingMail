//! 账号草稿列表复用 Composer 的保存与切换流程，不另开编辑窗口。
use super::*;
use crate::ui::{icon, motion, quiet, tool};
use gpui_kit::{AnyElement, MouseButton, MouseDownEvent, MouseMoveEvent};
use mail_protocol::{Command, DraftSyncInfo, Reply};

pub(crate) fn sync_label(revision: u32, info: Option<&DraftSyncInfo>) -> &'static str {
    match info {
        Some(i) if matches!(i.state.as_str(), "conflict" | "uncertain") => "同步冲突",
        Some(i) if i.state == "synced" && i.base_revision == revision => "已同步",
        _ => "本地",
    }
}

impl Composer {
    pub(super) fn upsert_draft(&mut self, draft: StoredDraft) {
        if let Some(old) = self.drafts.iter_mut().find(|d| d.id == draft.id) {
            *old = draft;
        } else {
            self.drafts.insert(0, draft);
        }
    }
    pub(super) fn restore_draft(
        &mut self,
        draft: StoredDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.draft_list_generation += 1;
        *self.session.borrow_mut() = Session::new(draft.document.body.clone());
        self.upsert_draft(draft.clone());
        self.stored = Some(draft);
        self.restore_fields(window, cx);
        self.saved = Some(0);
        self.close_error = None;
        self.auto_save_blocked = false;
        self.load_failed = false;
        self.confirm_delete = false;
        self.sync_input(window, cx);
        self.input
            .update(cx, |input, cx| input.set_readonly(false, cx));
        self.status = "已打开草稿".into();
        self.refresh_send_tasks(cx);
    }
    pub(super) fn refresh_workspace(&mut self, cx: &mut Context<Self>) {
        if self.list_refreshing || self.loading || self.deleting {
            return;
        }
        let Some((account, _)) = self.account_scope.clone() else {
            return;
        };
        self.list_refreshing = true;
        self.last_list_refresh = Instant::now();
        let generation = self.draft_list_generation;
        let known: Vec<_> = self
            .drafts
            .iter()
            .map(|d| (d.id.clone(), d.revision))
            .collect();
        let store = crate::lifecycle::storage(cx);
        let task = cx.background_executor().spawn(async move {
            let sync = match store.control(Command::DraftSyncStatus {
                account: account.clone(),
            })? {
                Reply::DraftSyncStatus(v) => v,
                _ => return Err("草稿同步状态响应无效".to_owned()),
            };
            let changed = sync.iter().any(|s| {
                !known
                    .iter()
                    .any(|(id, revision)| id == &s.draft && *revision == s.revision)
            }) || (sync.len() < 200 && known.len() != sync.len());
            let drafts = if changed {
                Some(receive(store.account_drafts(account))?)
            } else {
                None
            };
            Ok::<_, String>((drafts, sync))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.list_refreshing = false;
                // A delete may have completed after this read began. Refresh again;
                // never resurrect its stale list row or select it implicitly.
                if this.deleting || this.loading || this.draft_list_generation != generation {
                    return;
                }
                match result {
                    Ok((drafts, sync)) => {
                        if let Some(drafts) = drafts {
                            this.drafts = drafts;
                        }
                        if let Some(draft) = this.stored.clone()
                            && this
                                .drafts
                                .iter()
                                .any(|d| d.id == draft.id && d.revision <= draft.revision)
                        {
                            this.upsert_draft(draft);
                        }
                        this.sync_items = sync;
                    }
                    Err(e) => this.status = format!("列表更新失败，当前编辑内容保留：{e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn current_sync_label(&self) -> &'static str {
        let Some(d) = &self.stored else {
            return "本地";
        };
        let info = self.sync_items.iter().find(|s| s.draft == d.id);
        if info.is_some_and(|s| matches!(s.state.as_str(), "conflict" | "uncertain")) {
            return "同步冲突";
        }
        if self.saved != Some(self.session.borrow().revision) {
            return "本地";
        }
        sync_label(d.revision, info)
    }
    fn show_details(&mut self, cx: &mut Context<Self>) {
        if self.server_details.is_none()
            && let (Some((account, _)), Some(folder)) =
                (&self.account_scope, &self.workspace_folder)
        {
            let account = account.clone();
            let folder = folder.clone();
            self.server_details =
                Some(cx.new(|cx| crate::server_drafts::ServerDrafts::new_for(account, folder, cx)));
        }
        self.show_server_details = true;
        self.list_visible = false;
        cx.notify();
    }
    pub(super) fn close_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.saving || self.deleting || self.importing || self.closing {
            return;
        }
        if self.composing(window, cx) {
            self.status = "请先完成或取消中文候选词，再关闭编辑".into();
            cx.notify();
            return;
        }
        self.deselect_requested = true;
        self.pending_draft = None;
        self.save(cx);
        cx.notify();
    }
    pub(super) fn clear_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft_list_generation += 1;
        self.stored = None;
        self.saved = Some(0);
        *self.session.borrow_mut() = Session::new(Draft::plain("").expect("empty"));
        self.header_values = Default::default();
        for input in &self.header_inputs {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.attachment_ids.clear();
        self.attachment_infos.clear();
        self.send_tasks.clear();
        self.send_tasks_generation += 1;
        self.send_review = None;
        self.confirm_delete = false;
        self.auto_save_blocked = false;
        self.close_error = None;
        self.sync_input(window, cx);
        self.list_visible = true;
    }
    pub(super) fn delete_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading
            || self.saving
            || self.deleting
            || self.importing
            || self.closing
            || self.send_locked()
            || self.composing(window, cx)
        {
            return;
        }
        let (Some(draft), Some(store)) = (self.stored.clone(), self.store.clone()) else {
            return;
        };
        self.confirm_delete = false;
        self.deleting = true;
        self.draft_list_generation += 1;
        self.loading = true;
        self.last_list_refresh = Instant::now();
        self.pending_draft = None;
        self.input
            .update(cx, |input, cx| input.set_readonly(true, cx));
        let deleted = draft.id.clone();
        let task = cx.background_executor().spawn(async move {
            match store.control(Command::DiscardDraft {
                account: draft.account,
                draft: draft.id,
                revision: draft.revision,
            })? {
                Reply::Acknowledged => Ok(()),
                _ => Err("删除响应无效，请刷新列表确认结果".to_owned()),
            }
        });
        cx.spawn_in(window, async move |entity, cx| {
            let result = task.await;
            let _ = entity.update_in(cx, |this, window, cx| {
                this.deleting = false;
                this.draft_list_generation += 1;
                this.loading = false;
                match result {
                    Ok(()) => {
                        this.drafts.retain(|d| d.id != deleted);
                        this.clear_selection(window, cx);
                        this.status = "本地草稿已删除；服务器副本和发送记录保留".into();
                        this.list_visible = true;
                        // Do not create another empty draft after deleting the last one.
                        if let Some(next) = this.drafts.first() {
                            this.switch_draft_id(Some(next.id.clone()), window, cx);
                        }
                    }
                    Err(e) => {
                        this.input
                            .update(cx, |input, cx| input.set_readonly(false, cx));
                        this.status = format!("删除未确认，当前编辑内容保留：{e}");
                    }
                }
                this.last_list_refresh = Instant::now() - Duration::from_secs(3);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn workspace_list(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let menu = MaterialMenu::new("draft-list-menu").items([
            MaterialMenuItem::new("刷新草稿列表")
                .disabled(self.list_refreshing)
                .on_click({
                    let entity = entity.clone();
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            this.refresh_drafts(cx);
                            this.refresh_workspace(cx);
                        })
                    }
                }),
            MaterialMenuItem::new("服务器原件与同步冲突…")
                .on_click(move |_, _, cx| entity.update(cx, |this, cx| this.show_details(cx))),
        ]);
        let filter = self.draft_search.read(cx).value().to_lowercase();
        let mut drafts: Vec<_> = self
            .drafts
            .iter()
            .filter(|d| {
                let h = &d.document.headers;
                filter.is_empty()
                    || h.subject.to_lowercase().contains(&filter)
                    || h.to.join(" ").to_lowercase().contains(&filter)
                    || d.document.body.text().to_lowercase().contains(&filter)
            })
            .collect();
        let updated = |d: &StoredDraft| {
            self.sync_items
                .iter()
                .find(|s| s.draft == d.id)
                .map_or(0, |s| s.updated_at)
        };
        drafts.sort_by(|a, b| {
            updated(b)
                .cmp(&updated(a))
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        let mut list = div()
            .id("saved-drafts-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        for draft in &drafts {
            let id = draft.id.clone();
            let selected = self.stored.as_ref().is_some_and(|d| d.id == id);
            let info = self.sync_items.iter().find(|s| s.draft == id);
            let status = if selected {
                self.current_sync_label()
            } else {
                sync_label(draft.revision, info)
            };
            let time = info
                .filter(|i| i.updated_at > 0)
                .map(|i| {
                    let time = mail_service::sync::display_time(i.updated_at);
                    format!("{} UTC", time.get(5..16).unwrap_or(&time).replace('T', " "))
                })
                .unwrap_or_default();
            let h = &draft.document.headers;
            let background = motion::color(
                format!("draft-selection-{}", id.0),
                if selected {
                    ColorRole::SECONDARY_CONTAINER
                } else {
                    ColorRole::SURFACE
                },
                window,
                cx,
            );
            list = list.child(
                div()
                    .id(format!("draft-row-{}", id.0))
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .bg(background)
                    .border_l_2()
                    .border_color(theme::color(if selected {
                        ColorRole::PRIMARY
                    } else {
                        ColorRole::SURFACE
                    }))
                    .border_b_1()
                    .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(16.))
                                    .font_weight(FontWeight::BOLD)
                                    .child(if h.subject.is_empty() {
                                        "无主题".to_owned()
                                    } else {
                                        h.subject.clone()
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                                    .child(time),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                            .child(if h.to.is_empty() {
                                "尚未填写收件人".into()
                            } else {
                                format!("致：{}", h.to.join("，"))
                            }),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                            .child(
                                draft
                                    .document
                                    .body
                                    .text()
                                    .lines()
                                    .find(|s| !s.trim().is_empty())
                                    .unwrap_or("暂无正文")
                                    .to_owned(),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_size(px(11.))
                            .text_color(theme::color(if status == "同步冲突" {
                                ColorRole::ERROR
                            } else {
                                ColorRole::ON_SURFACE_VARIANT
                            }))
                            .child(
                                icon(match status {
                                    "已同步" => "check_circle",
                                    "同步冲突" => "warning",
                                    _ => "draft",
                                })
                                .size(px(16.)),
                            )
                            .child(status),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.show_server_details = false;
                        this.list_visible = false;
                        if this.stored.as_ref().is_none_or(|d| d.id != id) {
                            this.switch_draft_id(Some(id.clone()), window, cx);
                        }
                        cx.notify();
                    })),
            );
        }
        if drafts.is_empty() {
            list = list.child(
                div()
                    .p_5()
                    .text_sm()
                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child(if self.loading {
                        "正在读取草稿…"
                    } else if filter.is_empty() {
                        "暂无草稿，点击左侧写邮件开始"
                    } else {
                        "没有匹配的草稿"
                    }),
            );
        }
        div()
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(64.))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(22.))
                            .font_weight(FontWeight::BOLD)
                            .child("草稿"),
                    )
                    .child(div().text_sm().child(self.drafts.len().to_string()))
                    .child(div().flex_1())
                    .child(MenuPopup::new(
                        "draft-list-more",
                        tool("draft-more", "more_vert", "更多草稿操作"),
                        menu,
                    )),
            )
            .child(
                div()
                    .px_4()
                    .pb_2()
                    .text_size(px(11.))
                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child("按最近编辑排序"),
            )
            .child(list)
            .child(
                div()
                    .p_3()
                    .text_size(px(11.))
                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child(format!("{} 份草稿", drafts.len())),
            )
    }
    pub(super) fn workspace(
        &mut self,
        editor: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wide = window.bounds().size.width >= px(1050.);
        let available = window.bounds().size.width.as_f32() - 216.;
        self.list_width = self.list_width.clamp(300., (available - 420.).max(300.));
        self.pane_width = if wide {
            available - self.list_width - 5.
        } else {
            available
        };
        let mut detail = div().size_full().min_w_0().min_h_0().flex().flex_col();
        if !wide || self.show_server_details {
            detail = detail.child(
                quiet(
                    "draft-back-to-list",
                    if self.show_server_details {
                        "返回草稿编辑"
                    } else {
                        "返回草稿列表"
                    },
                )
                .icon(icon("arrow_back"))
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.show_server_details {
                        this.show_server_details = false;
                    } else {
                        this.list_visible = true;
                    }
                    cx.notify();
                })),
            );
        }
        if self.show_server_details {
            if let Some(details) = &self.server_details {
                detail = detail.child(div().flex_1().min_h_0().child(details.clone()));
            }
        } else if self.stored.is_some() || self.load_failed {
            detail = detail.child(motion::enter(
                format!(
                    "draft-editor-{}",
                    self.stored.as_ref().map_or("unsaved", |d| d.id.0.as_str())
                ),
                div().flex_1().min_h_0().child(editor),
                cx,
            ));
        } else {
            detail = detail.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .gap_3()
                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child(icon("edit_note").size(px(40.)))
                    .child(if self.loading {
                        "正在读取草稿…"
                    } else {
                        "选择一份草稿，或点击左侧写邮件"
                    })
                    .child(div().px_4().text_sm().child(self.status.clone())),
            );
        }
        let mut body = div().flex_1().min_h_0().flex();
        if wide {
            body = body
                .child(
                    div()
                        .w(px(self.list_width))
                        .flex_none()
                        .child(self.workspace_list(window, cx)),
                )
                .child(
                    div()
                        .id("draft-pane-divider")
                        .w(px(5.))
                        .h_full()
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
                        .child(detail),
                );
        } else if self.list_visible {
            body = body.child(div().flex_1().child(self.workspace_list(window, cx)));
        } else {
            body = body.child(detail);
        }
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if event.pressed_button != Some(MouseButton::Left) {
                    this.resize_start = None;
                }
                if let Some((x, width)) = this.resize_start {
                    this.list_width = (width + event.position.x.as_f32() - x).clamp(
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
            .child(
                div()
                    .h(px(72.))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                    .child(div().w(px(190.)).truncate().text_sm().child(format!(
                            "{} / 草稿",
                            self.account_scope
                                .as_ref()
                                .map_or("邮箱", |(_, a)| a.as_str())
                        )))
                    .child(
                        TextField::new("draft-search", &self.draft_search)
                            .outlined()
                            .density(TextFieldDensity::Compact)
                            .leading_icon(icon("search"))
                            .placeholder("搜索草稿")
                            .flex_1()
                            .max_w(px(420.)),
                    )
                    .child(div().flex_1())
                    .child(
                        tool("draft-accounts", "manage_accounts", "邮箱账号").on_click(
                            |_, _, cx| {
                                crate::lifecycle::open_page(crate::mailbox::Page::Accounts, cx)
                            },
                        ),
                    )
                    .child(
                        tool("draft-settings", "settings", "设置").on_click(|_, _, cx| {
                            crate::lifecycle::open_page(crate::mailbox::Page::Settings, cx)
                        }),
                    )
                    .child(
                        tool("draft-refresh", "sync", "刷新草稿列表")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_workspace(cx))),
                    ),
            )
            .child(body)
            .into_any_element()
    }
    pub(super) fn delete_confirmation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_4()
            .py_2()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .bg(theme::color(ColorRole::ERROR_CONTAINER))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .child("删除这份本地草稿及未保存修改？服务器副本和已确认的发送记录保留。"),
            )
            .child(
                quiet("confirm-delete-draft", "删除本地草稿")
                    .disabled(self.edit_disabled() || self.saving)
                    .on_click(cx.listener(|this, _, w, cx| this.delete_draft(w, cx))),
            )
            .child(
                quiet("cancel-delete-draft", "取消").on_click(cx.listener(|this, _, _, cx| {
                    this.confirm_delete = false;
                    cx.notify();
                })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn draft_status_never_reports_an_unconfirmed_revision_as_synced() {
        let mut info = DraftSyncInfo {
            updated_at: 0,
            draft: mail_core::DraftId("a".repeat(32)),
            subject: String::new(),
            revision: 2,
            base_revision: 1,
            state: "synced".into(),
            detail: String::new(),
            source: None,
        };
        assert_eq!(sync_label(2, Some(&info)), "本地");
        info.base_revision = 2;
        assert_eq!(sync_label(2, Some(&info)), "已同步");
        for state in ["pending", "failed", "stopped"] {
            info.state = state.into();
            assert_eq!(sync_label(2, Some(&info)), "本地");
        }
        for state in ["conflict", "uncertain"] {
            info.state = state.into();
            assert_eq!(sync_label(2, Some(&info)), "同步冲突");
        }
        assert_eq!(sync_label(2, None), "本地");
    }
}
