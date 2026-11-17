//! 服务器草稿原件入口；只在明确确认后转换编辑副本，不覆盖服务器。
use gpui_kit::{
    App, Bounds, Context, Render, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use gpui_kit_material::{prelude::*, theme};
use mail_core::{AccountId, AccountSettings, RemoteFolder};
use mail_daemon::storage::StorageClient;
use mail_protocol::{BusinessOperation, Command, Reply, ServerDraftSource};
pub(super) fn open(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1050.), px(780.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — 服务器草稿".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    if let Err(e) = gpui_kit::open_window(options, cx, |_, cx| cx.new(ServerDrafts::new)) {
        eprintln!("无法打开服务器草稿：{e}");
    }
}
struct ServerDrafts {
    store: StorageClient,
    accounts: Vec<(AccountId, AccountSettings)>,
    account: Option<AccountId>,
    folder: Option<String>,
    choices: Vec<RemoteFolder>,
    epoch: u32,
    uids: Vec<u32>,
    originals: Vec<ServerDraftSource>,
    sync_items: Vec<mail_protocol::DraftSyncInfo>,
    busy: bool,
    confirm: Option<String>,
    choosing_folder: bool,
    status: String,
}
struct Loaded {
    accounts: Vec<(AccountId, AccountSettings)>,
    account: Option<AccountId>,
    folder: Option<String>,
    choices: Vec<RemoteFolder>,
    originals: Vec<ServerDraftSource>,
    sync_items: Vec<mail_protocol::DraftSyncInfo>,
    index: Option<(u32, Vec<u32>, bool)>,
}
impl ServerDrafts {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            store: crate::lifecycle::storage(cx),
            accounts: vec![],
            account: None,
            folder: None,
            choices: vec![],
            epoch: 0,
            uids: vec![],
            originals: vec![],
            sync_items: vec![],
            busy: false,
            confirm: None,
            choosing_folder: false,
            status: "正在读取…".into(),
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
                index: None,
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
                match store.control(Command::ServerDrafts { account })? {
                    Reply::ServerDrafts { folder, items } => {
                        result.folder = folder;
                        result.originals = items;
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
                    this.accounts=result.accounts;this.account=result.account;this.folder=result.folder;this.choices=result.choices;this.originals=result.originals;this.sync_items=result.sync_items;
                    if let Some((epoch,uids,more))=result.index {
                        this.epoch=epoch;this.uids=uids;this.status=if more{"仅显示最新200个UID，旧草稿仍在服务器；不会推断远端删除".into()}else{"扫描完成；点击保留原件后可离线查看，转换副本需明确确认".into()};
                    } else {this.status="编辑停止约5秒后上传新版本，关窗会合并调度。结果不明只查证；旧服务器版本保留。".into();}
                },Err(e)=>this.status=e};cx.notify();});
        }).detach();
        cx.notify();
    }
    fn original_action(&mut self, source: String, convert: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let converted = self
            .originals
            .iter()
            .any(|s| s.id == source && s.converted.is_some());
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
        let mut accounts = div().flex().flex_wrap().gap_2();
        for (id, settings) in &self.accounts {
            let account = id.clone();
            accounts = accounts.child(
                MaterialButton::new(format!("draft-account-{}", id.0))
                    .variant(ButtonVariant::Outlined)
                    .disabled(self.busy)
                    .child(format!(
                        "{}{}",
                        if self.account.as_ref() == Some(id) {
                            "✓ "
                        } else {
                            ""
                        },
                        settings.display_name
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.account = Some(account.clone());
                        this.folder = None;
                        this.uids.clear();
                        this.epoch = 0;
                        this.confirm = None;
                        this.load(None, false, cx);
                    })),
            );
        }
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
            list = list.child("本地草稿服务器同步状态");
            for item in &self.sync_items {
                let state = match item.state.as_str() {
                    "synced" => "已核验服务器版本",
                    "pending" => "待同步",
                    "uncertain" => "结果不确定（只查证）",
                    "conflict" => "冲突（已暂停）",
                    "failed" => "失败（修正后再编辑）",
                    _ => "已停止",
                };
                list = list.child(
                    div()
                        .p_3()
                        .rounded_xl()
                        .bg(theme::color(ColorRole::SURFACE_CONTAINER))
                        .child(format!(
                            "{} · {} · 本地版本 {} / 基线 {} · {}",
                            item.subject, state, item.revision, item.base_revision, item.detail
                        )),
                );
            }
            list = list.child("已保留原件（最近200份；转换不会覆盖原件）");
            for source in &self.originals {
                let id = source.id.clone();
                let convert_id = id.clone();
                let converted = source.converted.is_some();
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
            list = list.child("服务器扫描结果（点击后下载原件，当前原件上限8MiB）");
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
            .child(div().text_xl().child("服务器草稿"))
            .child(accounts)
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
            .child(list)
    }
}
