//! 邮箱业务状态：复用存储线程、协议服务与安全 Reader；M3E 布局见 view。
#[path = "mailbox_compose_probe.rs"]
mod compose_probe;
#[path = "mailbox_loading.rs"]
mod loading;
#[path = "mailbox_sync.rs"]
mod sync_status;
#[path = "mailbox_view.rs"]
mod view;
use gpui_kit::{
    App, Bounds, Context, Entity, FocusHandle, KeyDownEvent, ScrollStrategy, TitlebarOptions,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions, base::input::InputState,
    prelude::*, px, size,
};
use mail_core::{AccountId, AccountSettings, CachedMessage, FolderCache, RemoteFolder};
use mail_daemon::storage::{StorageClient, receive};
use mail_protocol::{BusinessOperation as Op, Reply};
use mail_service::{
    actions,
    credentials::{CredentialVault, SystemVault},
    folders, sync,
};
use mail_storage::{SearchHit, SearchQuery};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
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
                label: crate::ui::folder_label(parts[i]),
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
    let mut nodes = nodes.into_values().collect::<Vec<_>>();
    nodes.sort_by_key(|row| (!row.name.eq_ignore_ascii_case("INBOX"), row.name.clone()));
    nodes
        .into_iter()
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
fn is_drafts_folder(name: &str) -> bool {
    name.rsplit(['/', '.', '\\']).next().is_some_and(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            "draft" | "drafts" | "草稿" | "草稿箱"
        )
    })
}
fn sync_counts(tasks: &[mail_storage::SendTask]) -> (usize, bool) {
    let pending = tasks
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
        .count();
    let sending = tasks
        .iter()
        .any(|task| task.state == mail_core::SendState::Sending);
    (pending, sending)
}
pub(super) fn open(cx: &mut App) {
    if crate::lifecycle::reuse_mailbox(cx) {
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1440.), px(900.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — 邮件".into()),
            ..Default::default()
        }),
        ..crate::ui::window_options()
    };
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| Mailbox::new(window, cx))
    }) {
        Ok((window, entity)) => crate::lifecycle::register_mailbox(window, entity.downgrade(), cx),
        Err(_) => eprintln!("无法打开邮件窗口"),
    }
}
struct InlineReading {
    location: Option<mail_core::MailUid>,
    account: Option<(AccountId, String)>,
    content: mail_content::MailContent,
    view: Entity<crate::reader::Reader>,
}
fn inline_matches_selection(
    inline: Option<&mail_core::MailUid>,
    selected: Option<&mail_core::MailUid>,
) -> bool {
    match inline {
        Some(location) => selected == Some(location),
        None => selected.is_none(),
    }
}
type ComposeRequest = (
    Option<(AccountId, String)>,
    Option<mail_storage::DraftDocument>,
    bool,
);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Page {
    #[default]
    Mail,
    Accounts,
    Settings,
    Drafts,
}
pub(super) struct Mailbox {
    page: Page,
    pending_page: Option<Page>,
    account_page: Option<Entity<crate::accounts::Accounts>>,
    settings_page: Option<Entity<crate::settings::Settings>>,
    sync_open: bool,
    sync_state: sync_status::Status,
    sync_refreshing: bool,
    keep_operation_confirm: Option<String>,
    confirm_resend: Option<(AccountId, mail_core::SendTaskId)>,
    composer: Option<Entity<crate::composer::Composer>>,
    composer_subscription: Option<gpui_kit::Subscription>,
    composer_observation: Option<gpui_kit::Subscription>,
    pending_compose: Option<ComposeRequest>,
    pane_generation: u64,
    notification_target: Option<mail_protocol::OpenMailTarget>,
    observed_status: String,
    store: StorageClient,
    account: Option<(AccountId, AccountSettings)>,
    accounts: Vec<(AccountId, AccountSettings)>,
    catalogs: BTreeMap<String, (Vec<RemoteFolder>, Option<i64>)>,
    closed_accounts: BTreeSet<String>,
    default_account: Option<AccountId>,
    unified: bool,
    inbox: Option<mail_protocol::MessagePage>,
    inbox_before: Option<mail_protocol::InboxCursor>,
    search_scope: u8,
    collapsed: BTreeSet<String>,
    folders: Vec<RemoteFolder>,
    catalog_at: Option<i64>,
    selected_folder: Option<String>,
    cache: Option<FolderCache>,
    selected: Option<usize>,
    checked: HashSet<mail_core::MailUid>,
    undo_flags: Option<(Vec<u32>, Option<bool>, Option<bool>)>,
    confirm_delete: bool,
    history_exhausted: bool,
    quiet_reload: bool,
    restore_selected: Option<usize>,
    search_text: Entity<InputState>,
    search_sender: Entity<InputState>,
    search_attachments: bool,
    search_results: Option<Vec<SearchHit>>,
    search_selected: Option<usize>,
    search_previous: Option<(Option<usize>, Option<u32>)>,
    narrow_detail: bool,
    positions: BTreeMap<String, (Option<u32>, Option<u32>, usize)>,
    restore_uid: Option<u32>,
    restore_count: usize,
    inline: Option<InlineReading>,
    reading: Option<mail_core::MailUid>,
    read_running: bool,
    read_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pending_read: Option<(mail_core::MailUid, String)>,
    reading_status: String,
    sync_revision: u64,
    filters_open: bool,
    more_open: bool,
    message_more: bool,
    list_width: f32,
    resize_start: Option<(f32, f32)>,
    before: Option<u32>,
    busy: bool,
    list_exhausted: bool,
    list_load_failed: bool,
    status: String,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
}
impl Mailbox {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        crate::perf_probe::mailbox_idle(window, cx);
        let mut this = Self {
            page: Page::Mail,
            pending_page: None,
            account_page: None,
            settings_page: None,
            sync_open: false,
            sync_state: sync_status::Status::default(),
            sync_refreshing: false,
            keep_operation_confirm: None,
            confirm_resend: None,
            composer: None,
            composer_subscription: None,
            composer_observation: None,
            pending_compose: None,
            pane_generation: 0,
            notification_target: None,
            observed_status: String::new(),
            store: crate::lifecycle::storage(cx),
            account: None,
            accounts: vec![],
            catalogs: BTreeMap::new(),
            closed_accounts: BTreeSet::new(),
            default_account: None,
            unified: false,
            inbox: None,
            inbox_before: None,
            search_scope: 0,
            collapsed: BTreeSet::new(),
            folders: vec![],
            catalog_at: None,
            selected_folder: None,
            cache: None,
            selected: None,
            checked: HashSet::new(),
            undo_flags: None,
            confirm_delete: false,
            history_exhausted: false,
            quiet_reload: false,
            restore_selected: None,
            search_text: cx.new(|cx| InputState::new(window, cx)),
            search_sender: cx.new(|cx| InputState::new(window, cx)),
            search_attachments: false,
            search_results: None,
            search_selected: None,
            search_previous: None,
            narrow_detail: false,
            positions: BTreeMap::new(),
            restore_uid: None,
            restore_count: 200,
            inline: None,
            reading: None,
            read_running: false,
            read_cancel: Default::default(),
            pending_read: None,
            reading_status: String::new(),
            sync_revision: 0,
            filters_open: false,
            more_open: false,
            message_more: false,
            list_width: 420.0,
            resize_start: None,
            before: None,
            busy: false,
            list_exhausted: false,
            list_load_failed: false,
            status: "正在读取本地账号与缓存…".into(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
        };
        this.reload(cx);
        Self::start_compose_probe(window, cx);
        Self::start_drafts_probe(window, cx);
        Self::start_motion_probe(window, cx);
        if std::env::var("QINGMAIL_WINDOW_PERF").as_deref() == Ok("interaction")
            && std::env::var_os("QINGMAIL_IPC_ROOT").is_some()
        {
            cx.spawn_in(window, async move |entity, cx| {
                // Isolated synthetic data only. Exercise the real widgets and async
                // mailbox pipeline; timings end at state completion, not presentation.
                for step in 0..6 {
                    let started = std::time::Instant::now();
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(25))
                            .await;
                        let ready = entity
                            .update(cx, |this, _| !this.busy && !this.read_running)
                            .expect("performance mailbox alive");
                        assert!(started.elapsed().as_secs() < 20, "mailbox action timeout");
                        if ready {
                            break;
                        }
                    }
                    entity.update_in(cx, |this, window, cx| {
                        if step > 0 {
                            eprintln!("MAILBOX_ACTION step={} completed_ms={:.2}", step - 1, started.elapsed().as_secs_f64() * 1000.);
                        }
                        match step {
                            0 => {
                                assert_eq!(this.accounts.len(), 5);
                                assert_eq!(this.messages().len(), 100);
                                for index in 0..30 { this.select_message(index, cx); }
                            }
                            1 => {
                                assert_eq!(this.inline.as_ref().unwrap().location.as_ref(), Some(this.selected_location().unwrap()));
                                this.account = this.accounts.first().cloned();
                                this.search_scope = 2;
                                this.search_text.update(cx, |input, cx| input.set_value("中文性能样本", window, cx));
                                this.run_search(cx);
                            }
                            2 => {
                                assert!(!this.search_results.as_ref().unwrap().is_empty());
                                this.search_results = None;
                                this.search_previous = None;
                                this.account = this.accounts.first().cloned();
                                this.select_folder("INBOX".into(), cx);
                            }
                            3 => {
                                assert_eq!(this.messages().len(), 200);
                                this.before = Some(9801);
                                this.load_cache(cx);
                            }
                            4 => {
                                assert_eq!(this.messages().len(), 200);
                                assert!(this.messages().iter().all(|m| m.location.uid < 9801));
                                this.narrow_detail = true;
                                window.resize(size(px(900.), px(760.)));
                                this.before = None;
                                this.load_cache(cx);
                            }
                            _ => {
                                assert_eq!(this.messages()[0].location.uid, 10_000);
                                eprintln!("MAILBOX_ACTION PASS rapid_selection=30 search=Chinese paging=200 narrow_requested=900x760 scale={} viewport={:?}",window.scale_factor(), window.viewport_size());
                                crate::lifecycle::close_frontend(cx);
                            }
                        }
                    }).expect("performance mailbox action");
                }
            }).detach();
        }
        cx.spawn(async move |entity, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                if entity
                    .update(cx, |this, cx| {
                        if !this.busy && this.notification_target.is_some() {
                            this.apply_notification(cx);
                        }
                        if !this.busy && this.sync_revision != crate::lifecycle::sync_revision(cx) {
                            this.refresh_local(cx);
                        }
                        // Queue progress is independent of the current folder or an
                        // open popup. Keep the icon and its open contents live.
                        this.refresh_sync_status(cx);
                        let status = format!(
                            "{}|{}",
                            crate::lifecycle::sync_status(cx),
                            crate::lifecycle::operation_summary(cx)
                        );
                        if this.observed_status != status {
                            this.observed_status = status;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        this.refresh_sync_status(cx);
        if std::env::args().any(|a| a == "--mailbox-ui-probe")
            || std::env::var_os("QINGMAIL_IPC_TEST_MAILBOX").is_some()
        {
            cx.spawn_in(window, async move |entity, cx| {
                for step in 0..12 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(900))
                        .await;
                    for _ in 0..40 {
                        if !entity
                            .update(cx, |this, _| this.busy)
                            .expect("mailbox UI state")
                        {
                            break;
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(150))
                            .await;
                    }
                    entity
                        .update_in(cx, |this, window, cx| this.ui_probe(step, window, cx))
                        .expect("mailbox UI probe");
                }
            })
            .detach();
        }
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
    pub(super) fn pages_saving(&self, cx: &App) -> bool {
        self.account_page
            .as_ref()
            .is_some_and(|p| p.read(cx).saving)
            || self
                .settings_page
                .as_ref()
                .is_some_and(|p| p.read(cx).saving())
    }
    pub(super) fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        self.pane_generation += 1;
        self.pending_compose = None;
        self.pending_page = None;
        if let Some(editor) = &self.composer
            && (page != Page::Mail || editor.read(cx).workspace_folder().is_some())
        {
            self.pending_page = Some(page);
            editor.update(cx, |editor, cx| editor.request_close(cx));
            return;
        }
        if page == Page::Accounts && self.account_page.is_none() {
            self.account_page = Some(cx.new(|cx| crate::accounts::Accounts::new(window, cx)));
        }
        if page == Page::Settings && self.settings_page.is_none() {
            self.settings_page = Some(cx.new(crate::settings::Settings::new));
        }
        if page == Page::Drafts
            && let (Some((account, settings)), Some(folder)) =
                (&self.account, &self.selected_folder)
        {
            let account = (account.clone(), settings.address.clone());
            let folder = folder.clone();
            let editor =
                cx.new(|cx| crate::composer::Composer::new_workspace(account, folder, window, cx));
            self.install_editor(editor, window, cx);
        }
        self.page = page;
        self.more_open = false;
        self.resize_start = None;
        cx.notify();
    }
    fn show_selected_drafts(&mut self, cx: &mut Context<Self>) {
        self.pane_generation += 1;
        self.pending_compose = None;
        self.pending_page = None;
        if let Some(editor) = &self.composer {
            self.pending_page = Some(Page::Drafts);
            editor.update(cx, |editor, cx| editor.request_close(cx));
        } else {
            cx.defer(|cx| crate::lifecycle::open_page(Page::Drafts, cx));
        }
        cx.notify();
    }
    pub(super) fn open_sync_status(&mut self, cx: &mut Context<Self>) {
        self.sync_open = true;
        self.keep_operation_confirm = None;
        self.refresh_sync_status(cx);
        cx.notify();
    }
    pub(super) fn refresh_sync_status(&mut self, cx: &mut Context<Self>) {
        if self.sync_refreshing {
            return;
        }
        self.sync_refreshing = true;
        let store = self.store.clone();
        let task = cx
            .background_executor()
            .spawn(async move { sync_status::read(&store) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.sync_refreshing = false;
                this.sync_state.update(result);
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn run_sync_operation(
        &mut self,
        command: mail_protocol::Command,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let task = cx
            .background_executor()
            .spawn(async move { store.control(command) });
        cx.spawn(async move |entity, cx| {
            let _ = task.await;
            let _ = entity.update(cx, |this, cx| this.refresh_sync_status(cx));
        })
        .detach();
    }
    pub(super) fn confirm_resend(
        &mut self,
        account: AccountId,
        task: mail_core::SendTaskId,
        cx: &mut Context<Self>,
    ) {
        self.confirm_resend = None;
        let reply = crate::lifecycle::sending(cx).resend(self.store.clone(), account, task);
        let future = cx
            .background_executor()
            .spawn(async move { receive(reply) });
        cx.spawn(async move |entity, cx| {
            let _ = future.await;
            let _ = entity.update(cx, |this, cx| this.refresh_sync_status(cx));
        })
        .detach();
    }
    pub(super) fn compose(
        &mut self,
        account: Option<(AccountId, String)>,
        document: Option<mail_storage::DraftDocument>,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.page == Page::Drafts
            && document.is_none()
            && self.account.as_ref().map(|(id, _)| id) == account.as_ref().map(|(id, _)| id);
        if !workspace {
            self.page = Page::Mail;
        }
        self.pending_page = None;
        self.pane_generation += 1;
        if let Some(editor) = &self.composer {
            if !force
                && document.is_none()
                && !editor.read(cx).leaving()
                && editor.read(cx).account_id() == account.as_ref().map(|(id, _)| id)
            {
                if workspace {
                    editor.update(cx, |editor, cx| editor.new_draft(window, cx));
                }
                self.narrow_detail = true;
                cx.notify();
                return;
            }
            self.pending_compose = Some((account, document, force));
            editor.update(cx, |editor, cx| editor.request_close(cx));
            return;
        }
        let editor = if workspace {
            let folder = self.selected_folder.clone().expect("draft folder");
            cx.new(|cx| {
                crate::composer::Composer::new_workspace(
                    account.expect("account"),
                    folder,
                    window,
                    cx,
                )
            })
        } else {
            cx.new(|cx| crate::composer::Composer::new(account, document, window, cx))
        };
        self.install_editor(editor, window, cx);
    }
    fn install_editor(
        &mut self,
        editor: Entity<crate::composer::Composer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer_observation = Some(cx.observe(&editor, |_, _, cx| cx.notify()));
        self.composer_subscription = Some(cx.subscribe_in(
            &editor,
            window,
            |this, editor, event: &crate::composer::ComposerEvent, window, cx| {
                if let crate::composer::ComposerEvent::Sent(account, task) = event {
                    this.open_local_sent_record(account.clone(), task.clone(), cx);
                    return;
                }
                if matches!(event, crate::composer::ComposerEvent::CloseCancelled) {
                    if let Some(folder) = editor.read(cx).workspace_folder() {
                        this.page = Page::Drafts;
                        this.selected_folder = Some(folder.to_owned());
                        this.account = this
                            .accounts
                            .iter()
                            .find(|(id, _)| Some(id) == editor.read(cx).account_id())
                            .cloned();
                        if let Some((account, _)) = &this.account {
                            let (folders, at) =
                                this.catalogs.get(&account.0).cloned().unwrap_or_default();
                            this.folders = folders;
                            this.catalog_at = at;
                        }
                    }
                    this.pending_compose = None;
                    this.pending_page = None;
                    cx.notify();
                    return;
                }
                this.composer = None;
                this.composer_subscription = None;
                this.composer_observation = None;
                if let Some(page) = this.pending_page.take() {
                    let host = cx.weak_entity();
                    let generation = this.pane_generation;
                    window.on_next_frame(move |window, cx| {
                        let _ = host.update(cx, |this, cx| {
                            if this.pane_generation == generation
                                && !crate::lifecycle::is_closing(cx)
                            {
                                this.show_page(page, window, cx);
                            }
                        });
                    });
                }
                if let Some((account, document, force)) = this.pending_compose.take()
                    && !crate::lifecycle::is_closing(cx)
                {
                    // Defer replacement until the old close event and lifecycle participant are released.
                    let host = cx.weak_entity();
                    let generation = this.pane_generation;
                    window.on_next_frame(move |window, cx| {
                        let _ = host.update(cx, |this, cx| {
                            if this.pane_generation == generation
                                && !crate::lifecycle::is_closing(cx)
                            {
                                this.compose(account, document, force, window, cx);
                            }
                        });
                    });
                }
                cx.notify();
            },
        ));
        crate::lifecycle::register_editor(window.window_handle(), editor.downgrade(), cx);
        self.composer = Some(editor);
        self.narrow_detail = true;
        cx.notify();
    }
    fn leave_composer(&mut self, cx: &mut Context<Self>) {
        self.page = Page::Mail;
        self.pending_page = None;
        self.pane_generation += 1;
        self.pending_compose = None;
        if let Some(editor) = &self.composer {
            editor.update(cx, |editor, cx| editor.request_close(cx));
        }
    }
    pub(super) fn locate_notification(
        &mut self,
        target: mail_protocol::OpenMailTarget,
        cx: &mut Context<Self>,
    ) {
        self.notification_target = Some(target);
        if !self.busy {
            self.apply_notification(cx);
        }
    }
    fn apply_notification(&mut self, cx: &mut Context<Self>) {
        self.page = Page::Mail;
        self.pending_page = None;
        cx.notify();
        let Some(target) = self.notification_target.take() else {
            return;
        };
        self.busy = true;
        self.invalidate_read();
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let account = receive(store.accounts())?
                .into_iter()
                .find(|(id, _)| id == &target.account)
                .ok_or("通知所属账号已移除")?;
            let folders = receive(store.folders(target.account.clone()))?;
            let folder = folders
                .into_iter()
                .find(|f| match &target.message {
                    Some(m) => f.id.0 == m.folder_id && f.uidvalidity == m.uidvalidity,
                    None => f.remote_name.eq_ignore_ascii_case("INBOX"),
                })
                .ok_or("通知邮件已失效或收件箱尚未缓存")?;
            let before = target.message.as_ref().and_then(|m| m.uid.checked_add(1));
            let cache = receive(store.folder_cache(
                target.account.clone(),
                folder.remote_name.clone(),
                before,
            ))?;
            let selected = target.message.as_ref().and_then(|m| {
                cache
                    .as_ref()
                    .and_then(|c| c.messages.iter().position(|v| v.location == *m))
            });
            if target.message.is_some() && selected.is_none() {
                return Err("通知邮件已删除或不在当前缓存中，请返回收件箱".to_string());
            }
            Ok::<_, String>((account, folder, cache, before, selected))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((account, folder, cache, before, selected)) => {
                        let (catalog, at) =
                            this.catalogs.get(&account.0.0).cloned().unwrap_or_default();
                        this.account = Some(account);
                        this.folders = catalog;
                        this.catalog_at = at;
                        this.unified = false;
                        this.inbox = None;
                        this.inbox_before = None;
                        this.selected_folder = Some(folder.remote_name);
                        this.before = before;
                        this.search_results = None;
                        this.search_previous = None;
                        this.search_selected = None;
                        this.checked.clear();
                        this.undo_flags = None;
                        this.confirm_delete = false;
                        this.list_exhausted = cache.as_ref().is_none_or(|c| c.messages.len() < 200);
                        this.list_load_failed = false;
                        this.restore_count = 200;
                        this.cache = cache;
                        this.selected = selected;
                        this.restore_uid = None;
                        this.restore_selected = None;
                        this.scroll = UniformListScrollHandle::new();
                        this.narrow_detail = selected.is_some();
                        this.status = "已定位通知邮件".into();
                        if selected.is_some() {
                            this.read_message(cx);
                        }
                    }
                    Err(e) => {
                        this.status = e;
                    }
                }
                if this.notification_target.is_some() {
                    this.apply_notification(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn position_key(&self, folder: &str) -> String {
        format!(
            "{}:{folder}",
            self.account
                .as_ref()
                .map(|(id, _)| id.0.as_str())
                .unwrap_or("")
        )
    }
    fn switch_account(&mut self, id: AccountId, folder: String, cx: &mut Context<Self>) {
        self.page = Page::Mail;
        self.pending_page = None;
        cx.notify();
        if self.busy {
            return;
        }
        if self.account.as_ref().map(|(a, _)| a) != Some(&id) || self.unified {
            if let Some(current) = self.selected_folder.as_ref() {
                let uid = self.selected_location().map(|loc| loc.uid);
                self.positions.insert(
                    self.position_key(current),
                    (uid, self.before, self.messages().len()),
                );
            }
            self.selected_folder = None;
            self.account = self.accounts.iter().find(|(a, _)| a == &id).cloned();
            let (folders, at) = self.catalogs.get(&id.0).cloned().unwrap_or_default();
            self.folders = folders;
            self.catalog_at = at;
        }
        self.select_folder(folder, cx);
    }
    fn compose_account(&self) -> Option<(AccountId, AccountSettings)> {
        if self.unified {
            self.accounts
                .iter()
                .find(|(id, _)| Some(id) == self.default_account.as_ref())
                .cloned()
        } else {
            self.account.clone()
        }
    }
    fn select_unified(&mut self, cx: &mut Context<Self>) {
        self.page = Page::Mail;
        self.pending_page = None;
        cx.notify();
        if self.busy {
            return;
        }
        self.unified = true;
        self.selected_folder = None;
        self.search_scope = 2;
        self.selected = None;
        self.checked.clear();
        self.undo_flags = None;
        self.search_results = None;
        self.search_previous = None;
        self.inbox_before = None;
        self.invalidate_read();
        self.load_unified(cx);
    }
    fn load_unified(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let store = self.store.clone();
        let before = self.inbox_before.clone();
        let target = if self.quiet_reload {
            self.messages().len().max(100)
        } else {
            100
        };
        let revision = crate::lifecycle::sync_revision(cx);
        let keep_status = std::mem::take(&mut self.quiet_reload);
        let saved_status = self.status.clone();
        let task = cx.background_executor().spawn(async move {
            let accounts = receive(store.accounts())?;
            let mut catalogs = BTreeMap::new();
            for (id, _) in &accounts {
                catalogs.insert(id.0.clone(), receive(store.catalog(id.clone()))?);
            }
            let default = match store.control(mail_protocol::Command::AccountOptions)? {
                Reply::AccountOptions { default, .. } => default,
                _ => None,
            };
            let page = loading::inbox_window(&store, before, target)?;
            Ok::<_, String>((page, accounts, catalogs, default))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.quiet_reload = false;
                this.sync_revision = revision;
                match result {
                    Ok((page, accounts, catalogs, default)) => {
                        let location = this.selected_location().cloned();
                        if !keep_status {
                            this.scroll = UniformListScrollHandle::new();
                        }
                        this.list_exhausted = page.next.is_none();
                        this.list_load_failed = false;
                        this.inbox = Some(page);
                        let visible: HashSet<_> =
                            this.messages().iter().map(|m| m.location.clone()).collect();
                        this.checked.retain(|loc| visible.contains(loc));
                        this.accounts = accounts;
                        this.catalogs = catalogs;
                        this.default_account = default;
                        this.account = this
                            .accounts
                            .iter()
                            .find(|(id, _)| Some(id) == this.default_account.as_ref())
                            .cloned();
                        this.selected = location
                            .and_then(|loc| this.messages().iter().position(|m| m.location == loc));
                        this.status = if keep_status {
                            saved_status
                        } else {
                            format!(
                                "统一收件箱 · {} 封 · {} 个账号",
                                this.messages().len(),
                                this.accounts.len()
                            )
                        };
                        if this.selected.is_some() {
                            this.read_message(cx);
                        } else {
                            this.invalidate_read();
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
    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let accounts = receive(store.accounts())?;
            let mut catalogs = BTreeMap::new();
            for (id, _) in &accounts {
                catalogs.insert(id.0.clone(), receive(store.catalog(id.clone()))?);
            }
            let default = match store.control(mail_protocol::Command::AccountOptions)? {
                Reply::AccountOptions { default, .. } => default,
                _ => None,
            };
            let account = accounts.first().cloned();
            let catalog = if let Some((id, _)) = &account {
                receive(store.catalog(id.clone()))?
            } else {
                (vec![], None)
            };
            Ok::<_, String>((account, catalog, accounts, catalogs, default))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((account, (folders, at), accounts, catalogs, default)) => {
                        this.accounts = accounts;
                        this.catalogs = catalogs;
                        this.default_account = default;
                        this.account = account;
                        this.folders = folders;
                        this.catalog_at = at;
                        this.invalidate_read();
                        this.selected_folder = None;
                        this.cache = None;
                        this.selected = None;
                        this.status = if this.account.is_none() {
                            "尚未添加邮箱，请先添加通用邮箱".into()
                        } else if at.is_none() {
                            "正在等待首次同步；本地邮件可离线阅读".into()
                        } else {
                            "已加载本地文件夹目录。选择文件夹查看缓存，可离线浏览".into()
                        };
                    }
                    Err(e) => this.status = e,
                };
                if this.accounts.len() > 1 {
                    this.select_unified(cx);
                } else if let Some(folder) = this
                    .folders
                    .iter()
                    .find(|f| f.selectable && f.name.eq_ignore_ascii_case("INBOX"))
                {
                    this.select_folder(folder.name.clone(), cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn refresh_local(&mut self, cx: &mut Context<Self>) {
        if self.unified {
            self.quiet_reload = true;
            self.load_unified(cx);
            return;
        }
        let Some((account, _)) = self.account.clone() else {
            self.reload(cx);
            return;
        };
        self.busy = true;
        let revision = crate::lifecycle::sync_revision(cx);
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let accounts = receive(store.accounts())?;
            let mut catalogs = BTreeMap::new();
            for (id, _) in &accounts {
                catalogs.insert(id.0.clone(), receive(store.catalog(id.clone()))?);
            }
            let catalog = catalogs.get(&account.0).cloned().unwrap_or_default();
            Ok::<_, String>((accounts, catalogs, catalog))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                if let Ok((accounts, catalogs, (folders, at))) = result {
                    this.accounts = accounts;
                    this.catalogs = catalogs;
                    this.account = this.account.as_ref().and_then(|(id, _)| {
                        this.accounts
                            .iter()
                            .find(|(new_id, _)| id == new_id)
                            .cloned()
                    });
                    if this.account.is_none() {
                        this.selected_folder = None;
                        this.cache = None;
                        this.selected = None;
                        this.invalidate_read();
                        this.reload(cx);
                        return;
                    }
                    this.folders = folders;
                    this.catalog_at = at;
                    this.sync_revision = revision;
                    if this.selected_folder.is_some() && this.search_results.is_none() {
                        this.quiet_reload = true;
                        this.load_cache(cx);
                    } else if this.selected_folder.is_none()
                        && let Some(folder) = this
                            .folders
                            .iter()
                            .find(|f| f.selectable && f.name.eq_ignore_ascii_case("INBOX"))
                    {
                        this.select_folder(folder.name.clone(), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn invalidate_read(&mut self) {
        self.read_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        self.reading = None;
        self.pending_read = None;
        self.inline = None;
        self.reading_status.clear();
    }
    fn selected_location(&self) -> Option<&mail_core::MailUid> {
        if let Some(results) = &self.search_results {
            self.search_selected
                .and_then(|index| results.get(index))
                .map(|hit| &hit.message.location)
        } else {
            self.selected
                .and_then(|index| self.messages().get(index))
                .map(|message| &message.location)
        }
    }
    fn select_message(&mut self, index: usize, cx: &mut Context<Self>) {
        self.leave_composer(cx);
        self.selected = Some(index);
        self.confirm_delete = false;
        self.read_message(cx);
        cx.notify();
    }
    fn select_folder(&mut self, name: String, cx: &mut Context<Self>) {
        if let Some(editor) = &self.composer
            && editor.read(cx).workspace_folder().is_some()
        {
            editor.update(cx, |editor, cx| editor.request_close(cx));
        }
        self.page = Page::Mail;
        self.pending_page = None;
        cx.notify();
        if self.busy {
            return;
        }
        if let Some(current) = self.selected_folder.clone() {
            let uid = self
                .selected
                .and_then(|index| self.messages().get(index))
                .map(|message| message.location.uid);
            self.positions.insert(
                self.position_key(&current),
                (uid, self.before, self.messages().len()),
            );
        }
        let (restore_uid, restore_before, restore_count) = self
            .positions
            .get(&self.position_key(&name))
            .copied()
            .unwrap_or((None, None, 200));
        self.invalidate_read();
        self.restore_uid = restore_uid;
        self.narrow_detail = false;
        self.unified = false;
        self.search_scope = 0;
        self.selected_folder = Some(name);
        self.before = restore_before;
        self.restore_count = restore_count;
        self.search_results = None;
        self.search_previous = None;
        self.search_selected = None;
        self.checked.clear();
        self.undo_flags = None;
        self.confirm_delete = false;
        self.history_exhausted = false;
        if self.selected_folder.as_deref().is_some_and(|name| {
            is_drafts_folder(name)
                || self
                    .folders
                    .iter()
                    .any(|f| f.name == name && is_drafts_folder(&f.display_name))
        }) {
            self.show_selected_drafts(cx);
            return;
        }
        self.load_cache(cx);
    }
    fn load_cache(&mut self, cx: &mut Context<Self>) {
        if self.unified {
            self.load_unified(cx);
            return;
        }
        let Some((account, _)) = self.account.clone() else {
            return;
        };
        let Some(name) = self.selected_folder.clone() else {
            return;
        };
        self.busy = true;
        let quiet = self.quiet_reload;
        if !quiet {
            self.selected = None;
            self.invalidate_read();
            self.cache = None;
        }
        let store = self.store.clone();
        let before = self.before;
        let target = if quiet {
            self.messages().len().max(200)
        } else {
            self.restore_count.max(200)
        };
        let task = cx
            .background_executor()
            .spawn(async move { loading::folder_window(&store, account, name, before, target) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((cache, exhausted)) => {
                        this.list_exhausted = exhausted;
                        this.list_load_failed = false;
                        // 静默刷新期间用户仍可选信，保留完成时的选择而非请求发起时的旧选择。
                        let selected_uid = this.selected_location().map(|location| location.uid);
                        this.cache = cache;
                        let visible: HashSet<_> =
                            this.messages().iter().map(|m| m.location.clone()).collect();
                        this.checked.retain(|loc| visible.contains(loc));
                        if !quiet {
                            this.scroll = UniformListScrollHandle::new();
                        }
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
                        } else if quiet && let Some(uid) = selected_uid {
                            this.selected = this
                                .messages()
                                .iter()
                                .position(|message| message.location.uid == uid);
                        }
                        if this.selected.is_some() {
                            this.read_message(cx);
                        } else {
                            this.invalidate_read();
                        }
                        if !quiet {
                            this.status = match &this.cache {
                                None => "这个文件夹尚无本地邮件；同步完成后会自动更新".into(),
                                Some(cache) => format!(
                                    "{} 封 · 上次更新 {}",
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
                if this.notification_target.is_some() {
                    this.apply_notification(cx);
                }
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
        let Some((id, _)) = self.account.clone() else {
            return;
        };
        self.busy = true;
        self.status = "正在获取服务器文件夹…".into();
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            if let Some(client) = store.ipc_client() {
                return match mail_daemon::client::run(&client, Op::RefreshCatalog { account: id })?
                {
                    Reply::Catalog { folders, .. } => Ok(folders),
                    _ => Err("目录响应无效".into()),
                };
            }
            let _guard = NETWORK
                .try_lock()
                .map_err(|_| "另一个同步检查仍在结束，请稍后重试".to_owned())?;
            let access = receive(store.account_version(id.clone()))?;
            let settings = access.settings;
            let credentials = SystemVault
                .load(&access.credential_key)
                .map_err(|e| e.to_string())?;
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
        let remote = self.store.ipc_client().is_some();
        let request = (!remote).then(|| {
            self.store
                .publish_catalog(account.clone(), folders.clone(), at)
        });
        let task = cx.background_executor().spawn(async move {
            match request {
                Some(request) => receive(request),
                None => Ok(()),
            }
        });
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
        if self.unified && !older {
            if let Some(client) = self.store.ipc_client() {
                self.status = "已请求同步所有账号".into();
                cx.background_executor()
                    .spawn(async move {
                        let _ = client.request(mail_protocol::Command::SyncNow {
                            account: None,
                            folder: None,
                            before: None,
                        });
                    })
                    .detach();
                cx.notify();
            }
            return;
        }
        if self.busy {
            return;
        }
        let Some((id, _)) = self.account.clone() else {
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
        if let Some(client) = self.store.ipc_client() {
            self.busy = true;
            self.status = "正在同步文件夹…".into();
            let task = cx.background_executor().spawn(async move {
                mail_daemon::client::run(
                    &client,
                    Op::SyncFolder {
                        account: id,
                        name,
                        before,
                        history: false,
                    },
                )
            });
            cx.spawn(async move |entity, cx| {
                let result = task.await;
                let _ = entity.update(cx, |this, cx| {
                    this.busy = false;
                    match result {
                        Ok(_) => {
                            if !older {
                                this.before = None;
                            }
                            this.quiet_reload = true;
                            this.load_cache(cx);
                        }
                        Err(e) => {
                            this.status = format!("同步失败：{e}，缓存保留");
                            cx.notify();
                        }
                    }
                });
            })
            .detach();
            cx.notify();
            return;
        }
        let expected_epoch = self.cache.as_ref().map(|cache| cache.folder.uidvalidity);
        self.busy = true;
        self.status = "正在只读检查文件夹并获取最近90天的邮件头…".into();
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let _guard = NETWORK
                .try_lock()
                .map_err(|_| "另一个同步检查仍在结束，请稍后重试".to_owned())?;
            let access = receive(store.account_version(id.clone()))?;
            let settings = access.settings;
            let credentials = SystemVault
                .load(&access.credential_key)
                .map_err(|e| e.to_string())?;
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
                                    if !older {
                                        this.before = None;
                                    }
                                    this.quiet_reload = true;
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
        let Some(message) = self
            .selected
            .and_then(|index| self.messages().get(index))
            .cloned()
        else {
            return;
        };
        let name = if self.unified {
            self.catalogs
                .get(&message.location.account.0)
                .and_then(|(folders, _)| {
                    folders
                        .iter()
                        .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
                })
                .map(|f| f.name.clone())
        } else {
            self.selected_folder.clone()
        };
        let Some(name) = name else {
            return;
        };
        self.read_location(message.location, name, cx);
    }
    fn open_hit(&mut self, index: usize, cx: &mut Context<Self>) {
        self.leave_composer(cx);
        let Some(hit) = self
            .search_results
            .as_ref()
            .and_then(|results| results.get(index))
            .cloned()
        else {
            return;
        };
        self.search_selected = Some(index);
        self.narrow_detail = true;
        self.read_location(hit.message.location, hit.folder, cx);
    }
    fn read_location(
        &mut self,
        location: mail_core::MailUid,
        folder: String,
        cx: &mut Context<Self>,
    ) {
        let Some((account, settings)) = self
            .accounts
            .iter()
            .find(|(id, _)| *id == location.account)
            .cloned()
        else {
            return;
        };
        self.reading = Some(location.clone());
        if self
            .inline
            .as_ref()
            .is_some_and(|inline| inline.location.as_ref() == Some(&location))
        {
            self.reading_status.clear();
            return;
        }
        self.reading_status = "正在加载邮件…".into();
        // 仅一个正文任务在执行；快速选择合并为最后一封，避免积压网络连接。
        if self.read_running {
            self.read_cancel
                .store(true, std::sync::atomic::Ordering::Release);
            self.pending_read = Some((location, folder));
            cx.notify();
            return;
        }
        self.read_running = true;
        self.read_cancel = Default::default();
        let cancel = self.read_cancel.clone();
        let reader_account = Some((account.clone(), settings.address.clone()));
        let inline_location = location.clone();
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            if let Some(client) = store.ipc_client() {
                let content = mail_daemon::client::content_cancellable(
                    &client,
                    Op::Read {
                        location: location.clone(),
                        folder: folder.clone(),
                    },
                    &cancel,
                )?;
                let delivery = receive(store.tasks(account.clone()))
                    .ok()
                    .and_then(|tasks| {
                        tasks.into_iter().find(|task| {
                            task.copy.as_ref().is_some_and(|copy| {
                                copy.folder == folder
                                    && copy.uid == Some(location.uid)
                                    && copy.uidvalidity == location.uidvalidity
                            })
                        })
                    })
                    .map(|task| crate::send_labels::state_label(&task));
                return Ok((content, account, reader_account, delivery));
            }
            let raw = match receive(store.message_body(location.clone()))? {
                Some(raw) => raw,
                None => {
                    let _guard = NETWORK
                        .try_lock()
                        .map_err(|_| "另一个网络请求仍在结束，请稍后重试".to_owned())?;
                    let access = receive(store.account_version(account.clone()))?;
                    let settings = access.settings;
                    let credentials = SystemVault
                        .load(&access.credential_key)
                        .map_err(|e| e.to_string())?;
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
            let delivery = receive(store.tasks(account.clone()))
                .ok()
                .and_then(|tasks| {
                    tasks.into_iter().find(|task| {
                        task.copy.as_ref().is_some_and(|copy| {
                            copy.folder == folder
                                && copy.uid == Some(location.uid)
                                && copy.uidvalidity == location.uidvalidity
                        })
                    })
                })
                .map(|task| crate::send_labels::state_label(&task));
            // 记录附件数量，供本地“有附件”搜索使用；失败不影响阅读。
            let _ = receive(store.mark_attachments(location, content.attachments.len() as u32));
            Ok::<_, String>((content, account, reader_account, delivery))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.read_running = false;
                let current = this.reading.as_ref() == Some(&inline_location)
                    && this.selected_location() == Some(&inline_location);
                if current {
                    match result {
                        Ok((content, _, reader_account, delivery)) => {
                            let view = cx.new(|cx| {
                                crate::reader::Reader::embedded(
                                    content.clone(),
                                    reader_account.clone(),
                                    delivery,
                                    cx,
                                )
                            });
                            this.inline = Some(InlineReading {
                                location: Some(inline_location),
                                account: reader_account,
                                content,
                                view,
                            });
                            this.reading_status.clear();
                        }
                        Err(e) => {
                            this.reading_status = format!("无法加载邮件：{e}。可重新点击邮件重试")
                        }
                    }
                }
                if let Some((location, folder)) = this.pending_read.take()
                    && this.selected_location() == Some(&location)
                {
                    this.read_location(location, folder, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_local_sent_record(
        &mut self,
        account: AccountId,
        task: mail_core::SendTaskId,
        cx: &mut Context<Self>,
    ) {
        self.page = Page::Mail;
        self.pending_page = None;
        self.unified = false;
        self.search_results = None;
        self.search_previous = None;
        self.search_selected = None;
        self.selected = None;
        self.selected_folder = self
            .catalogs
            .get(&account.0)
            .and_then(|(folders, _)| {
                folders.iter().find(|folder| {
                    let name = folder.name.to_ascii_lowercase();
                    name == "sent"
                        || name == "sent items"
                        || name == "sent messages"
                        || folder.display_name.contains("已发送")
                        || folder.display_name.contains("已发件")
                })
            })
            .map(|folder| folder.name.clone());
        self.account = self.accounts.iter().find(|(id, _)| id == &account).cloned();
        if let Some((folders, at)) = self.catalogs.get(&account.0).cloned() {
            self.folders = folders;
            self.catalog_at = at;
        }
        self.invalidate_read();
        self.cache = None;
        self.inbox = None;
        self.narrow_detail = true;
        self.reading_status = "正在打开本地发送记录…".into();
        if self.selected_folder.is_some() {
            self.quiet_reload = false;
            self.load_cache(cx);
        }
        let store = self.store.clone();
        let task = cx.background_executor().spawn(async move {
            let snapshot = receive(store.task_snapshot(account.clone(), task))?;
            let mime = mail_service::outgoing::prepare(&snapshot, true)
                .map_err(|error| error.to_string())?;
            let content = mail_content::parse(mime.raw()).map_err(|error| error.to_string())?;
            let settings = receive(store.accounts())?
                .into_iter()
                .find(|(id, _)| id == &account)
                .ok_or("发送记录所属账号已移除")?;
            Ok::<_, String>((account, snapshot, content, settings.1.address))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                match result {
                    Ok((account, snapshot, content, address)) => {
                        let view = cx.new(|cx| {
                            crate::reader::Reader::embedded(
                                content.clone(),
                                Some((account.clone(), address)),
                                Some("本地发送记录".into()),
                                cx,
                            )
                        });
                        this.inline = Some(InlineReading {
                            location: None,
                            account: this
                                .account
                                .as_ref()
                                .map(|(id, settings)| (id.clone(), settings.address.clone())),
                            content,
                            view,
                        });
                        this.reading_status.clear();
                        this.status = format!("已发送：{}", snapshot.headers.subject);
                    }
                    Err(error) => {
                        this.reading_status = format!("本地发送记录暂不可用：{error}");
                    }
                }
                this.sync_revision = crate::lifecycle::sync_revision(cx);
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
        let folder = if self.search_scope == 0 {
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
        let scope = self.search_scope;
        let store = self.store.clone();
        let reply = cx.background_executor().spawn(async move {
            match store.control(mail_protocol::Command::Search {
                account: (scope != 2).then_some(account),
                query,
            })? {
                Reply::Search(hits) => Ok::<_, String>(hits),
                _ => Err("搜索响应无效".into()),
            }
        });
        let task = reply;
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(results) => {
                        this.status = format!(
                            "本地搜索：{} 封 · {} · 仅覆盖已缓存索引；附件筛选只统计已下载正文",
                            results.len(),
                            if scope == 2 {
                                "全部本地缓存"
                            } else if folder.is_some() {
                                "当前文件夹"
                            } else {
                                "整个账号"
                            }
                        );
                        this.invalidate_read();
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
        self.invalidate_read();
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
            return self.checked.iter().map(|location| location.uid).collect();
        }
        self.selected
            .and_then(|index| self.messages().get(index))
            .map(|message| vec![message.location.uid])
            .unwrap_or_default()
    }
    fn toggle_checked(&mut self, uid: mail_core::MailUid, cx: &mut Context<Self>) {
        if !self.checked.remove(&uid) {
            self.checked.insert(uid);
        }
        self.confirm_delete = false;
        cx.notify();
    }
    fn unified_action(
        &mut self,
        flags: Option<(Option<bool>, Option<bool>)>,
        trash: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        let locations: Vec<_> = if self.checked.is_empty() {
            self.selected_location().cloned().into_iter().collect()
        } else {
            self.checked.iter().cloned().collect()
        };
        if locations.is_empty() {
            return;
        }
        let store = self.store.clone();
        let accounts = self.accounts.clone();
        let catalogs = self.catalogs.clone();
        self.busy = true;
        self.confirm_delete = false;
        self.undo_flags = None;
        self.status = "正在分别处理各账号邮件…".into();
        let task = cx.background_executor().spawn(async move {
            store.ipc_client().ok_or("统一操作需要独立后台")?;
            let mut groups = BTreeMap::<(String, String, u32), Vec<u32>>::new();
            for location in locations {
                groups
                    .entry((location.account.0, location.folder_id, location.uidvalidity))
                    .or_default()
                    .push(location.uid);
            }
            let mut count = 0u64;
            let mut errors = vec![];
            for ((id, folder_id, epoch), uids) in groups {
                let account = AccountId(id.clone());
                let label = accounts
                    .iter()
                    .find(|(a, _)| a == &account)
                    .map(|(_, s)| s.address.as_str())
                    .unwrap_or(&id);
                let result: Result<u64, String> = (|| {
                    let folders = receive(store.folders(account.clone()))?;
                    let folder = folders
                        .iter()
                        .find(|f| f.id.0 == folder_id && f.uidvalidity == epoch)
                        .ok_or("邮件定位已变化，请刷新")?;
                    let operation = if let Some((seen, flagged)) = flags {
                        Op::Flags {
                            account: account.clone(),
                            name: folder.remote_name.clone(),
                            epoch,
                            uids,
                            seen,
                            flagged,
                        }
                    } else {
                        let catalog = catalogs.get(&id).ok_or("账号目录尚未缓存")?;
                        let destination = if trash {
                            folders::trash(&catalog.0, None)
                        } else {
                            folders::archive(&catalog.0, None)
                        }
                        .ok_or("未找到目标文件夹；此账号未移动")?;
                        if destination == folder.remote_name && !trash {
                            return Err("邮件已在目标文件夹".into());
                        }
                        Op::Move {
                            account: account.clone(),
                            name: folder.remote_name.clone(),
                            epoch,
                            uids,
                            destination,
                        }
                    };
                    match enqueue_action(&store, operation)? {
                        Reply::Count(n)
                        | Reply::Moved {
                            count: n,
                            copied_only: false,
                            ..
                        } => Ok(n),
                        Reply::Moved {
                            copied_only: true, ..
                        } => Err(
                            "服务器已复制并标记删除，但移动未完全确认；请先同步核对，勿重复移动"
                                .into(),
                        ),
                        _ => Err("操作响应无效".into()),
                    }
                })();
                match result {
                    Ok(n) => count += n,
                    Err(e) => errors.push(format!("{label}: {e}")),
                }
            }
            Ok::<_, String>((count, errors))
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.checked.clear();
                this.status = match result {
                    Ok((n, errors)) => format!(
                        "已保存 {n} 封操作 · 待同步{}",
                        if errors.is_empty() {
                            String::new()
                        } else {
                            format!("；{}", errors.join("；"))
                        }
                    ),
                    Err(e) => e,
                };
                this.quiet_reload = true;
                this.load_unified(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn apply_flags(&mut self, seen: Option<bool>, flagged: Option<bool>, cx: &mut Context<Self>) {
        if self.unified {
            self.unified_action(Some((seen, flagged)), false, cx);
            return;
        }
        if self.busy {
            return;
        }
        let targets = self.targets();
        if targets.is_empty() {
            self.status = "请先选择邮件".into();
            cx.notify();
            return;
        }
        let Some((account, _)) = self.account.clone() else {
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
        self.status = "正在保存标记操作…".into();
        let store = self.store.clone();
        let uids = targets.clone();
        let task = cx.background_executor().spawn(async move {
            if store.ipc_client().is_some() {
                return match enqueue_action(
                    &store,
                    Op::Flags {
                        account,
                        name,
                        epoch,
                        uids,
                        seen,
                        flagged,
                    },
                )? {
                    Reply::Count(count) => Ok(count as usize),
                    _ => Err("标记响应无效".into()),
                };
            }
            let access = receive(store.account_version(account.clone()))?;
            let settings = access.settings;
            let credentials = SystemVault
                .load(&access.credential_key)
                .map_err(|e| e.to_string())?;
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
                        this.status = format!("已保存 {count} 封标记操作 · 待同步");
                        this.checked.clear();
                        this.undo_flags = undo;
                        this.quiet_reload = true;
                        this.load_cache(cx);
                    }
                    Err(error) => {
                        this.status =
                            format!("标记操作保存未确认，请查看右下角同步状态后再尝试：{error}");
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
        self.checked = self
            .messages()
            .iter()
            .filter(|m| uids.contains(&m.location.uid))
            .map(|m| m.location.clone())
            .collect();
        self.apply_flags(seen, flagged, cx);
    }
    fn move_selected(&mut self, trash: bool, cx: &mut Context<Self>) {
        if self.unified {
            self.unified_action(None, trash, cx);
            return;
        }
        if self.busy {
            return;
        }
        let targets = self.targets();
        if targets.is_empty() {
            self.status = "请先选择邮件".into();
            cx.notify();
            return;
        }
        let Some((account, _)) = self.account.clone() else {
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
        if destination == name && !trash {
            self.status = "邮件已在目标文件夹".into();
            cx.notify();
            return;
        }
        let permanent = trash && destination == name;
        self.busy = true;
        self.confirm_delete = false;
        self.status = if destination == name {
            "正在保存服务器永久删除操作…".into()
        } else {
            format!("正在移动到 {destination}…")
        };
        let store = self.store.clone();
        let uids = targets.clone();
        let task = cx.background_executor().spawn(async move {
            if store.ipc_client().is_some() {
                return match enqueue_action(
                    &store,
                    Op::Move {
                        account,
                        name,
                        epoch,
                        uids,
                        destination,
                    },
                )? {
                    Reply::Moved {
                        count,
                        copied_only,
                        destination,
                    } => Ok((
                        if copied_only {
                            actions::MoveOutcome::CopiedOnly
                        } else {
                            actions::MoveOutcome::Moved
                        },
                        count as usize,
                        destination,
                    )),
                    _ => Err("移动响应无效".into()),
                };
            }
            let access = receive(store.account_version(account.clone()))?;
            let settings = access.settings;
            let credentials = SystemVault
                .load(&access.credential_key)
                .map_err(|e| e.to_string())?;
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
                        this.status = if permanent {
                            format!("已保存 {count} 封永久删除操作 · 待同步")
                        } else { format!("已保存 {count} 封移至 {destination} 的操作 · 待同步") };
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
                    Err(error) => this.status = format!("移动操作保存未确认，请查看右下角同步状态后再尝试：{error}"),
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
        let Some((account, _)) = self.account.clone() else {
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
            if let Some(client) = store.ipc_client() {
                return match mail_daemon::client::run(
                    &client,
                    Op::SyncFolder {
                        account,
                        name,
                        before,
                        history: true,
                    },
                )? {
                    Reply::History { count, has_more } => Ok((count as usize, has_more)),
                    _ => Err("历史响应无效".into()),
                };
            }
            let access = receive(store.account_version(account.clone()))?;
            let settings = access.settings;
            let credentials = SystemVault
                .load(&access.credential_key)
                .map_err(|e| e.to_string())?;
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
        if self.unified {
            return self
                .inbox
                .as_ref()
                .map(|p| p.messages.as_slice())
                .unwrap_or(&[]);
        }
        self.cache
            .as_ref()
            .map(|cache| cache.messages.as_slice())
            .unwrap_or(&[])
    }
    fn keys(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Mail {
            return;
        }
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
            self.open_hit(index, cx);
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
        self.select_message(index, cx);
        self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        cx.notify();
    }
    fn ui_probe(&mut self, step: u8, window: &mut Window, cx: &mut Context<Self>) {
        assert!(!self.busy, "step {step}: {}", self.status);
        match step {
            0 => {
                assert_eq!(self.selected_folder.as_deref(), Some("INBOX"));
                assert_eq!(self.messages().len(), 200);
                // 同一事件内连续选择，确保执行中的第一封不会覆盖最后选择。
                self.select_message(0, cx);
                self.select_message(1, cx);
                self.select_message(2, cx);
                assert!(self.read_running);
                assert_eq!(self.pending_read.as_ref().unwrap().0.uid, 238);
            }
            1 => {
                assert!(!self.read_running);
                assert_eq!(
                    self.inline.as_ref().unwrap().location.as_ref().unwrap().uid,
                    238
                );
                self.select_message(0, cx);
                self.select_folder("Projects/2026".into(), cx);
            }
            2 => {
                assert!(self.inline.is_none());
                assert!(self.reading.is_none());
                assert!(self.messages().is_empty());
                self.select_folder("INBOX".into(), cx);
            }
            3 => self.select_message(0, cx),
            4 => {
                assert_eq!(
                    self.inline.as_ref().unwrap().location.as_ref().unwrap().uid,
                    240
                );
                self.search_text
                    .update(cx, |input, cx| input.set_value("合成主题 240", window, cx));
                self.run_search(cx);
            }
            5 => {
                assert_eq!(self.search_results.as_ref().unwrap().len(), 1);
                self.open_hit(0, cx);
            }
            6 => {
                assert_eq!(
                    self.inline.as_ref().unwrap().location.as_ref().unwrap().uid,
                    240
                );
                self.clear_search(cx);
            }
            7 => {
                assert!(self.search_results.is_none());
                assert_eq!(
                    self.inline.as_ref().unwrap().location.as_ref().unwrap().uid,
                    240
                );
                let mut preferences = crate::settings::prefs(cx);
                preferences.dark = false;
                crate::settings::set(cx, preferences);
                window.resize(size(px(1440.), px(900.)));
                eprintln!("QINGMAIL_UI_CAPTURE: light");
            }
            8 => {
                let mut preferences = crate::settings::prefs(cx);
                preferences.dark = true;
                crate::settings::set(cx, preferences);
                eprintln!("QINGMAIL_UI_CAPTURE: dark");
            }
            9 => {
                self.narrow_detail = true;
                window.resize(size(px(900.), px(760.)));
                eprintln!("QINGMAIL_UI_CAPTURE: narrow");
            }
            10 => {
                self.load_more(cx);
            }
            _ => {
                assert_eq!(
                    self.inline.as_ref().unwrap().location.as_ref().unwrap().uid,
                    240
                );
                assert_eq!(self.messages().len(), 240);
                assert_eq!(self.messages().last().unwrap().location.uid, 1);
                eprintln!(
                    "QINGMAIL_MAILBOX_UI_PROBE: PASS · auto inbox / direct reading / latest selection wins / folder switch discards stale result / search / restore / append loading / light dark narrow"
                );
                if std::env::var_os("QINGMAIL_IPC_TEST_MAILBOX").is_none() {
                    cx.quit();
                }
            }
        }
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
                if let Some((id, _)) = &self.account {
                    self.closed_accounts.insert(id.0.clone());
                }
                assert_eq!(self.selected_folder.as_deref(), Some("INBOX"));
            }
            2 => {
                assert_eq!(self.selected, Some(1));
                self.load_more(cx);
            }
            3 => {
                assert!(!self.busy);
                assert_eq!(self.messages().len(), 240);
                assert_eq!(self.messages()[0].location.uid, 240);
                assert_eq!(self.messages().last().unwrap().location.uid, 1);
                self.quiet_reload = true;
                self.load_cache(cx);
            }
            _ => {
                assert_eq!(self.messages().len(), 240);
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
// 复用控件的数量/目标响应，仅表示队列已落盘；调用方明确展示“待同步”。
fn enqueue_action(store: &StorageClient, operation: Op) -> Result<Reply, String> {
    use mail_protocol::{Command, MailOperation};
    let (account, name, epoch, uids, op, destination) = match operation {
        Op::Flags {
            account,
            name,
            epoch,
            uids,
            seen,
            flagged,
        } => (
            account,
            name,
            epoch,
            uids,
            MailOperation::Flags { seen, flagged },
            None,
        ),
        Op::Move {
            account,
            name,
            epoch,
            uids,
            destination,
        } => {
            let catalog = match store.control(Command::Catalog {
                account: account.clone(),
            })? {
                Reply::Catalog { folders, .. } => folders,
                _ => return Err("账号目录响应无效".into()),
            };
            let op = if folders::trash(&catalog, None).as_deref() == Some(destination.as_str()) {
                MailOperation::Delete
            } else {
                let target = receive(store.folders(account.clone()))?
                    .into_iter()
                    .find(|f| f.remote_name == destination)
                    .ok_or("目标文件夹尚未同步，请先同步账号")?;
                MailOperation::Move {
                    destination: target.id,
                }
            };
            (account, name, epoch, uids, op, Some(destination))
        }
        _ => return Err("此入口仅处理邮件标记与移动".into()),
    };
    let folder = receive(store.folders(account.clone()))?
        .into_iter()
        .find(|f| f.remote_name == name && f.uidvalidity == epoch)
        .ok_or("邮件定位已变化，请刷新")?;
    let locations = uids
        .into_iter()
        .map(|uid| mail_core::MailUid {
            account: account.clone(),
            folder_id: folder.id.0.clone(),
            uidvalidity: epoch,
            uid,
        })
        .collect();
    let Reply::Operations(records) = store.control(Command::EnqueueOperation {
        locations,
        operation: op,
    })?
    else {
        return Err("操作落盘响应无效".into());
    };
    let count = records.len() as u64;
    Ok(match destination {
        Some(destination) => Reply::Moved {
            count,
            copied_only: false,
            destination,
        },
        None => Reply::Count(count),
    })
}

impl Drop for Mailbox {
    fn drop(&mut self) {
        self.read_cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn location(uid: u32) -> mail_core::MailUid {
        mail_core::MailUid {
            account: AccountId("test-account".into()),
            folder_id: "inbox".into(),
            uidvalidity: 42,
            uid,
        }
    }

    #[test]
    fn inline_reader_matches_selected_message_in_any_folder_and_keeps_local_sent_record() {
        let inbox = location(7);
        let other = location(8);
        assert!(inline_matches_selection(Some(&inbox), Some(&inbox)));
        assert!(!inline_matches_selection(Some(&inbox), Some(&other)));
        assert!(!inline_matches_selection(Some(&inbox), None));
        assert!(inline_matches_selection(None, None));
        assert!(!inline_matches_selection(None, Some(&inbox)));
    }

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

    #[test]
    fn drafts_folder_names_route_to_the_account_draft_page() {
        for folder in [
            "Drafts",
            "draft",
            "[Gmail]/Drafts",
            "草稿/Drafts",
            "草稿",
            "草稿箱",
        ] {
            assert!(is_drafts_folder(folder), "{folder}");
        }
        for folder in ["INBOX", "Draft Archive", "Projects/Drafts-2026"] {
            assert!(!is_drafts_folder(folder), "{folder}");
        }
    }
}
