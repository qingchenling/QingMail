//! 原生只读索引窗口。邮件正文、远端标记与发送操作尚未接入。
use gpui_kit::{
    App, Bounds, Context, FocusHandle, KeyDownEvent, Render, ScrollStrategy, TitlebarOptions,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
    uniform_list,
};
use gpui_kit_material::{prelude::*, theme};
use mail_core::{AccountId, AccountSettings, CachedMessage, FolderCache, RemoteFolder};
use mail_daemon::storage::{StorageClient, receive};
use mail_service::{
    credentials::{CredentialVault, SystemVault},
    sync,
};
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
            before: None,
            busy: false,
            status: "正在读取本地账号与缓存…".into(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
        };
        this.reload(cx);
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
        self.selected_folder = Some(name);
        self.before = None;
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
        self.selected = None;
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
                        this.status = match &this.cache {
                            None => "尚未同步这个文件夹。点击“同步最近邮件”".into(),
                            Some(cache) => format!(
                                "本地缓存 · 本页 {} 封 · 最近确认 {}。缓存可能包含远端已移除的邮件",
                                cache.messages.len(),
                                sync::display_time(cache.synced_at)
                            ),
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
        let Some((account, settings)) = self.account.clone() else {
            return;
        };
        if account != message.location.account {
            return;
        }
        let Some(name) = self.selected_folder.clone() else {
            return;
        };
        self.busy = true;
        self.status = "正在读取缓存；未缓存时将只读下载完整邮件（含内含附件，最多8MiB）…".into();
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let raw = match receive(store.message_body(message.location.clone()))? {
                Some(raw) => raw,
                None => {
                    let _guard = NETWORK
                        .try_lock()
                        .map_err(|_| "另一个网络请求仍在结束，请稍后重试".to_owned())?;
                    let credentials = SystemVault.load(&account).map_err(|e| e.to_string())?;
                    let raw = sync::message(
                        &settings.imap,
                        &credentials.imap,
                        &name,
                        message.location.uidvalidity,
                        message.location.uid,
                    )
                    .map_err(|e| e.to_string())?;
                    receive(store.save_message_body(message.location.clone(), raw.clone()))?;
                    // 重新读取时再核对关联与摘要；不将损坏缓存误当可用下载。
                    receive(store.message_body(message.location.clone()))?
                        .ok_or_else(|| "邮件缓存未确认".to_owned())?
                }
            };
            mail_content::parse(&raw).map_err(|e| e.to_string())
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(content) => {
                        this.status = "邮件已缓存并交给简化阅读，远端已读状态未改变".into();
                        cx.defer(move |cx| crate::reader::open_content(content, cx));
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
    fn messages(&self) -> &[CachedMessage] {
        self.cache
            .as_ref()
            .map(|cache| cache.messages.as_slice())
            .unwrap_or(&[])
    }
    fn keys(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
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
                .child("写信 / 本地草稿")
                .on_click(|_, _, cx| cx.defer(crate::composer::open)),
        );
        tree = tree.child(
            MaterialButton::new("mailbox-account-settings")
                .child("添加邮箱")
                .on_click(|_, _, cx| crate::accounts::open(cx)),
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
            .child(
                div()
                    .p_2()
                    .text_sm()
                    .child("最近90天 · 每页最多200封 · 按UID倒序 · 只下载邮件头"),
            );
        if self.messages().is_empty() {
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
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let message = &this.messages()[index];
                                let selected = this.selected == Some(index);
                                div()
                                    .id(("message", index))
                                    .h(px(68.))
                                    .px_3()
                                    .py_2()
                                    .flex()
                                    .flex_col()
                                    .overflow_hidden()
                                    .cursor_pointer()
                                    .bg(theme::color(if selected {
                                        ColorRole::SECONDARY_CONTAINER
                                    } else {
                                        ColorRole::SURFACE
                                    }))
                                    .child(div().truncate().child(
                                        if message.envelope.subject.is_empty() {
                                            "（无主题）".into()
                                        } else {
                                            message.envelope.subject.clone()
                                        },
                                    ))
                                    .child(
                                        div()
                                            .text_sm()
                                            .truncate()
                                            .child(message.envelope.sender.clone()),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.selected = Some(index);
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
                ),
        );
        let mut detail = div()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .min_w_0()
            .child("邮件头详情");
        if let Some(message) = self.selected.and_then(|index| self.messages().get(index)) {
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
        let wide = window.bounds().size.width >= px(1050.);
        let body = if wide {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(list)
                .child(detail.w(px(360.)).flex_shrink_0())
        } else {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(list)
                .child(detail.max_h(px(170.)).overflow_hidden())
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
