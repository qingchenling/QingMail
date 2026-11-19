//! 服务器草稿原件入口；只在明确确认后转换编辑副本，不覆盖服务器。
use crate::ui::{icon, quiet, tool};
use gpui_kit::{AnyElement, Context, Render, Window, div, prelude::*};
use gpui_kit_material::{prelude::*, theme};
use mail_core::{AccountId, AccountSettings, RemoteFolder};
use mail_daemon::storage::StorageClient;
use mail_protocol::{
    BusinessOperation, Command, DraftConflict, DraftResolution, Reply, ServerDraftSource,
};
pub(super) struct ServerDrafts {
    store: StorageClient,
    accounts: Vec<(AccountId, AccountSettings)>,
    account: Option<AccountId>,
    folder: Option<String>,
    choices: Vec<RemoteFolder>,
    epoch: u32,
    uids: Vec<u32>,
    originals: Vec<ServerDraftSource>,
    sync_items: Vec<mail_protocol::DraftSyncInfo>,
    conflicts: Vec<DraftConflict>,
    busy: bool,
    confirm: Option<String>,
    choosing_folder: bool,
    advanced: bool,
    selected_conflict: Option<String>,
    preferred_draft: Option<mail_core::DraftId>,
    status: String,
    notice: String,
}
struct Loaded {
    accounts: Vec<(AccountId, AccountSettings)>,
    account: Option<AccountId>,
    folder: Option<String>,
    choices: Vec<RemoteFolder>,
    originals: Vec<ServerDraftSource>,
    sync_items: Vec<mail_protocol::DraftSyncInfo>,
    conflicts: Vec<DraftConflict>,
    index: Option<(u32, Vec<u32>, bool)>,
    notice: String,
}
impl ServerDrafts {
    pub(super) fn new_for(
        account: AccountId,
        folder: String,
        preferred_draft: Option<mail_core::DraftId>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            store: crate::lifecycle::storage(cx),
            accounts: vec![],
            account: Some(account.clone()),
            folder: Some(folder.clone()),
            choices: vec![],
            epoch: 0,
            uids: vec![],
            originals: vec![],
            sync_items: vec![],
            conflicts: vec![],
            busy: false,
            confirm: None,
            choosing_folder: false,
            advanced: false,
            selected_conflict: None,
            preferred_draft,
            status: String::new(),
            notice: String::new(),
        };
        this.load(None, false, cx);
        cx.spawn(async move |entity, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                if entity
                    .update(cx, |this, cx| this.load(None, false, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        this
    }
    fn load(&mut self, command: Option<Command>, scan: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        if command.is_some() || scan {
            self.status.clear();
        }
        let store = self.store.clone();
        let selected = self.account.clone();
        let task = cx.background_executor().spawn(async move {
            if let Some(command) = command {
                store.control(command)?;
            }
            let accounts = match store.control(Command::Accounts)? {
                Reply::Accounts(v) => v,
                _ => return Err("账号响应无效".to_string()),
            };
            let account = selected
                .filter(|a| accounts.iter().any(|(id, _)| id == a))
                .or_else(|| accounts.first().map(|(id, _)| id.clone()));
            let mut result = Loaded {
                accounts,
                account: account.clone(),
                folder: None,
                choices: vec![],
                originals: vec![],
                sync_items: vec![],
                conflicts: vec![],
                index: None,
                notice: String::new(),
            };
            if let Some(account) = account {
                result.choices = match store.control(Command::Catalog {
                    account: account.clone(),
                })? {
                    Reply::Catalog { folders, .. } => folders,
                    _ => vec![],
                };
                if scan {
                    let client = store.ipc_client().ok_or("后台连接不可用")?;
                    match mail_daemon::client::run(
                        &client,
                        BusinessOperation::ScanServerDrafts {
                            account: account.clone(),
                        },
                    )? {
                        Reply::ServerDraftIndex {
                            folder,
                            choices,
                            uidvalidity,
                            uids,
                            has_more,
                        } => {
                            result.folder = folder;
                            result.choices = choices;
                            result.index = Some((uidvalidity, uids, has_more));
                        }
                        _ => return Err("扫描响应无效".into()),
                    }
                }
                result.sync_items = match store.control(Command::DraftSyncStatus {
                    account: account.clone(),
                })? {
                    Reply::DraftSyncStatus(items) => items,
                    _ => vec![],
                };
                result.conflicts = match store.control(Command::DraftConflicts {
                    account: Some(account.clone()),
                })? {
                    Reply::DraftConflicts(items) => items,
                    _ => return Err("冲突列表响应无效".into()),
                };
                match store.control(Command::ServerDrafts { account })? {
                    Reply::ServerDrafts {
                        folder,
                        items,
                        notice,
                    } => {
                        result.folder = folder;
                        result.originals = items;
                        result.notice = notice;
                    }
                    _ => return Err("原件列表响应无效".into()),
                }
            }
            Ok::<_, String>(result)
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(result) => {
                        this.accounts = result.accounts;
                        this.account = result.account;
                        this.folder = result.folder;
                        this.choices = result.choices;
                        this.originals = result.originals;
                        this.sync_items = result.sync_items;
                        this.conflicts = result.conflicts;
                        this.notice = result.notice;
                        if let Some((epoch, uids, more)) = result.index {
                            this.epoch = epoch;
                            this.uids = uids;
                            this.status = if more {
                                "仅显示最新200个UID，旧草稿仍在服务器；不会推断远端删除".into()
                            } else {
                                "扫描完成；点击保留原件后可离线查看，转换副本需明确确认".into()
                            };
                        }
                        if this
                            .selected_conflict
                            .as_ref()
                            .is_none_or(|id| !this.conflicts.iter().any(|c| &c.id == id))
                        {
                            let preferred = this.preferred_draft.take();
                            this.selected_conflict = this
                                .conflicts
                                .iter()
                                .find(|c| Some(&c.local) == preferred.as_ref())
                                .or(this.conflicts.first())
                                .map(|c| c.id.clone());
                        }
                    }
                    Err(e) => this.status = e,
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn resolve_conflict(
        &mut self,
        id: String,
        resolution: DraftResolution,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        if self.busy {
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let key = format!("{id}-{resolution:?}");
        if self.confirm.as_ref() != Some(&key) {
            self.confirm = Some(key);
            self.status.clear();
            cx.notify();
            return;
        }
        self.confirm = None;
        self.load(
            Some(Command::ResolveDraftConflict {
                account,
                conflict: id,
                resolution,
            }),
            false,
            cx,
        );
    }
    fn delete_conflict(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(conflict) = self.conflicts.iter().find(|c| c.id == id) else {
            return;
        };
        let Some(info) = self.sync_items.iter().find(|i| i.draft == conflict.local) else {
            return;
        };
        let key = format!("delete-{id}");
        if self.confirm.as_ref() != Some(&key) {
            self.confirm = Some(key);
            cx.notify();
            return;
        }
        let command = Command::DiscardDraft {
            account: conflict.account.clone(),
            draft: conflict.local.clone(),
            revision: info.revision,
        };
        self.confirm = None;
        self.load(Some(command), false, cx);
    }

    fn unconfirmed(&self, conflict: &DraftConflict) -> bool {
        self.sync_items
            .iter()
            .any(|i| i.draft == conflict.local && i.state == "uncertain")
    }

    fn open_local(&mut self, draft: mail_core::DraftId, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some((account, settings)) = self
            .accounts
            .iter()
            .find(|(id, _)| Some(id) == self.account.as_ref())
            .cloned()
        else {
            return;
        };
        self.busy = true;
        let store = self.store.clone();
        let selected = account.clone();
        let task = cx.background_executor().spawn(async move {
            store.control(Command::SelectDraft {
                account: Some(selected),
                draft,
            })
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(_) => crate::composer::open_saved(account, settings.address, cx),
                    Err(e) => this.status = e,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn original_action(&mut self, source: String, convert: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let converted = self.originals.iter().any(|s| {
            s.id == source
                && s.converted
                    .as_ref()
                    .is_some_and(|d| self.sync_items.iter().any(|i| &i.draft == d))
        });
        if convert && !converted && self.confirm.as_ref() != Some(&source) {
            self.confirm = Some(source);
            self.status="转换会以清理后的纯文本创建本地副本，HTML排版可能丢失；附件保留，原件和服务器不变。再次点击确认转换。".into();
            cx.notify();
            return;
        }
        self.confirm = None;
        self.busy = true;
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            if convert {
                let draft = match store.control(Command::ConvertServerDraft {
                    account: account.clone(),
                    source,
                })? {
                    Reply::Draft(d) => d,
                    _ => return Err("转换响应无效".to_string()),
                };
                store.control(Command::SelectDraft {
                    account: Some(account),
                    draft: draft.id,
                })?;
                Ok(None)
            } else {
                let client = store.ipc_client().ok_or("后台连接不可用")?;
                mail_daemon::client::content(
                    &client,
                    BusinessOperation::ReadServerDraft { account, source },
                )
                .map(Some)
            }
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(Some(content)) => crate::reader::open_content(content, None, cx),
                    Ok(None) => {
                        this.load(None, false, cx);
                        if let Some((id, settings)) = this
                            .accounts
                            .iter()
                            .find(|(id, _)| Some(id) == this.account.as_ref())
                        {
                            crate::composer::open_saved(id.clone(), settings.address.clone(), cx);
                        }
                    }
                    Err(e) => this.status = e,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn import(&mut self, uid: u32, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (Some(account), Some(folder)) = (self.account.clone(), self.folder.clone()) else {
            return;
        };
        self.busy = true;
        let epoch = self.epoch;
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let client = store.ipc_client().ok_or("后台连接不可用")?;
            match mail_daemon::client::run(
                &client,
                BusinessOperation::ImportServerDraft {
                    account,
                    folder,
                    uidvalidity: epoch,
                    uid,
                },
            )? {
                Reply::ServerDraftSource(_) => Ok(()),
                _ => Err("原件保存响应无效".to_string()),
            }
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => this.load(None, false, cx),
                    Err(e) => this.status = format!("原件未导入：{e}；服务器内容保持原样"),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
impl ServerDrafts {
    fn conflict_card(&self, conflict: &DraftConflict, cx: &mut Context<Self>) -> AnyElement {
        let unknown = self.unconfirmed(conflict);
        let info = self.sync_items.iter().find(|i| i.draft == conflict.local);
        let source = conflict
            .original
            .as_ref()
            .and_then(|id| self.originals.iter().find(|s| &s.id == id));
        let subject = info
            .map(|i| i.subject.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("无主题");
        let local = conflict.local.clone();
        let mut versions = div().flex().flex_wrap().gap_3().child(
            div()
                .flex_1()
                .min_w(gpui_kit::px(160.))
                .p_4()
                .rounded_xl()
                .bg(theme::color(ColorRole::SURFACE_CONTAINER))
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(icon("computer"))
                        .child("本地版本"),
                )
                .child(div().text_sm().child(subject.to_owned()))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                        .child("已保存在这台电脑上"),
                )
                .child(
                    quiet(format!("local-{}", conflict.id), "查看内容")
                        .disabled(self.busy)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.open_local(local.clone(), cx)),
                        ),
                ),
        );
        let mut remote = div()
            .flex_1()
            .min_w(gpui_kit::px(160.))
            .p_4()
            .rounded_xl()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon("cloud"))
                    .child("云端版本"),
            )
            .child(div().text_sm().child(if unknown {
                "正在确认是否上传成功".to_owned()
            } else {
                source
                    .map(|s| s.subject.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("未找到可用的云端版本".into())
            }));
        if let Some(id) = conflict.original.clone() {
            remote = remote.child(
                quiet(format!("remote-{}", conflict.id), "查看已保留内容")
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.original_action(id.clone(), false, cx)
                    })),
            );
        }
        versions = versions.child(remote);
        let mut actions = div().flex().flex_wrap().items_center().gap_2();
        if unknown {
            let account = conflict.account.clone();
            actions = actions.child(
                MaterialButton::new(format!("retry-{}", conflict.id))
                    .variant(ButtonVariant::Tonal)
                    .disabled(self.busy)
                    .child("继续核验")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm = None;
                        this.load(
                            Some(Command::FlushDraftSync {
                                account: account.clone(),
                            }),
                            false,
                            cx,
                        );
                    })),
            );
        }
        let resolutions: Vec<_> = if unknown {
            vec![("仅保留本地", DraftResolution::KeepLocal)]
        } else {
            vec![
                ("保留两份", DraftResolution::KeepBoth),
                ("使用本地", DraftResolution::KeepLocal),
                ("使用云端", DraftResolution::KeepRemote),
            ]
        };
        let mut confirmation = None;
        for (label, resolution) in resolutions {
            let id = conflict.id.clone();
            let key = format!("{id}-{resolution:?}");
            let confirming = self.confirm.as_ref() == Some(&key);
            if confirming {
                confirmation = Some(if unknown {
                    "保留当前本地草稿并暂停云端同步，不额外创建副本。"
                } else {
                    match resolution {
                        DraftResolution::KeepBoth => "保留两边内容，本地将作为独立版本继续同步。",
                        DraftResolution::KeepLocal => {
                            "使用本地内容继续同步，云端原始版本保留供恢复。"
                        }
                        DraftResolution::KeepRemote => {
                            "打开云端内容的编辑副本，当前本地内容仍保留。"
                        }
                    }
                });
            }
            actions = actions.child(
                MaterialButton::new(key)
                    .button_size(ButtonSize::Small)
                    .variant(
                        if !unknown && matches!(resolution, DraftResolution::KeepBoth) {
                            ButtonVariant::Tonal
                        } else {
                            ButtonVariant::Text
                        },
                    )
                    .disabled(
                        self.busy
                            || (matches!(resolution, DraftResolution::KeepRemote)
                                && conflict.original.is_none()),
                    )
                    .child(if confirming {
                        format!("确认{label}")
                    } else {
                        label.into()
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.resolve_conflict(id.clone(), resolution, cx)
                    })),
            );
        }
        let id = conflict.id.clone();
        let deleting = self.confirm.as_deref() == Some(&format!("delete-{id}"));
        if deleting {
            confirmation = Some("删除本地草稿；云端副本由后台核验后清理，不必先解决冲突。");
        }
        actions = actions.child(div().flex_1()).child(
            tool(format!("delete-{id}"), "delete", "删除草稿")
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _, _, cx| this.delete_conflict(&id, cx))),
        );
        let mut card = div().p_4().rounded_xl().flex().flex_col().gap_4()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
            .child(div().flex().items_center().gap_2().child(icon("sync_problem"))
                .child(if unknown {"上传结果待确认"} else {"草稿版本冲突"}))
            .child(div().text_lg().child(subject.to_owned()))
            .child(div().text_sm().text_color(theme::color(ColorRole::ON_SURFACE_VARIANT)).child(if unknown {
                "连接中断后，尚未确定云端是否收到。你的本地内容已保存，可以继续核验，也可以只保留本地。"
            } else {"本地和云端发生了不同修改。选择要继续使用的版本，已有内容不会被静默覆盖。"}))
            .child(versions).child(actions);
        if let Some(message) = confirmation {
            let id = conflict.id.clone();
            let mut bar = div()
                .p_3()
                .rounded_xl()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .bg(theme::color(ColorRole::SECONDARY_CONTAINER))
                .child(div().flex_1().min_w_0().text_sm().child(message));
            if deleting {
                bar = bar.child(
                    quiet(format!("confirm-delete-{id}"), "确认删除")
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_conflict(&id, cx))),
                );
            }
            card = card.child(bar.child(
                quiet(format!("cancel-{}", conflict.id), "取消").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.confirm = None;
                        cx.notify();
                    },
                )),
            ));
        }
        card.into_any_element()
    }
}

impl Render for ServerDrafts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("draft-conflict-content")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_4();
        if self.conflicts.is_empty() {
            list = list.child(
                div()
                    .p_6()
                    .rounded_xl()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
                    .child(icon("check_circle").size(gpui_kit::px(32.)))
                    .child("没有需要处理的草稿冲突")
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                            .child("返回草稿继续编辑；后台清理进度可在右下角查看。"),
                    ),
            );
        } else {
            if self.conflicts.len() > 1 {
                let mut tabs = div().flex().flex_wrap().gap_2();
                for conflict in &self.conflicts {
                    let id = conflict.id.clone();
                    let subject = self
                        .sync_items
                        .iter()
                        .find(|i| i.draft == conflict.local)
                        .map(|i| i.subject.clone())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("无主题".into());
                    tabs = tabs.child(
                        MaterialButton::new(format!("pick-{id}"))
                            .button_size(ButtonSize::Small)
                            .variant(if self.selected_conflict.as_ref() == Some(&id) {
                                ButtonVariant::Tonal
                            } else {
                                ButtonVariant::Text
                            })
                            .child(subject)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_conflict = Some(id.clone());
                                this.confirm = None;
                                cx.notify();
                            })),
                    );
                }
                list = list.child(tabs);
            }
            if let Some(conflict) = self
                .conflicts
                .iter()
                .find(|c| Some(&c.id) == self.selected_conflict.as_ref())
                .or(self.conflicts.first())
            {
                list = list.child(self.conflict_card(conflict, cx));
            }
        }
        list = list.child(
            quiet(
                "draft-advanced",
                if self.advanced {
                    "收起目录与历史原件"
                } else {
                    "目录与历史原件"
                },
            )
            .icon(icon(if self.advanced {
                "expand_less"
            } else {
                "expand_more"
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.advanced = !this.advanced;
                cx.notify();
            })),
        );
        if self.advanced {
            let mut advanced = div()
                .p_4()
                .rounded_xl()
                .flex()
                .flex_col()
                .gap_3()
                .bg(theme::color(ColorRole::SURFACE_CONTAINER_LOW))
                .child(div().text_sm().child(format!(
                    "云端草稿目录：{}",
                    self.folder.as_deref().unwrap_or("未选择")
                )));
            advanced = advanced.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        quiet("draft-change-folder", "更改目录")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choosing_folder = !this.choosing_folder;
                                cx.notify();
                            })),
                    )
                    .child(
                        quiet("draft-scan", "扫描服务器")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.load(None, true, cx))),
                    ),
            );
            if self.choosing_folder
                && let Some(account) = self.account.clone()
            {
                for folder in self.choices.iter().filter(|f| f.selectable) {
                    let name = folder.name.clone();
                    let a = account.clone();
                    advanced = advanced.child(
                        quiet(format!("folder-{name}"), &folder.display_name)
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.choosing_folder = false;
                                this.load(
                                    Some(Command::DraftSyncFolder {
                                        account: a.clone(),
                                        folder: name.clone(),
                                    }),
                                    true,
                                    cx,
                                );
                            })),
                    );
                }
            }
            for conflict in &self.conflicts {
                advanced = advanced.child(
                    div()
                        .text_sm()
                        .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                        .child(conflict.reason.clone()),
                );
            }
            if !self.notice.is_empty() {
                advanced = advanced.child(div().text_sm().child(self.notice.clone()));
            }
            advanced = advanced.child(
                div()
                    .text_sm()
                    .child("历史原件用于恢复，不代表仍在服务器上的草稿。"),
            );
            for source in &self.originals {
                let id = source.id.clone();
                let convert = id.clone();
                advanced = advanced.child(
                    div()
                        .py_2()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(if source.subject.is_empty() {
                            "无主题".to_owned()
                        } else {
                            source.subject.clone()
                        })
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    quiet(format!("view-{id}"), "查看原件")
                                        .disabled(self.busy)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.original_action(id.clone(), false, cx)
                                        })),
                                )
                                .child(
                                    quiet(
                                        format!("convert-{convert}"),
                                        if self.confirm.as_ref() == Some(&convert) {
                                            "确认另存副本"
                                        } else {
                                            "另存编辑副本"
                                        },
                                    )
                                    .disabled(self.busy)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.original_action(convert.clone(), true, cx)
                                        },
                                    )),
                                ),
                        ),
                );
            }
            for &uid in &self.uids {
                advanced = advanced.child(
                    quiet(format!("import-{uid}"), &format!("读取服务器原件 {uid}"))
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, _, cx| this.import(uid, cx))),
                );
            }
            list = list.child(advanced);
        }
        let mut page = div()
            .size_full()
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
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
                            .gap_1()
                            .child(div().text_xl().child("草稿同步"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                                    .child(
                                        self.accounts
                                            .iter()
                                            .find(|(id, _)| Some(id) == self.account.as_ref())
                                            .map(|(_, s)| s.address.clone())
                                            .unwrap_or_default(),
                                    ),
                            ),
                    )
                    .child(
                        tool("reload-draft-conflicts", "refresh", "刷新")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.load(None, false, cx))),
                    ),
            );
        if !self.status.is_empty() {
            page = page.child(
                div()
                    .p_3()
                    .rounded_xl()
                    .text_sm()
                    .bg(theme::color(ColorRole::SECONDARY_CONTAINER))
                    .child(self.status.clone()),
            );
        }
        page.child(list)
    }
}
