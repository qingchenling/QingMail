//! 原生只读索引窗口：目录/头部同步、按需正文下载与阅读；远端标记与发送操作尚未接入。
use gpui_kit::{
    App, Bounds, Context, Entity, FocusHandle, KeyDownEvent, Render, ScrollStrategy,
    TitlebarOptions, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
    base::input::InputState, div, prelude::*, px, size, uniform_list,
};
use gpui_kit_material::{prelude::*, theme};
use mail_core::{AccountId, AccountSettings, CachedMessage, FolderCache, RemoteFolder};
use mail_daemon::storage::{StorageClient, receive};
use mail_service::{
    actions,
    credentials::{CredentialVault, SystemVault},
    folders, sync,
};
use mail_storage::{SearchHit, SearchQuery};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
static NETWORK: Mutex<()> = Mutex::new(());
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
#[derive(Clone, Debug)]
struct TreeRow {
    name: String,
    label: String,
    depth: usize,
    selectable: bool,
    children: bool,
}
fn tree_rows(folders: &[RemoteFolder], collapsed: &BTreeSet<String>) -> Vec<TreeRow> {
    let mut nodes = BTreeMap::<String, TreeRow>::new();
    for folder in folders {
        let delimiter = folder.delimiter.as_deref().filter(|d| !d.is_empty());
        let parts = delimiter
            .map(|d| folder.name.split(d).collect::<Vec<_>>())
            .unwrap_or_else(|| vec![folder.name.as_str()]);
        for i in 0..parts.len().min(32) {
            if nodes.len() >= 8192 {
                break;
            }
            let name = parts[..=i].join(delimiter.unwrap_or(""));
            let real = i + 1 == parts.len();
            let row = nodes.entry(name.clone()).or_insert(TreeRow {
                name,
                label: mail_service::accounts::folder_label(parts[i]),
                depth: i.min(24),
                selectable: false,
                children: false,
            });
            row.children |= !real;
            if real {
                row.selectable = folder.selectable;
            }
        }
    }
    nodes
        .into_values()
        .filter(|row| {
            !folders
                .iter()
                .filter_map(|f| f.delimiter.as_deref().filter(|d| !d.is_empty()))
                .any(|delimiter| {
                    collapsed
                        .iter()
                        .any(|parent| row.name.starts_with(&format!("{parent}{delimiter}")))
                })
        })
        .collect()
}
pub(super) fn open(cx: &mut App) {
    if crate::lifecycle::reuse_mailbox(cx) {
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1200.), px(820.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — 邮件".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| Mailbox::new(window, cx))
    }) {
        Ok((window, _)) => crate::lifecycle::register_mailbox(window, cx),
        Err(_) => eprintln!("无法打开邮件窗口"),
    }
}
struct Mailbox {
    store: StorageClient,
    account: Option<(AccountId, AccountSettings)>,
    expanded: bool,
    collapsed: BTreeSet<String>,
    folders: Vec<RemoteFolder>,
    catalog_at: Option<i64>,
    selected_folder: Option<String>,
    cache: Option<FolderCache>,
    selected: Option<usize>,
    checked: BTreeSet<u32>,
    undo_flags: Option<(Vec<u32>, Option<bool>, Option<bool>)>,
    confirm_delete: bool,
    history_exhausted: bool,
    quiet_reload: bool,
    restore_selected: Option<usize>,
    search_text: Entity<InputState>,
    search_sender: Entity<InputState>,
    search_current_folder: bool,
    search_attachments: bool,
    search_results: Option<Vec<SearchHit>>,
    search_selected: Option<usize>,
    search_previous: Option<(Option<usize>, Option<u32>)>,
    narrow_detail: bool,
    positions: BTreeMap<String, (Option<u32>, Option<u32>)>,
    restore_uid: Option<u32>,
    before: Option<u32>,
    busy: bool,
    status: String,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
}
impl Mailbox {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            store: crate::lifecycle::storage(cx),
            account: None,
            expanded: true,
            collapsed: BTreeSet::new(),
            folders: vec![],
            catalog_at: None,
            selected_folder: None,
            cache: None,
            selected: None,
            checked: BTreeSet::new(),
            undo_flags: None,
            confirm_delete: false,
            history_exhausted: false,
            quiet_reload: false,
            restore_selected: None,
            search_text: cx.new(|cx| InputState::new(window, cx)),
            search_sender: cx.new(|cx| InputState::new(window, cx)),
            search_current_folder: true,
            search_attachments: false,
            search_results: None,
            search_selected: None,
            search_previous: None,
            narrow_detail: false,
            positions: BTreeMap::new(),
            restore_uid: None,
            before: None,
            busy: false,
            status: "正在读取本地账号与缓存…".into(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
        };
        this.reload(cx);
        cx.spawn(async move |entity, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                if entity.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        if std::env::args().any(|a| a == "--mailbox-probe") {
            cx.spawn_in(window, async move |entity, cx| {
                for step in 0..5 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(500))
                        .await;
                    entity
                        .update_in(cx, |this, _, cx| this.probe(step, cx))
                        .expect("mailbox probe");
                }
            })
            .detach();
        }
        if std::env::args().any(|a| a == "--body-probe") {
            cx.spawn_in(window, async move |entity, cx| {
                for step in 0..3 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(700))
                        .await;
                    entity
                        .update_in(cx, |this, _, cx| {
                            assert!(!this.busy);
                            match step {
                                0 => this.select_folder("INBOX".into(), cx),
                                1 => {
                                    this.selected = Some(0);
                                    this.read_message(cx);
                                }
                                _ => panic!("body reader did not complete: {}", this.status),
                            }
                        })
                        .expect("body probe mailbox");
                }
            })
            .detach();
        }
        if std::env::args().any(|a| a == "--send-review-probe") {
            cx.spawn_in(window, async move |entity, cx| {
                for _ in 0..50 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(200))
                        .await;
                    let ready = entity
                        .update_in(cx, |this, _, cx| {
                            if this.busy {
                                return false;
                            }
                            let (id, settings) =
                                this.account.clone().expect("probe synthetic account");
                            cx.defer(move |cx| {
                                crate::composer::open_account(id, settings.address, cx)
                            });
                            true
                        })
                        .unwrap();
                    if ready {
                        return;
                    }
                }
                panic!("send review probe mailbox timeout");
            })
            .detach();
        }
        this
    }
    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let accounts = receive(store.accounts())?;
            let account = accounts.into_iter().next();
            let catalog = if let Some((id, _)) = &account {
                receive(store.catalog(id.clone()))?
            } else {
                (vec![], None)
            };
            Ok::<_, String>((account, catalog))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((account, (folders, at))) => {
                        this.account = account;
                        this.folders = folders;
                        this.catalog_at = at;
                        this.selected_folder = None;
                        this.cache = None;
                        this.selected = None;
                        this.status = if this.account.is_none() {
                            "尚未添加邮箱，请先添加通用邮箱".into()
                        } else if at.is_none() {
                            "尚未同步。点击“刷新文件夹”获取服务器目录".into()
                        } else {
                            "已加载本地文件夹目录。选择文件夹查看缓存，可离线浏览".into()
                        };
                    }
                    Err(e) => this.status = e,
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn select_folder(&mut self, name: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if let Some(current) = self.selected_folder.clone() {
            let uid = self
                .selected
                .and_then(|index| self.messages().get(index))
                .map(|message| message.location.uid);
            self.positions.insert(current, (uid, self.before));
        }
        let (restore_uid, restore_before) =
            self.positions.get(&name).copied().unwrap_or((None, None));
        self.restore_uid = restore_uid;
        self.narrow_detail = false;
        self.selected_folder = Some(name);
        self.before = restore_before;
        self.search_results = None;
        self.search_previous = None;
        self.search_selected = None;
        self.checked.clear();
        self.undo_flags = None;
        self.confirm_delete = false;
        self.history_exhausted = false;
        self.load_cache(cx);
    }
    fn load_cache(&mut self, cx: &mut Context<Self>) {
        let Some((account, _)) = &self.account else {
            return;
        };
        let Some(name) = &self.selected_folder else {
            return;
        };
        self.busy = true;
        let selected_uid = self
            .selected
            .and_then(|index| self.messages().get(index))
            .map(|message| message.location.uid);
        let quiet = self.quiet_reload;
        if !quiet {
            self.selected = None;
        }
        let request = self
            .store
            .folder_cache(account.clone(), name.clone(), self.before);
        let task = cx
            .background_executor()
            .spawn(async move { receive(request) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(cache) => {
                        this.cache = cache;
                        this.scroll = UniformListScrollHandle::new();
                        let quiet = std::mem::take(&mut this.quiet_reload);
                        if let Some(index) = std::mem::take(&mut this.restore_selected) {
                            if index < this.messages().len() {
                                this.selected = Some(index);
                            }
                        } else if let Some(uid) = std::mem::take(&mut this.restore_uid) {
                            this.selected = this
                                .messages()
                                .iter()
                                .position(|message| message.location.uid == uid);
                        } else if quiet
                            && let Some(uid) = selected_uid
                        {
                            this.selected = this
                                .messages()
                                .iter()
                                .position(|message| message.location.uid == uid);
                        }
                        if !quiet {
                            this.status = match &this.cache {
                                None => "尚未同步这个文件夹。点击“同步最近邮件”".into(),
                                Some(cache) => format!(
                                    "本地缓存 · 本页 {} 封 · 最近确认 {}。缓存可能包含远端已移除的邮件",
                                    cache.messages.len(),
                                    sync::display_time(cache.synced_at)
                                ),
                            };
                        }
                    }
                    Err(e) => {
                        this.quiet_reload = false;
                        this.status = e;
                    }
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some((id, settings)) = self.account.clone() else {
            return;
        };
        self.busy = true;
        self.status = "正在获取服务器文件夹…".into();
        let task = cx.background_executor().spawn(async move {
            let _guard = NETWORK
                .try_lock()
                .map_err(|_| "另一个同步检查仍在结束，请稍后重试".to_owned())?;
            let credentials = SystemVault.load(&id).map_err(|e| e.to_string())?;
            sync::folders(&settings.imap, &credentials.imap).map_err(|e| e.to_string())
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| match result {
                Ok(folders) => this.publish_catalog(folders, cx),
                Err(e) => {
                    this.busy = false;
                    this.status = format!("同步失败：{e}。已保留本地目录，可重试");
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }
    fn publish_catalog(&mut self, folders: Vec<RemoteFolder>, cx: &mut Context<Self>) {
        let Some((account, _)) = &self.account else {
            return;
        };
        let at = now();
        let request = self
            .store
            .publish_catalog(account.clone(), folders.clone(), at);
        let task = cx
            .background_executor()
            .spawn(async move { receive(request) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        this.folders = folders;
                        this.catalog_at = Some(at);
                        if this.selected_folder.as_ref().is_some_and(|name| {
                            !this
                                .folders
                                .iter()
                                .any(|folder| &folder.name == name && folder.selectable)
                        }) {
                            this.selected_folder = None;
                            this.cache = None;
                            this.selected = None;
                        }
                        this.status = format!(
                            "文件夹目录已保存 · {} 项。请选择文件夹同步邮件",
                            this.folders.len()
                        );
                    }
                    Err(e) => this.status = format!("本地保存失败：{e}。保留上次目录"),
                };
                cx.notify();
            });
        })
        .detach();
    }
    fn sync_folder(&mut self, older: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some((id, settings)) = self.account.clone() else {
            return;
        };
        let Some(name) = self.selected_folder.clone() else {
            return;
        };
        let before = if older {
            self.cache
                .as_ref()
                .and_then(|cache| cache.messages.last())
                .map(|message| message.location.uid)
        } else {
            None
        };
        if older && before.is_none() {
            return;
        }
        let expected_epoch = self.cache.as_ref().map(|cache| cache.folder.uidvalidity);
        self.busy = true;
        self.status = "正在只读检查文件夹并获取最近90天的邮件头…".into();
        let task = cx.background_executor().spawn(async move {
            let _guard = NETWORK
                .try_lock()
                .map_err(|_| "另一个同步检查仍在结束，请稍后重试".to_owned())?;
            let credentials = SystemVault.load(&id).map_err(|e| e.to_string())?;
            sync::folder(
                &settings.imap,
                &credentials.imap,
                &name,
                before,
                expected_epoch,
            )
            .map_err(|e| e.to_string())
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| match result {
                Ok(batch) => {
                    let account = this.account.as_ref().expect("account").0.clone();
                    let request = this.store.publish_folder(account, batch, now());
                    let task = cx
                        .background_executor()
                        .spawn(async move { receive(request) });
                    cx.spawn(async move |entity, cx| {
                        let result = task.await;
                        let _ = entity.update(cx, |this, cx| {
                            this.busy = false;
                            match result {
                                Ok(()) => {
                                    this.before = before;
                                    this.load_cache(cx);
                                }
                                Err(e) => {
                                    this.status = format!("同步结果未保存：{e}。保留上次缓存");
                                    cx.notify();
                                }
                            }
                        });
                    })
                    .detach();
                }
                Err(e) => {
                    this.busy = false;
                    this.status = format!("同步失败：{e}。保留上次缓存，可重试");
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }
    fn read_message(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(message) = self
            .selected
            .and_then(|index| self.messages().get(index))
            .cloned()
        else {
            return;
        };
        let Some(name) = self.selected_folder.clone() else {
            return;
        };
        self.read_location(message.location, name, cx);
    }
    fn open_hit(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(hit) = self
            .search_results
            .as_ref()
            .and_then(|results| results.get(index))
            .cloned()
        else {
            return;
        };
        self.search_selected = Some(index);
        self.read_location(hit.message.location, hit.folder, cx);
    }
    fn read_location(
        &mut self,
        location: mail_core::MailUid,
        folder: String,
        cx: &mut Context<Self>,
    ) {
        let Some((account, settings)) = self.account.clone() else {
            return;
        };
        if account != location.account {
            return;
        }
        self.busy = true;
        self.status = "正在读取缓存；未缓存时将只读下载完整邮件（含内含附件，最多8MiB）…".into();
        let reader_account = Some((account.clone(), settings.address.clone()));
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let raw = match receive(store.message_body(location.clone()))? {
                Some(raw) => raw,
                None => {
                    let _guard = NETWORK
                        .try_lock()
                        .map_err(|_| "另一个网络请求仍在结束，请稍后重试".to_owned())?;
                    let credentials = SystemVault.load(&account).map_err(|e| e.to_string())?;
                    let raw = sync::message(
                        &settings.imap,
                        &credentials.imap,
                        &folder,
                        location.uidvalidity,
                        location.uid,
                    )
                    .map_err(|e| e.to_string())?;
                    receive(store.save_message_body(location.clone(), raw.clone()))?;
                    // 重新读取时再核对关联与摘要；不将损坏缓存误当可用下载。
                    receive(store.message_body(location.clone()))?
                        .ok_or_else(|| "邮件缓存未确认".to_owned())?
                }
            };
            let content = mail_content::parse(&raw).map_err(|e| e.to_string())?;
            // 记录附件数量，供本地“有附件”搜索使用；失败不影响阅读。
            let _ = receive(store.mark_attachments(location, content.attachments.len() as u32));
            Ok::<_, String>((content, account, reader_account))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((content, _, reader_account)) => {
                        this.status = "邮件已缓存并交给简化阅读，远端已读状态未改变".into();
                        cx.defer(move |cx| {
                            crate::reader::open_content(content, reader_account, cx)
                        });
                    }
                    Err(e) => {
                        this.status = format!("无法打开邮件：{e}。索引与原缓存已保留，可重试")
                    }
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn run_search(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some((account, _)) = self.account.clone() else {
            self.status = "请先添加邮箱".into();
            cx.notify();
            return;
        };
        let text = self.search_text.read(cx).value().trim().to_owned();
        let sender = self.search_sender.read(cx).value().trim().to_owned();
        if text.is_empty() && sender.is_empty() && !self.search_attachments {
            self.status = "请输入关键词或发件人，或勾选“有附件”".into();
            cx.notify();
            return;
        }
        let folder = if self.search_current_folder {
            match self.selected_folder.clone() {
                Some(folder) => Some(folder),
                None => {
                    self.status = "请先选择文件夹，或切换为整个账号范围".into();
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        if self.search_results.is_none() {
            self.search_previous = Some((self.selected, self.before));
        }
        let query = SearchQuery {
            text: text.clone(),
            sender: sender.clone(),
            has_attachments: self.search_attachments,
            folder: folder.clone(),
        };
        self.busy = true;
        self.status = "正在搜索本地缓存…".into();
        let reply = self.store.search(account, query);
        let task = cx
            .background_executor()
            .spawn(async move { receive(reply) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(results) => {
                        this.status = format!(
                            "本地搜索：{} 封 · {} · 仅覆盖已缓存索引；附件筛选只统计已下载正文",
                            results.len(),
                            if folder.is_some() {
                                "当前文件夹"
                            } else {
                                "整个账号"
                            }
                        );
                        this.search_results = Some(results);
                        this.search_selected = None;
                        this.checked.clear();
                    }
                    Err(error) => this.status = format!("搜索失败：{error}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn clear_search(&mut self, cx: &mut Context<Self>) {
        if self.search_results.take().is_none() {
            return;
        }
        let (selected, before) = self.search_previous.take().unwrap_or((None, None));
        self.before = before;
        self.restore_selected = selected;
        self.search_selected = None;
        self.checked.clear();
        self.status = "已清除搜索，恢复之前位置".into();
        self.quiet_reload = true;
        self.load_cache(cx);
    }

    fn targets(&self) -> Vec<u32> {
        if !self.checked.is_empty() {
            return self.checked.iter().copied().collect();
        }
        self.selected
            .and_then(|index| self.messages().get(index))
            .map(|message| vec![message.location.uid])
            .unwrap_or_default()
    }
    fn toggle_checked(&mut self, uid: u32, cx: &mut Context<Self>) {
        if !self.checked.remove(&uid) {
            self.checked.insert(uid);
        }
        self.confirm_delete = false;
        cx.notify();
    }
    fn apply_flags(&mut self, seen: Option<bool>, flagged: Option<bool>, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let targets = self.targets();
        if targets.is_empty() {
            self.status = "请先选择邮件".into();
            cx.notify();
            return;
        }
        let Some((account, settings)) = self.account.clone() else {
            return;
        };
        let Some(name) = self.selected_folder.clone() else {
            return;
        };
        let Some(epoch) = self.cache.as_ref().map(|cache| cache.folder.uidvalidity) else {
            return;
        };
        // 撤销只在目标邮件标记一致时提供，避免把混合状态压成一个值。
        let values = self
            .messages()
            .iter()
            .filter(|message| targets.contains(&message.location.uid))
            .map(|message| (message.seen, message.flagged))
            .collect::<Vec<_>>();
        let previous = (values.len() == targets.len()
            && values.windows(2).all(|pair| pair[0] == pair[1]))
        .then(|| values[0]);
        let undo = previous.map(|(previous_seen, previous_flagged)| {
            (
                targets.clone(),
                seen.is_some().then_some(previous_seen),
                flagged.is_some().then_some(previous_flagged),
            )
        });
        self.busy = true;
        self.status = "正在更新服务器标记…".into();
        let store = self.store.clone();
        let uids = targets.clone();
        let task = cx.background_executor().spawn(async move {
            let credentials = SystemVault.load(&account).map_err(|e| e.to_string())?;
            actions::store_flags(
                &settings.imap,
                &credentials.imap,
                &name,
                epoch,
                &uids,
                actions::FlagChange { seen, flagged },
            )
            .map_err(|e| e.to_string())?;
            receive(store.set_flags(
                account.clone(),
                name.clone(),
                epoch,
                uids.clone(),
                seen,
                flagged,
            ))?;
            Ok::<_, String>(uids.len())
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(count) => {
                        this.status = format!("已更新 {count} 封邮件的标记");
                        this.checked.clear();
                        this.undo_flags = undo;
                        this.quiet_reload = true;
                        this.load_cache(cx);
                    }
                    Err(error) => {
                        this.status = format!("标记更新失败，缓存未改动：{error}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn undo_flags(&mut self, cx: &mut Context<Self>) {
        let Some((uids, seen, flagged)) = self.undo_flags.take() else {
            return;
        };
        self.checked = uids.iter().copied().collect();
        self.apply_flags(seen, flagged, cx);
    }
    fn move_selected(&mut self, trash: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let targets = self.targets();
        if targets.is_empty() {
            self.status = "请先选择邮件".into();
            cx.notify();
            return;
        }
        let Some((account, settings)) = self.account.clone() else {
            return;
        };
        let Some(name) = self.selected_folder.clone() else {
            return;
        };
        let Some(epoch) = self.cache.as_ref().map(|cache| cache.folder.uidvalidity) else {
            return;
        };
        let destination = if trash {
            folders::trash(&self.folders, None)
        } else {
            folders::archive(&self.folders, None)
        };
        let Some(destination) = destination else {
            self.status = if trash {
                "未找到“已删除”文件夹，未执行删除".into()
            } else {
                "未找到“归档”文件夹，未执行归档".into()
            };
            cx.notify();
            return;
        };
        if destination == name {
            self.status = "邮件已在目标文件夹".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.confirm_delete = false;
        self.status = format!("正在移动到 {destination}…");
        let store = self.store.clone();
        let uids = targets.clone();
        let task = cx.background_executor().spawn(async move {
            let credentials = SystemVault.load(&account).map_err(|e| e.to_string())?;
            let outcome = actions::move_messages(
                &settings.imap,
                &credentials.imap,
                &name,
                epoch,
                &uids,
                &destination,
            )
            .map_err(|e| e.to_string())?;
            receive(store.remove_cached(account.clone(), name.clone(), epoch, uids.clone()))?;
            Ok::<_, String>((outcome, uids.len(), destination))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.undo_flags = None;
                match result {
                    Ok((actions::MoveOutcome::Moved, count, destination)) => {
                        this.status = format!("已移动 {count} 封邮件到 {destination}");
                        this.checked.clear();
                        this.quiet_reload = true;
                        this.load_cache(cx);
                    }
                    Ok((actions::MoveOutcome::CopiedOnly, count, destination)) => {
                        this.status = format!(
                            "已复制 {count} 封到 {destination} 并标记删除；服务器未确认彻底移除，请稍后同步核对"
                        );
                        this.checked.clear();
                        this.quiet_reload = true;
                        this.load_cache(cx);
                    }
                    Err(error) => this.status = format!("移动失败，缓存未改动：{error}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn load_older_history(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some((account, settings)) = self.account.clone() else {
            return;
        };
        let Some(name) = self.selected_folder.clone() else {
            return;
        };
        let Some(epoch) = self.cache.as_ref().map(|cache| cache.folder.uidvalidity) else {
            return;
        };
        let before = self
            .messages()
            .iter()
            .map(|message| message.location.uid)
            .min();
        self.busy = true;
        self.status = "正在加载最近90天之前的历史…".into();
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let credentials = SystemVault.load(&account).map_err(|e| e.to_string())?;
            let batch = sync::history(
                &settings.imap,
                &credentials.imap,
                &name,
                before,
                Some(epoch),
            )
            .map_err(|e| e.to_string())?;
            let count = batch.messages.len();
            let has_more = batch.has_more;
            receive(store.publish_folder(account.clone(), batch, now()))?;
            Ok::<_, String>((count, has_more))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((0, _)) => {
                        this.history_exhausted = true;
                        this.status = "没有更早的历史邮件".into();
                    }
                    Ok((count, has_more)) => {
                        this.history_exhausted = !has_more;
                        this.status = format!("已加载 {count} 封更早历史");
                        this.quiet_reload = true;
                        this.load_cache(cx);
                    }
                    Err(error) => this.status = format!("历史加载失败：{error}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn messages(&self) -> &[CachedMessage] {
        self.cache
            .as_ref()
            .map(|cache| cache.messages.as_slice())
            .unwrap_or(&[])
    }
    fn keys(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(results) = &self.search_results {
            let count = results.len();
            if count == 0 {
                return;
            }
            let index = match event.keystroke.key.as_str() {
                "down" => self
                    .search_selected
                    .map(|i| (i + 1).min(count - 1))
                    .unwrap_or(0),
                "up" => self.search_selected.unwrap_or(0).saturating_sub(1),
                "enter" => {
                    if let Some(index) = self.search_selected {
                        self.open_hit(index, cx);
                    }
                    return;
                }
                _ => return,
            };
            self.search_selected = Some(index);
            cx.notify();
            return;
        }
        let count = self.messages().len();
        if count == 0 {
            return;
        }
        let index = match event.keystroke.key.as_str() {
            "down" => self.selected.map(|i| (i + 1).min(count - 1)).unwrap_or(0),
            "up" => self.selected.unwrap_or(0).saturating_sub(1),
            _ => return,
        };
        self.selected = Some(index);
        self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        cx.notify();
    }
    fn probe(&mut self, step: u8, cx: &mut Context<Self>) {
        match step {
            0 => {
                assert!(!self.busy);
                assert!(self.account.is_some());
                assert!(self.catalog_at.is_some());
                assert!(tree_rows(&self.folders, &self.collapsed).len() >= 3);
                self.select_folder("INBOX".into(), cx);
            }
            1 => {
                assert!(!self.busy);
                assert_eq!(self.messages().len(), 200);
                assert_eq!(self.messages()[0].envelope.subject, "合成主题 240");
                self.selected = Some(1);
                self.expanded = false;
                assert_eq!(self.selected_folder.as_deref(), Some("INBOX"));
            }
            2 => {
                assert_eq!(self.selected, Some(1));
                self.before = Some(41);
                self.load_cache(cx);
            }
            3 => {
                assert!(!self.busy);
                assert_eq!(self.messages().len(), 40);
                assert_eq!(self.messages()[0].location.uid, 40);
                self.before = None;
                self.load_cache(cx);
            }
            _ => {
                assert_eq!(self.messages().len(), 200);
                self.selected = Some(1);
                assert_eq!(self.selected, Some(1));
                assert_eq!(self.messages()[1].location.uid, 239);
                eprintln!(
                    "QINGMAIL_MAILBOX_PROBE: PASS · cached tree / UID list / selection / collapse preserves folder / no network"
                );
                cx.quit();
            }
        }
        cx.notify();
    }
}
impl Render for Mailbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tree = div()
            .id("folder-tree")
            .w(px(208.))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .bg(theme::color(ColorRole::SURFACE_CONTAINER))
            .flex()
            .flex_col()
            .gap_1()
            .child(div().p_3().child("QingMail"));
        tree = tree.child(
            MaterialButton::new("mailbox-compose")
                .child("写信 / 当前账号草稿")
                .disabled(self.account.is_none())
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some((account, settings)) = this.account.clone() {
                        cx.defer(move |cx| {
                            crate::composer::open_account(account, settings.address, cx)
                        });
                    }
                })),
        );
        tree = tree
            .child(
                MaterialButton::new("mailbox-account-settings")
                    .child("账号管理")
                    .on_click(|_, _, cx| crate::accounts::open(cx)),
            )
            .child(
                MaterialButton::new("mailbox-send-tasks")
                    .child("发送任务")
                    .on_click(|_, _, cx| crate::send_tasks::open(cx)),
            )
            .child(
                MaterialButton::new("mailbox-settings")
                    .child("设置")
                    .on_click(|_, _, cx| crate::settings::open(cx)),
            );
        if let Some((_, settings)) = &self.account {
            tree = tree.child(
                div()
                    .id("account-tree-root")
                    .p_3()
                    .cursor_pointer()
                    .child(format!(
                        "{} {}",
                        if self.expanded { "▾" } else { "▸" },
                        settings.display_name
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.expanded = !this.expanded;
                        cx.notify();
                    })),
            );
            if self.expanded {
                for (index, row) in tree_rows(&self.folders, &self.collapsed)
                    .into_iter()
                    .enumerate()
                {
                    let selected = self.selected_folder.as_ref() == Some(&row.name);
                    let toggle = row.name.clone();
                    let target = row.name.clone();
                    let children = row.children;
                    let selectable = row.selectable;
                    tree = tree.child(
                        div()
                            .flex()
                            .w_full()
                            .pl(px(12. + row.depth as f32 * 12.))
                            .bg(theme::color(if selected {
                                ColorRole::SECONDARY_CONTAINER
                            } else {
                                ColorRole::SURFACE_CONTAINER
                            }))
                            .child(
                                div()
                                    .id(("branch", index))
                                    .p_2()
                                    .cursor_pointer()
                                    .child(if children {
                                        if self.collapsed.contains(&row.name) {
                                            "▸"
                                        } else {
                                            "▾"
                                        }
                                    } else {
                                        "·"
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if children {
                                            if !this.collapsed.remove(&toggle) {
                                                this.collapsed.insert(toggle.clone());
                                            }
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                div()
                                    .id(("folder", index))
                                    .p_2()
                                    .flex_1()
                                    .overflow_hidden()
                                    .child(row.label)
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if selectable {
                                            this.select_folder(target.clone(), cx);
                                        }
                                    })),
                            ),
                    );
                }
            }
        } else {
            tree = tree.child(div().p_3().child("尚未添加邮箱"));
        }
        let title = self
            .selected_folder
            .as_deref()
            .map(mail_service::accounts::folder_label)
            .unwrap_or_else(|| "邮件索引".into());
        let header = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .p_3()
            .child(div().text_size(px(20.)).child(title))
            .child(
                MaterialButton::new("reload-cache")
                    .disabled(self.busy)
                    .child("重载本地账号")
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
            .child(
                MaterialButton::new("sync-catalog")
                    .disabled(self.busy || self.account.is_none())
                    .child("刷新文件夹")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_catalog(cx))),
            )
            .child(
                MaterialButton::new("sync-folder")
                    .disabled(self.busy || self.selected_folder.is_none())
                    .child("同步最近邮件")
                    .on_click(cx.listener(|this, _, _, cx| this.sync_folder(false, cx))),
            )
            .child(
                MaterialButton::new("toggle-auto-sync")
                    .variant(ButtonVariant::Tonal)
                    .child(if crate::lifecycle::sync_paused(cx) {
                        "恢复自动收信"
                    } else {
                        "暂停自动收信"
                    })
                    .on_click(cx.listener(|_, _, _, cx| {
                        crate::lifecycle::toggle_sync(cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .child(crate::lifecycle::sync_status(cx)),
            );
        let mut ops = div().flex().flex_wrap().gap_2().items_center().p_2();
        ops = ops
            .child(
                MaterialButton::new("select-page")
                    .child("全选本页")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.checked = this
                            .messages()
                            .iter()
                            .map(|message| message.location.uid)
                            .collect();
                        this.confirm_delete = false;
                        cx.notify();
                    })),
            )
            .child(
                MaterialButton::new("clear-selection")
                    .child("清除选择")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.checked.clear();
                        this.confirm_delete = false;
                        cx.notify();
                    })),
            )
            .child(
                MaterialButton::new("mark-seen")
                    .child("已读")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.apply_flags(Some(true), None, cx))),
            )
            .child(
                MaterialButton::new("mark-unseen")
                    .child("未读")
                    .disabled(self.busy)
                    .on_click(
                        cx.listener(|this, _, _, cx| this.apply_flags(Some(false), None, cx)),
                    ),
            )
            .child(
                MaterialButton::new("mark-flagged")
                    .child("星标")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.apply_flags(None, Some(true), cx))),
            )
            .child(
                MaterialButton::new("unmark-flagged")
                    .child("取消星标")
                    .disabled(self.busy)
                    .on_click(
                        cx.listener(|this, _, _, cx| this.apply_flags(None, Some(false), cx)),
                    ),
            )
            .child(
                MaterialButton::new("archive-selected")
                    .child("归档")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.move_selected(false, cx))),
            )
            .child(
                MaterialButton::new("delete-selected")
                    .child(if self.confirm_delete {
                        "再次点击确认删除"
                    } else {
                        "删除（移到已删除）"
                    })
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.confirm_delete {
                            this.move_selected(true, cx);
                        } else {
                            this.confirm_delete = true;
                            this.status = "再次点击删除按钮确认移到“已删除”".into();
                            cx.notify();
                        }
                    })),
            );
        if self.undo_flags.is_some() {
            ops = ops.child(
                MaterialButton::new("undo-flags")
                    .child("撤销标记")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.undo_flags(cx))),
            );
        }
        let search_bar = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .p_2()
            .child(TextField::new("mail-search-text", &self.search_text).label("关键词"))
            .child(TextField::new("mail-search-sender", &self.search_sender).label("发件人"))
            .child(
                MaterialButton::new("search-scope")
                    .variant(ButtonVariant::Tonal)
                    .child(if self.search_current_folder {
                        "当前文件夹"
                    } else {
                        "整个账号"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search_current_folder = !this.search_current_folder;
                        cx.notify();
                    })),
            )
            .child(
                MaterialButton::new("search-attachments")
                    .variant(if self.search_attachments {
                        ButtonVariant::Filled
                    } else {
                        ButtonVariant::Text
                    })
                    .child("有附件")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search_attachments = !this.search_attachments;
                        cx.notify();
                    })),
            )
            .child(
                MaterialButton::new("run-search")
                    .child("搜索本地缓存")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.run_search(cx))),
            )
            .child(
                MaterialButton::new("clear-search")
                    .child("清除搜索")
                    .disabled(self.busy || self.search_results.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx))),
            );
        let mut list = div()
            .id("mail-index-focus")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::keys))
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(search_bar)
            .child(ops)
            .child(
                div()
                    .p_2()
                    .text_sm()
                    .child("最近90天 · 每页最多200封 · 按UID倒序 · 只下载邮件头"),
            );
        let compact = crate::settings::prefs(cx).compact;
        if let Some(results) = &self.search_results {
            let mut search_view = div()
                .id("search-results")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1()
                .p_2();
            if results.is_empty() {
                search_view = search_view.child(
                    div()
                        .p_4()
                        .child("没有匹配的已缓存邮件；搜索不访问网络，可先同步或在目标文件夹重试"),
                );
            }
            for (index, hit) in results.iter().enumerate() {
                let selected = self.search_selected == Some(index);
                search_view = search_view.child(
                    div()
                        .id(("search-hit", index))
                        .p_2()
                        .cursor_pointer()
                        .bg(theme::color(if selected {
                            ColorRole::SECONDARY_CONTAINER
                        } else {
                            ColorRole::SURFACE
                        }))
                        .child(format!(
                            "{}{}",
                            if hit.message.seen { "" } else { "● " },
                            if hit.message.envelope.subject.is_empty() {
                                "（无主题）"
                            } else {
                                &hit.message.envelope.subject
                            }
                        ))
                        .child(div().text_sm().child(format!(
                            "{} · {}",
                            hit.message.envelope.sender,
                            mail_service::accounts::folder_label(&hit.folder)
                        )))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_hit(index, cx);
                        })),
                );
            }
            list = list.child(search_view);
        } else if self.messages().is_empty() {
            list = list.child(div().p_4().child(if self.cache.is_some() {
                "本页没有缓存邮件。同步后无结果表示该查询范围为空"
            } else {
                "选择文件夹查看缓存；尚未同步时请主动同步"
            }));
        } else {
            list = list.child(
                uniform_list(
                    "message-index",
                    self.messages().len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let message = &this.messages()[index];
                                let selected = this.selected == Some(index);
                                let uid = message.location.uid;
                                let checked = this.checked.contains(&uid);
                                let status = format!(
                                    "{}{}",
                                    if message.seen { "" } else { "● " },
                                    if message.flagged { "★ " } else { "" }
                                );
                                div()
                                    .id(("message", index))
                                    .h(px(if compact { 48. } else { 68. }))
                                    .px_2()
                                    .py(px(if compact { 4. } else { 8. }))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .overflow_hidden()
                                    .cursor_pointer()
                                    .bg(theme::color(if selected {
                                        ColorRole::SECONDARY_CONTAINER
                                    } else {
                                        ColorRole::SURFACE
                                    }))
                                    .child(
                                        MaterialButton::new(("check", index))
                                            .child(if checked { "☑" } else { "☐" })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.toggle_checked(uid, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .child(div().truncate().child(format!(
                                                "{status}{}",
                                                if message.envelope.subject.is_empty() {
                                                    "（无主题）"
                                                } else {
                                                    &message.envelope.subject
                                                }
                                            )))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .truncate()
                                                    .child(message.envelope.sender.clone()),
                                            ),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.selected = Some(index);
                                        this.confirm_delete = false;
                                        if window.bounds().size.width < px(1050.) {
                                            this.narrow_detail = true;
                                        }
                                        window.focus(&this.focus, cx);
                                        cx.notify();
                                    }))
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            );
        }
        if self.search_results.is_none() {
            list = list.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .p_2()
                    .child(
                        MaterialButton::new("first-cache-page")
                            .disabled(self.busy || self.before.is_none())
                            .child("返回最新缓存")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.before = None;
                                this.load_cache(cx);
                            })),
                    )
                    .child(
                        MaterialButton::new("older-cache-page")
                            .disabled(self.busy || self.messages().is_empty())
                            .child("下一页缓存")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.before =
                                    this.messages().last().map(|message| message.location.uid);
                                this.load_cache(cx);
                            })),
                    )
                    .child(
                        MaterialButton::new("older-network-page")
                            .disabled(self.busy || self.messages().is_empty())
                            .child("查询较早邮件（90天内）")
                            .on_click(cx.listener(|this, _, _, cx| this.sync_folder(true, cx))),
                    )
                    .child(
                        MaterialButton::new("older-history")
                            .disabled(self.busy || self.history_exhausted)
                            .child("加载更早历史（90天前）")
                            .on_click(cx.listener(|this, _, _, cx| this.load_older_history(cx))),
                    ),
            );
        }
        let wide = window.bounds().size.width >= px(1050.);
        let mut detail = div()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .child("邮件头详情");
        if !wide {
            detail = detail.child(
                MaterialButton::new("back-to-list")
                    .child("返回列表")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.narrow_detail = false;
                        cx.notify();
                    })),
            );
        }
        if let Some((index, hit)) = self
            .search_selected
            .zip(self.search_results.as_ref())
            .and_then(|(index, results)| results.get(index).map(|hit| (index, hit)))
        {
            detail = detail
                .child(hit.message.envelope.subject.clone())
                .child(hit.message.envelope.sender.clone())
                .child(format!(
                    "文件夹：{} · 邮件日期：{}",
                    mail_service::accounts::folder_label(&hit.folder),
                    sync::display_time(hit.message.envelope.received_at)
                ))
                .child(format!(
                    "UID {} · UIDVALIDITY {}",
                    hit.message.location.uid, hit.message.location.uidvalidity
                ))
                .child(
                    MaterialButton::new("open-search-hit")
                        .disabled(self.busy)
                        .child("打开正文")
                        .on_click(cx.listener(move |this, _, _, cx| this.open_hit(index, cx))),
                );
        } else if let Some(message) = self.selected.and_then(|index| self.messages().get(index)) {
            detail = detail
                .child(message.envelope.subject.clone())
                .child(message.envelope.sender.clone())
                .child(format!(
                    "邮件日期：{}",
                    sync::display_time(message.envelope.received_at)
                ))
                .child(format!(
                    "UID {} · UIDVALIDITY {}",
                    message.location.uid, message.location.uidvalidity
                ))
                .child("首次打开将下载完整邮件及其内含附件（≤8MiB）；缓存后可离线重开，不改变远端已读状态。")
                .child(MaterialButton::new("read-cached-message").disabled(self.busy).child("打开正文 / 缓存").on_click(cx.listener(|this,_,_,cx|this.read_message(cx))));
        } else {
            detail = detail.child("选择邮件查看头部详情");
        }
        let body = if wide {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(list)
                .child(detail.w(px(360.)).flex_shrink_0())
        } else if self.narrow_detail {
            div().flex().flex_col().flex_1().min_h_0().child(detail)
        } else {
            div().flex().flex_col().flex_1().min_h_0().child(list)
        };
        div()
            .size_full()
            .flex()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
            .child(tree)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(div().p_2().text_sm().child(self.status.clone()))
                    .child(body),
            )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hierarchy_creates_implicit_parents_and_collapse_keeps_stable_names() {
        let folders = vec![
            RemoteFolder {
                name: "Work/Team/计划".into(),
                display_name: "Work/Team/计划".into(),
                delimiter: Some("/".into()),
                selectable: true,
            },
            RemoteFolder {
                name: "INBOX".into(),
                display_name: "INBOX".into(),
                delimiter: Some("/".into()),
                selectable: true,
            },
        ];
        let rows = tree_rows(&folders, &BTreeSet::new());
        assert_eq!(rows.len(), 4);
        assert!(rows[1].children);
        assert!(!rows[1].selectable);
        assert_eq!(rows[3].depth, 2);
        let rows = tree_rows(&folders, &BTreeSet::from(["Work".into()]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "Work");
    }
}
