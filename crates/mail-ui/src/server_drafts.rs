//! 服务器草稿原件入口；只在明确确认后转换编辑副本，不覆盖服务器。
use gpui_kit::{Context, Render, Window, div, prelude::*};
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
    pub(super) fn new_for(account: AccountId, folder: String, cx: &mut Context<Self>) -> Self {
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
            status: "正在读取…".into(),
            notice: String::new(),
        };
        this.load(Some(Command::DraftSyncFolder { account, folder }), true, cx);
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
        cx.spawn(async move |entity,cx|{
            let result=task.await;
            let _=entity.update(cx,|this,cx|{this.busy=false;match result {
                Ok(result)=>{
                    this.accounts=result.accounts;this.account=result.account;this.folder=result.folder;this.choices=result.choices;this.originals=result.originals;this.sync_items=result.sync_items;this.conflicts=result.conflicts;this.notice=result.notice;
                    if let Some((epoch,uids,more))=result.index {
                        this.epoch=epoch;this.uids=uids;this.status=if more{"仅显示最新200个UID，旧草稿仍在服务器；不会推断远端删除".into()}else{"扫描完成；点击保留原件后可离线查看，转换副本需明确确认".into()};
                    } else {this.status="编辑停止约5秒后上传新版本，关窗会合并调度。结果不明只查证；旧服务器版本保留。".into();}
                },Err(e)=>this.status=e};cx.notify();});
        }).detach();
        cx.notify();
    }
    fn resolve_conflict(
        &mut self,
        id: String,
        resolution: DraftResolution,
        cx: &mut Context<Self>,
    ) {
        let Some(account) = self.account.clone() else {
            return;
        };
        let key = format!("{id}-{resolution:?}");
        if self.confirm.as_ref() != Some(&key) {
            self.confirm = Some(key);
            self.status="再次点击确认：所有选择保留原件及本地内容。保留两份会另存本地新版本；结果不明的旧上传将停止查证，绝不重放。选择远端会创建可编辑副本，外部HTML格式可能损失。".into();
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
impl Render for ServerDrafts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("server-drafts-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3();
        if let Some(account) = self.account.clone() {
            list = list.child(
                MaterialButton::new("choose-draft-folder")
                    .variant(ButtonVariant::Outlined)
                    .disabled(self.busy)
                    .child("选择草稿目录")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choosing_folder = !this.choosing_folder;
                        cx.notify();
                    })),
            );
            list = list.child(format!(
                "草稿目录：{}",
                self.folder.as_deref().unwrap_or("未确定，请扫描后选择")
            ));
            if self.folder.is_none() || self.choosing_folder {
                for folder in self.choices.iter().filter(|f| f.selectable) {
                    let name = folder.name.clone();
                    let a = account.clone();
                    list = list.child(
                        MaterialButton::new(format!("draft-folder-{}", folder.name))
                            .variant(ButtonVariant::Outlined)
                            .disabled(self.busy)
                            .child(format!("选择目录：{}", folder.display_name))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.uids.clear();
                                this.epoch = 0;
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
            list = list.child("冲突处理（保留原件，不静默覆盖）");
            for conflict in &self.conflicts {
                let local = conflict.local.clone();
                let mut card = div()
                    .p_3()
                    .rounded_xl()
                    .bg(theme::color(ColorRole::SURFACE_CONTAINER))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(conflict.reason.clone())
                    .child(
                        MaterialButton::new(format!("conflict-local-{}", conflict.id))
                            .variant(ButtonVariant::Outlined)
                            .disabled(self.busy)
                            .child("查看本地草稿")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_local(local.clone(), cx)
                            })),
                    );
                if let Some(source) = conflict.original.clone() {
                    card = card.child(
                        MaterialButton::new(format!("conflict-original-{}", conflict.id))
                            .variant(ButtonVariant::Outlined)
                            .disabled(self.busy)
                            .child("查看远端原件")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.original_action(source.clone(), false, cx)
                            })),
                    );
                }
                for (label, resolution) in [
                    ("保留两份并另存新版本", DraftResolution::KeepBoth),
                    ("使用本地版本", DraftResolution::KeepLocal),
                    ("使用远端编辑副本", DraftResolution::KeepRemote),
                ] {
                    let id = conflict.id.clone();
                    let key = format!("{id}-{resolution:?}");
                    let text = if self.confirm.as_ref() == Some(&key) {
                        format!("确认：{label}")
                    } else {
                        label.into()
                    };
                    card = card.child(
                        MaterialButton::new(key)
                            .variant(ButtonVariant::Outlined)
                            .disabled(
                                self.busy
                                    || (matches!(resolution, DraftResolution::KeepRemote)
                                        && conflict.original.is_none()),
                            )
                            .child(text)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_conflict(id.clone(), resolution, cx)
                            })),
                    );
                }
                list = list.child(card);
            }
            list = list.child("本地草稿服务器同步状态");
            for item in &self.sync_items {
                let draft = item.draft.clone();
                let state = crate::composer::drafts_view::sync_label(item.revision, Some(item));
                list = list.child(
                    div()
                        .p_3()
                        .rounded_xl()
                        .bg(theme::color(ColorRole::SURFACE_CONTAINER))
                        .child(format!(
                            "{} · {} · 本地版本 {} / 基线 {} · {}",
                            item.subject, state, item.revision, item.base_revision, item.detail
                        ))
                        .child(
                            MaterialButton::new(format!("open-sync-{}", item.draft.0))
                                .variant(ButtonVariant::Outlined)
                                .disabled(self.busy)
                                .child("打开草稿")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_local(draft.clone(), cx)
                                })),
                        ),
                );
            }
            list = list.child("已保留原件（最近200份；转换不会覆盖原件）");
            for source in &self.originals {
                let id = source.id.clone();
                let convert_id = id.clone();
                let converted = source
                    .converted
                    .as_ref()
                    .is_some_and(|d| self.sync_items.iter().any(|i| &i.draft == d));
                list = list.child(
                    div()
                        .p_3()
                        .rounded_xl()
                        .bg(theme::color(ColorRole::SURFACE_CONTAINER))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(format!(
                            "{} · {} · UID {} · {} 字节",
                            source.subject, source.folder, source.uid, source.size
                        ))
                        .child(
                            MaterialButton::new(format!("draft-view-{id}"))
                                .variant(ButtonVariant::Outlined)
                                .disabled(self.busy)
                                .child("查看原件")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.original_action(id.clone(), false, cx)
                                })),
                        )
                        .child(
                            MaterialButton::new(format!("draft-convert-{convert_id}"))
                                .variant(ButtonVariant::Outlined)
                                .disabled(self.busy)
                                .child(if converted {
                                    "打开编辑副本"
                                } else if self.confirm.as_ref() == Some(&convert_id) {
                                    "确认转换副本"
                                } else {
                                    "转换为编辑副本"
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.original_action(convert_id.clone(), true, cx)
                                })),
                        ),
                );
            }
            list =
                list.child("服务器扫描结果（点击后下载原件，原件上限40MiB，超限保留服务器内容）");
            for &uid in &self.uids {
                list = list.child(
                    MaterialButton::new(format!("draft-import-{uid}"))
                        .variant(ButtonVariant::Outlined)
                        .disabled(self.busy)
                        .child(format!("UID {uid} · 保留原件 / 检查新版本"))
                        .on_click(cx.listener(move |this, _, _, cx| this.import(uid, cx))),
                );
            }
        }
        div()
            .size_full()
            .p_6()
            .flex()
            .flex_col()
            .gap_3()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
            .child(div().text_xl().child("草稿"))
            .child(format!(
                "{} · {}",
                self.accounts
                    .iter()
                    .find(|(id, _)| Some(id) == self.account.as_ref())
                    .map(|(_, settings)| settings.address.as_str())
                    .unwrap_or("邮箱账号"),
                self.folder.as_deref().unwrap_or("草稿文件夹")
            ))
            .child(
                MaterialButton::new("scan-server-drafts")
                    .variant(ButtonVariant::Outlined)
                    .disabled(self.busy || self.account.is_none())
                    .child("扫描服务器草稿")
                    .on_click(cx.listener(|this, _, _, cx| this.load(None, true, cx))),
            )
            .child(
                MaterialButton::new("reload-server-drafts")
                    .variant(ButtonVariant::Outlined)
                    .disabled(self.busy)
                    .child("刷新本地原件")
                    .on_click(cx.listener(|this, _, _, cx| this.load(None, false, cx))),
            )
            .child(self.status.clone())
            .child(self.notice.clone())
            .child(list)
    }
}
