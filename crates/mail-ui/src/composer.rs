//! 原生写信与明确发送确认；后台持久任务禁止自动重发。
use gpui_kit::{
    App, Context, Entity, EntityInputHandler, EventEmitter, FontStyle, FontWeight, HighlightStyle,
    Hsla, KeyDownEvent, Render, SharedString, Subscription, UnderlineStyle, Window,
    base::{
        TextSelectionLayer,
        input::{
            Editor, EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputEditsEvent,
            InputHighlighter, InputState, Redo, Undo,
        },
    },
    div,
    prelude::*,
    px,
};
use gpui_kit_material::{prelude::*, theme};
use mail_content::draft::{Attributes, Block, Draft};
use mail_daemon::storage::{StorageClient, receive};
use mail_storage::StoredDraft;
use std::{
    cell::RefCell,
    io::Write,
    ops::Range,
    os::unix::fs::OpenOptionsExt,
    rc::Rc,
    time::{Duration, Instant},
};

#[path = "composer_drafts.rs"]
pub(super) mod drafts_view;
#[path = "composer_fields.rs"]
mod fields;
#[path = "composer_probe.rs"]
mod probe;
#[path = "composer_send.rs"]
mod send;
#[path = "composer_view.rs"]
mod view;

const SAMPLE: &str = "你好，QingMail！\n选中这段中文，测试粗体、斜体和链接。🙂\n第一项\n这是一段引用。\n凌晨 · QingMail";

/// IME 提交事件可能把整个文档标为替换；缩窄到实际变动以保留外围格式。
fn difference(old: &str, new: &str) -> (Range<usize>, Range<usize>) {
    let prefix: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    let suffix: usize = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    (prefix..old.len() - suffix, prefix..new.len() - suffix)
}

#[derive(Clone)]
struct Snapshot {
    draft: Draft,
    selection: Range<usize>,
}
struct Session {
    draft: Draft,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    composition: Option<Snapshot>,
    typing: Option<(usize, Attributes)>,
    revision: u64,
}
impl Session {
    fn new(draft: Draft) -> Self {
        Self {
            draft,
            undo: Vec::new(),
            redo: Vec::new(),
            composition: None,
            typing: None,
            revision: 0,
        }
    }
    fn checkpoint(&mut self, snapshot: Snapshot) {
        if self.undo.len() == 32 {
            self.undo.remove(0);
        }
        self.undo.push(snapshot);
        self.redo.clear();
    }
    fn finish_composition(&mut self) {
        if let Some(before) = self.composition.take()
            && before.draft != self.draft
        {
            self.checkpoint(before);
        }
    }
    fn change(&mut self, edits: &InputEditsEvent, composing: bool) -> Result<(), String> {
        let Some((first, _)) = edits.edits().first() else {
            return Ok(());
        };
        if edits
            .edits()
            .last()
            .is_some_and(|(_, text)| text == self.draft.text())
        {
            return Ok(());
        }
        let before = Snapshot {
            draft: self.draft.clone(),
            selection: first.start_byte..first.old_end_byte,
        };
        let mut next = self.draft.clone();
        for (edit, text) in edits.edits() {
            let text = text.to_string();
            let (old_range, new_range) = if edit.start_byte == 0
                && edit.old_end_byte == next.text().len()
                && edit.new_end_byte == text.len()
            {
                difference(next.text(), &text)
            } else {
                (
                    edit.start_byte..edit.old_end_byte,
                    edit.start_byte..edit.new_end_byte,
                )
            };
            let inserted = text.get(new_range.clone()).ok_or("编辑范围无效")?;
            let typing = self
                .typing
                .as_ref()
                .filter(|(offset, _)| *offset == edit.start_byte)
                .map(|(_, a)| a.clone());
            next.replace(old_range, inserted, typing.as_ref())?;
            if next.text() != text {
                return Err("编辑快照与受控文档不一致".into());
            }
            self.typing = typing.map(|a| (edit.new_end_byte, a));
        }
        if composing {
            if self.composition.is_none() {
                self.composition = Some(before);
            }
        } else if self.composition.is_none() {
            self.checkpoint(before);
        }
        self.draft = next;
        self.revision += 1;
        if !composing {
            self.finish_composition();
        }
        Ok(())
    }
}

struct RichStyles {
    session: Rc<RefCell<Session>>,
    projection: Option<Draft>,
    quote_background: Hsla,
}
impl InputHighlighter for RichStyles {
    fn language(&self) -> SharedString {
        "qingmail-rich".into()
    }
    fn update(
        &mut self,
        _: Option<InputEdit>,
        text: &ropey::Rope,
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        let text = text.to_string();
        let session = self.session.borrow();
        self.projection = if text == session.draft.text() {
            None
        } else {
            let mut projected = session.draft.clone();
            let (old, new) = difference(projected.text(), &text);
            projected
                .replace(old, &text[new], None)
                .ok()
                .map(|()| projected)
        };
    }
    fn fold_ranges(&self, _: &ropey::Rope) -> Vec<FoldRange> {
        vec![]
    }
    fn styles(
        &self,
        range: &Range<usize>,
        _: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let session = self.session.borrow();
        let draft = self
            .projection
            .as_ref()
            .filter(|d| d.text() != session.draft.text())
            .unwrap_or(&session.draft);
        let mut runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        for ((offset, ch), a) in draft.text().char_indices().zip(draft.attributes()) {
            let r = offset.max(range.start)..(offset + ch.len_utf8()).min(range.end);
            if r.is_empty() {
                continue;
            }
            let style = HighlightStyle {
                font_weight: a.bold.then_some(FontWeight::BOLD),
                font_style: a.italic.then_some(FontStyle::Italic),
                // 引用在编辑器中直接可见；签名淡化，列表/引用续项见当前区块提示。
                background_color: (a.block == Block::Quote).then_some(self.quote_background),
                fade_out: (a.block == Block::Signature).then_some(0.65),
                underline: a.link.as_ref().map(|_| UnderlineStyle {
                    thickness: px(1.),
                    color: None,
                    wavy: false,
                }),
                ..Default::default()
            };
            if let Some((last, previous)) = runs
                .last_mut()
                .filter(|(r, s)| r.end == offset && *s == style)
            {
                let _ = previous;
                last.end = r.end;
            } else {
                runs.push((r, style));
            }
        }
        if runs.is_empty() {
            runs.push((range.clone(), HighlightStyle::default()));
        }
        runs
    }
}

pub(super) fn open(cx: &mut App) {
    open_scoped(crate::lifecycle::default_sender(cx), None, false, cx);
}
pub(super) fn open_account(account: mail_core::AccountId, address: String, cx: &mut App) {
    open_scoped(Some((account, address)), None, false, cx);
}
/// 回复/转发先保存当前编辑，再在主窗口中创建新草稿。
pub(super) fn open_document(
    account: mail_core::AccountId,
    address: String,
    document: mail_storage::DraftDocument,
    cx: &mut App,
) {
    open_scoped(Some((account, address)), Some(document), false, cx);
}
pub(super) fn open_saved(account: mail_core::AccountId, address: String, cx: &mut App) {
    open_scoped(Some((account, address)), None, true, cx);
}
fn open_scoped(
    account: Option<(mail_core::AccountId, String)>,
    document: Option<mail_storage::DraftDocument>,
    force: bool,
    cx: &mut App,
) {
    crate::mailbox::open(cx);
    crate::lifecycle::compose_in_mailbox(account, document, force, cx);
}

pub(super) enum ComposerEvent {
    Closed,
    CloseCancelled,
    Sent(Box<mail_storage::SendTask>),
}
impl EventEmitter<ComposerEvent> for Composer {}

pub(super) struct Composer {
    checking_send: bool,
    sending: bool,
    send_review: Option<(mail_daemon::sending::SendReview, u64)>,
    send_tasks: Vec<mail_storage::SendTask>,
    send_tasks_generation: u64,
    account_scope: Option<(mail_core::AccountId, String)>,
    header_inputs: Vec<Entity<InputState>>,
    header_values: [String; 4],
    header_subscriptions: Vec<Subscription>,
    attachment_ids: Vec<mail_core::AttachmentId>,
    attachment_infos: Vec<mail_storage::AttachmentInfo>,
    importing: bool,
    closing: bool,
    closed: bool,
    close_requested: bool,
    show_link: bool,
    pub(super) pane_width: f32,
    more_open: bool,
    drafts_open: bool,
    sender_open: bool,
    pending_draft: Option<Option<mail_core::DraftId>>,
    workspace_folder: Option<String>,
    draft_search: Entity<InputState>,
    _draft_search_subscription: Subscription,
    sync_items: Vec<mail_protocol::DraftSyncInfo>,
    list_refreshing: bool,
    draft_list_generation: u64,
    deselect_requested: bool,
    last_list_refresh: Instant,
    server_details: Option<Entity<crate::server_drafts::ServerDrafts>>,
    show_server_details: bool,
    pending_details: bool,
    confirm_delete: bool,
    deleting: bool,
    list_visible: bool,
    _selection_subscription: Subscription,
    close_error: Option<String>,
    font_family: SharedString,
    input: Entity<EditorState>,
    link: Entity<InputState>,
    session: Rc<RefCell<Session>>,
    store: Option<StorageClient>,
    stored: Option<StoredDraft>,
    initial_document: Option<mail_storage::DraftDocument>,
    force_new: bool,
    drafts: Vec<StoredDraft>,
    saved: Option<u64>,
    saving: bool,
    load_failed: bool,
    auto_save_blocked: bool,
    loading: bool,
    last_edit: Instant,
    status: String,
    _subscription: Subscription,
    probe_step: usize,
    show_cc_bcc: bool,
    field_errors: [Option<String>; 4],
}

impl Composer {
    pub(super) fn workspace_folder(&self) -> Option<&str> {
        self.workspace_folder.as_deref()
    }
    pub(super) fn account_id(&self) -> Option<&mail_core::AccountId> {
        self.account_scope.as_ref().map(|(id, _)| id)
    }
    pub(super) fn closed(&self) -> bool {
        self.closed
    }
    pub(super) fn leaving(&self) -> bool {
        self.closing || self.close_requested || self.closed
    }
    pub(super) fn request_close(&mut self, cx: &mut Context<Self>) {
        self.close_requested = true;
        cx.notify();
    }
    fn finish_close(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        cx.emit(ComposerEvent::Closed);
        cx.notify();
    }
    pub(super) fn new(
        account_scope: Option<(mail_core::AccountId, String)>,
        initial_document: Option<mail_storage::DraftDocument>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(account_scope, initial_document, None, window, cx)
    }
    pub(super) fn new_workspace(
        account: (mail_core::AccountId, String),
        folder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(Some(account), None, Some(folder), window, cx)
    }
    fn new_inner(
        account_scope: Option<(mail_core::AccountId, String)>,
        initial_document: Option<mail_storage::DraftDocument>,
        workspace_folder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let names = window.text_system().all_font_names();
        let font_family: SharedString = ["Noto Sans", "Liberation Sans", "DejaVu Sans"]
            .into_iter()
            .find(|name| names.iter().any(|available| available == name))
            .unwrap_or(".SystemUIFont")
            .into();
        let initial = initial_document
            .as_ref()
            .map(|document| document.body.text().to_owned())
            .unwrap_or_else(|| if account_scope.is_some() { "" } else { SAMPLE }.to_owned());
        let draft = Draft::plain(&initial).expect("bounded sample");
        let session = Rc::new(RefCell::new(Session::new(draft.clone())));
        let input = cx.new(|cx| {
            let mut input = EditorState::new(window, cx)
                .language("qingmail-rich")
                .line_number(false)
                .folding(false)
                .auto_close(false)
                .smart_indent(false)
                .searchable(false)
                .soft_wrap(true)
                .context_menu(false)
                .default_value(initial);
            let session = session.clone();
            input.set_highlighter_factory(
                Rc::new(move |_| {
                    Some(Box::new(RichStyles {
                        session: session.clone(),
                        projection: None,
                        quote_background: Hsla::from(theme::color(ColorRole::SURFACE_CONTAINER)),
                    }))
                }),
                cx,
            );
            input
        });
        let link =
            cx.new(|cx| InputState::new(window, cx).default_value("https://example.org/qingmail"));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEditsEvent, window, cx| {
                let composing = input.update(cx, |input, cx| {
                    input.marked_text_range(window, cx).is_some()
                });
                let result = this.session.borrow_mut().change(event, composing);
                if let Err(e) = result {
                    this.status = e;
                    this.sync_input(window, cx);
                } else {
                    this.last_edit = Instant::now();
                }
                cx.notify();
            },
        );
        let header_inputs = (0..4)
            .map(|_| cx.new(|cx| InputState::new(window, cx)))
            .collect();
        let draft_search = cx.new(|cx| InputState::new(window, cx));
        let mut this = Self {
            checking_send: false,
            sending: false,
            send_review: None,
            send_tasks: vec![],
            send_tasks_generation: 0,
            account_scope,
            header_inputs,
            header_values: Default::default(),
            header_subscriptions: vec![],
            attachment_ids: vec![],
            attachment_infos: vec![],
            importing: false,
            closing: false,
            closed: false,
            close_requested: false,
            show_link: false,
            pane_width: 600.,
            more_open: false,
            drafts_open: false,
            sender_open: false,
            pending_draft: None,
            workspace_folder,
            draft_search: draft_search.clone(),
            _draft_search_subscription: cx.observe(&draft_search, |_, _, cx| cx.notify()),
            sync_items: vec![],
            list_refreshing: false,
            draft_list_generation: 0,
            deselect_requested: false,
            last_list_refresh: Instant::now() - Duration::from_secs(3),
            server_details: None,
            show_server_details: false,
            pending_details: false,
            confirm_delete: false,
            deleting: false,
            list_visible: true,
            _selection_subscription: cx.observe(&input, |_, _, cx| cx.notify()),
            close_error: None,
            font_family,
            input: input.clone(),
            link,
            session,
            store: None,
            stored: None,
            initial_document,
            force_new: false,
            drafts: vec![],
            saved: None,
            saving: false,
            load_failed: false,
            auto_save_blocked: false,
            loading: true,
            last_edit: Instant::now(),
            status: "正在恢复草稿…".into(),
            _subscription: subscription,
            probe_step: 0,
            show_cc_bcc: false,
            field_errors: [None, None, None, None],
        };
        this.subscribe_headers(cx);
        this.input
            .update(cx, |input, cx| input.set_readonly(true, cx));
        this.load_store(window, cx);
        cx.spawn_in(window, async move |entity, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                if entity
                    .update_in(cx, |this, window, cx| this.tick(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        this
    }

    fn load_store(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = crate::lifecycle::storage(cx);
        let account_scope = self.account_scope.clone();
        let initial_document = self.initial_document.take();
        let force_new = std::mem::take(&mut self.force_new);
        if self.workspace_folder.is_some() && !force_new && initial_document.is_none() {
            let account = account_scope.as_ref().expect("workspace account").0.clone();
            let task = cx.background_executor().spawn(async move {
                let drafts = receive(store.account_drafts(account.clone()))?;
                let draft = if drafts.is_empty() {
                    None
                } else {
                    Some(receive(store.open_account_editor(account))?)
                };
                Ok::<_, String>((store, draft, drafts))
            });
            cx.spawn_in(window, async move |entity, cx| {
                let result = task.await;
                let _ = entity.update_in(cx, |this, window, cx| {
                    this.loading = false;
                    match result {
                        Ok((store, draft, drafts)) => {
                            this.store = Some(store);
                            this.drafts = drafts;
                            if let Some(draft) = draft {
                                this.restore_draft(draft, window, cx);
                            } else {
                                this.clear_selection(window, cx);
                                this.status = "暂无草稿，点击左侧写邮件开始".into();
                            }
                            this.refresh_workspace(cx);
                        }
                        Err(e) => {
                            this.load_failed = true;
                            this.status = format!("草稿加载失败，原内容保留：{e}");
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        let request = if let Some((account, _)) = &account_scope {
            if force_new {
                store.new_account_draft(account.clone())
            } else if let Some(document) = initial_document {
                store.new_account_draft_document(account.clone(), document)
            } else {
                store.open_account_editor(account.clone())
            }
        } else if force_new {
            store.new_draft(Draft::plain("").expect("empty draft"))
        } else {
            store.open_editor(self.session.borrow().draft.clone())
        };
        let task = cx
            .background_executor()
            .spawn(async move { receive(request).map(|draft| (store, draft)) });
        cx.spawn_in(window, async move |entity, cx| {
            let result = task.await;
            let _ = entity.update_in(cx, |this, window, cx| {
                this.loading = false;
                window.activate_window();
                match result {
                    Ok((store, draft)) => {
                        this.input
                            .update(cx, |input, cx| input.set_readonly(false, cx));
                        this.store = Some(store);
                        *this.session.borrow_mut() = Session::new(draft.document.body.clone());
                        this.stored = Some(draft);
                        this.restore_fields(window, cx);
                        this.saved = Some(0);
                        this.load_failed = false;
                        this.sync_input(window, cx);
                        this.status = "已恢复草稿 · 检查并确认后才发送".into();
                        this.refresh_drafts(cx);
                        this.refresh_send_tasks(cx);
                    }
                    Err(e) => {
                        this.load_failed = true;
                        this.status = format!("草稿加载失败，原文件未改动：{e}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_drafts(&mut self, cx: &mut Context<Self>) {
        let generation = self.draft_list_generation;
        let store = crate::lifecycle::storage(cx);
        let request = if let Some((account, _)) = &self.account_scope {
            store.account_drafts(account.clone())
        } else {
            store.list()
        };
        let task = cx
            .background_executor()
            .spawn(async move { receive(request) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                match result {
                    Ok(drafts) if this.draft_list_generation == generation => this.drafts = drafts,
                    Ok(_) => (),
                    Err(e) => this.status = e,
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn switch_draft(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let id = index.and_then(|i| self.drafts.get(i).map(|d| d.id.clone()));
        if index.is_some() && id.is_none() {
            return;
        }
        self.switch_draft_id(id, window, cx);
    }
    pub(super) fn new_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_draft_id(None, window, cx);
    }
    fn switch_draft_id(
        &mut self,
        id: Option<mail_core::DraftId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.deleting
            || self.loading
            || self.saving
            || self.closing
            || self.importing
            || self.send_locked()
            || self.composing(window, cx)
        {
            return;
        }
        if self.saved != Some(self.session.borrow().revision) {
            self.pending_draft = Some(id);
            self.status = "正在保存当前修改，完成后切换草稿".into();
            self.save(cx);
            return;
        }
        let client = crate::lifecycle::storage(cx);
        let request = if let Some(id) = id {
            if let Some((account, _)) = &self.account_scope {
                client.select_account_draft(account.clone(), id)
            } else {
                client.select(id)
            }
        } else {
            if let Some((account, _)) = &self.account_scope {
                client.new_account_draft(account.clone())
            } else {
                client.new_draft(Draft::plain("").expect("empty draft"))
            }
        };
        self.confirm_delete = false;
        self.deselect_requested = false;
        self.draft_list_generation += 1;
        self.loading = true;
        self.input
            .update(cx, |input, cx| input.set_readonly(true, cx));
        let task = cx
            .background_executor()
            .spawn(async move { receive(request) });
        cx.spawn_in(window, async move |entity, cx| {
            let result = task.await;
            let _ = entity.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Ok(draft) => {
                        this.restore_draft(draft, window, cx);
                        this.show_server_details = false;
                        this.list_visible = false;
                        this.refresh_drafts(cx);
                    }
                    Err(e) => this.status = format!("切换失败，当前内容保留：{e}"),
                }
                this.input
                    .update(cx, |input, cx| input.set_readonly(false, cx));
                cx.notify();
            });
        })
        .detach();
    }

    fn composing(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.input.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        }) || self.header_inputs.iter().any(|input| {
            input.update(cx, |input, cx| {
                input.marked_text_range(window, cx).is_some()
            })
        })
    }
    fn sync_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.session.borrow().draft.text().to_owned();
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.refresh(cx);
        });
    }
    pub(super) fn lifecycle_probe(
        &mut self,
        step: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.loading {
            return false;
        }
        match step {
            0 => {
                if std::env::var_os("QINGMAIL_IPC_TEST_EDITOR").is_some() {
                    let root = std::env::var_os("QINGMAIL_IPC_ROOT").unwrap();
                    if !std::path::Path::new(&root).join("edit-trigger").exists() {
                        return false;
                    }
                }
                if std::env::args().any(|arg| arg == "--lifecycle-failure-probe") {
                    let root = std::env::var_os("QINGMAIL_P0_DRAFT_ROOT").unwrap();
                    if !std::path::Path::new(&root).join("trigger").exists() {
                        return false;
                    }
                }
                let length = self.input.read(cx).value().encode_utf16().count();
                self.input.update(cx, |input, cx| {
                    input.replace_text_in_range(
                        Some(0..length),
                        "生命周期验证🙂\n关闭前最新草稿",
                        window,
                        cx,
                    )
                });
            }
            1 => {
                let utf16 = self.input.read(cx).value().encode_utf16().count();
                self.input.update(cx, |input, cx| {
                    input.replace_and_mark_text_in_range(
                        Some(utf16..utf16),
                        "ni",
                        Some(2..2),
                        window,
                        cx,
                    )
                });
                assert!(
                    self.prepare_close(window, cx).is_err(),
                    "active preedit must veto close"
                );
                assert!(!self.closing);
                self.input.update(cx, |input, cx| {
                    input.replace_text_in_range(None, "", window, cx)
                });
                eprintln!("P0_PROBE: marked preedit vetoed close; cancel restored document");
                let length = self.session.borrow().draft.text().len();
                self.input
                    .update(cx, |input, cx| input.set_selected_range(0..length, cx));
                self.format("bold", window, cx);
            }
            2 => {
                let _ = self.prepare_close(window, cx);
            }
            _ => return false,
        }
        true
    }
    pub(super) fn prepare_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        if let Some(error) = self.close_error.take() {
            cx.emit(ComposerEvent::CloseCancelled);
            return Err(error);
        }
        if self.deleting {
            self.close_requested = true;
            return Ok(());
        }
        if self.importing {
            self.status = "请先完成或取消附件选择，等待附件导入结束后再关闭".into();
            cx.notify();
            cx.emit(ComposerEvent::CloseCancelled);
            return Err(self.status.clone());
        }
        if self.loading {
            self.close_requested = true;
            self.status = "正在恢复草稿，请稍后关闭".into();
            return Ok(());
        }
        if self.composing(window, cx) {
            self.status = "请先完成或取消中文候选词，再关闭窗口".into();
            cx.notify();
            cx.emit(ComposerEvent::CloseCancelled);
            return Err(self.status.clone());
        }
        if self.store.is_none() && self.session.borrow().revision == 0 {
            // 初始加载失败时输入保持只读；未产生用户改动，可以关闭。
            self.finish_close(cx);
            return Ok(());
        }
        if self.store.is_none() && self.saved != Some(self.session.borrow().revision) {
            self.status = "关闭已取消：草稿存储不可用，请复制保留内容并排除问题".into();
            cx.notify();
            cx.emit(ComposerEvent::CloseCancelled);
            return Err(self.status.clone());
        }
        // 关闭不会撤回已确认发送；预览尚未确认时只丢弃预览。
        self.send_review = None;
        self.pending_draft = None;
        self.closing = true;
        self.input
            .update(cx, |input, cx| input.set_readonly(true, cx));
        self.save(cx);
        cx.notify();
        Ok(())
    }
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        if self.workspace_folder.is_some()
            && self.last_list_refresh.elapsed() >= Duration::from_secs(3)
        {
            self.refresh_workspace(cx);
        }
        if self.deleting {
            return;
        }
        if self.close_requested {
            self.close_requested = false;
            let _ = self.prepare_close(window, cx);
            if self.closed {
                return;
            }
        }
        if self.loading {
            return;
        }
        if !self.closing && self.store.is_some() {
            let locked = self.send_locked() || self.stored.is_none();
            self.input
                .update(cx, |input, cx| input.set_readonly(locked, cx));
        }
        if probe::enabled() {
            self.probe(window, cx);
        }
        if std::env::args().any(|a| a == "--send-review-probe") {
            self.send_probe(window, cx);
        }
        if self.composing(window, cx) {
            return;
        }
        if self.closing && !self.saving && self.saved == Some(self.session.borrow().revision) {
            if let (Some(store), Some(draft)) = (self.store.clone(), self.stored.as_ref()) {
                let account = draft.account.clone();
                cx.background_executor()
                    .spawn(async move {
                        let _ = store.control(mail_protocol::Command::FlushDraftSync { account });
                    })
                    .detach();
            }
            self.finish_close(cx);
            return;
        }
        if self.pending_details
            && !self.saving
            && !self.closing
            && self.saved == Some(self.session.borrow().revision)
        {
            self.pending_details = false;
            self.show_details(window, cx);
            return;
        }
        if !self.closing
            && !self.saving
            && self.saved == Some(self.session.borrow().revision)
            && let Some(index) = self.pending_draft.take()
        {
            self.switch_draft_id(index, window, cx);
            return;
        }
        if self.deselect_requested
            && !self.saving
            && self.saved == Some(self.session.borrow().revision)
        {
            self.deselect_requested = false;
            self.clear_selection(window, cx);
            cx.notify();
            return;
        }
        self.session.borrow_mut().finish_composition();
        if !self.saving
            && self.stored.is_some()
            && !self.auto_save_blocked
            && self.last_edit.elapsed() >= Duration::from_millis(750)
        {
            self.save(cx);
        }
    }
    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let session = self.session.borrow();
        if self.saving || self.saved == Some(session.revision) {
            return;
        }
        let revision = session.revision;
        let draft = session.draft.clone();
        let Some(previous) = self.stored.clone() else {
            return;
        };
        drop(session);
        self.saving = true;
        self.auto_save_blocked = false;
        self.close_error = None;
        let document = mail_storage::DraftDocument {
            headers: fields::headers(&self.header_values),
            body: draft,
            attachments: self.attachment_ids.clone(),
        };
        let request = store.save_document(previous, document);
        let task = cx.background_executor().spawn(async move {
            let started = Instant::now();
            if probe::enabled() {
                std::thread::sleep(Duration::from_millis(900));
            }
            let result = receive(request);
            (result, started.elapsed())
        });
        cx.spawn(async move |entity, cx| {
            let (result, elapsed) = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(stored) => {
                        this.draft_list_generation += 1;
                        this.upsert_draft(stored.clone());
                        this.stored = Some(stored);
                        this.saved = Some(revision);
                        this.close_error = None;
                        this.status = format!(
                            "已保存版本 {revision}（后台 {:.1} ms）",
                            elapsed.as_secs_f64() * 1000.
                        );
                    }
                    Err(e) => {
                        this.pending_draft = None;
                        this.pending_details = false;
                        this.deselect_requested = false;
                        this.auto_save_blocked = true;
                        this.closing = false;
                        let locked = this.send_locked();
                        this.input
                            .update(cx, |input, cx| input.set_readonly(locked, cx));
                        for input in &this.header_inputs {
                            input.update(cx, |input, cx| input.set_readonly(locked, cx));
                        }
                        this.status = format!("保存失败，关闭已取消，保留上次确认版本：{e}");
                        this.close_error = Some(this.status.clone());
                        cx.emit(ComposerEvent::CloseCancelled);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn format(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading
            || self.closing
            || self.send_locked()
            || self.store.is_none()
            || self.composing(window, cx)
        {
            self.status = "请先完成中文候选词输入".into();
            cx.notify();
            return;
        }
        let range = self.input.read(cx).selected_range();
        let link = self.link.read(cx).value().to_string();
        let mut session = self.session.borrow_mut();
        session.finish_composition();
        let before = Snapshot {
            draft: session.draft.clone(),
            selection: range.clone(),
        };
        let scalar = session.draft.scalar_range(range.clone()).unwrap_or(0..0);
        let mut attr = session.draft.attributes()[scalar.start].clone();
        let result = match command {
            "bold" | "italic" => {
                if command == "bold" {
                    attr.bold = !attr.bold;
                } else {
                    attr.italic = !attr.italic;
                }
                if range.is_empty() {
                    session.typing = Some((range.start, attr));
                    Ok(())
                } else {
                    session.draft.inline(range.clone(), |a| {
                        if command == "bold" {
                            a.bold = attr.bold;
                        } else {
                            a.italic = attr.italic;
                        }
                    })
                }
            }
            "link" => session.draft.set_link(range.clone(), Some(&link)),
            "unlink" => session.draft.set_link(range.clone(), None),
            "bullet" => session.draft.set_block(range.clone(), Block::Bullet),
            "quote" => session.draft.set_block(range.clone(), Block::Quote),
            "signature" => session.draft.set_block(range.clone(), Block::Signature),
            _ => session.draft.set_block(range.clone(), Block::Paragraph),
        };
        match result {
            Ok(()) => {
                if before.draft != session.draft {
                    session.checkpoint(before);
                    session.revision += 1;
                }
                self.status = "格式已应用 · 段落结构见右侧预览".into();
            }
            Err(e) => self.status = e,
        }
        drop(session);
        self.last_edit = Instant::now();
        self.input.update(cx, |input, cx| {
            input.refresh(cx);
            input.focus(window, cx);
        });

        cx.notify();
    }
    fn history(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading
            || self.closing
            || self.send_locked()
            || self.store.is_none()
            || self.composing(window, cx)
        {
            return;
        }
        let current = self.input.read(cx).selected_range();
        let mut session = self.session.borrow_mut();
        session.finish_composition();
        let restored = if redo {
            session.redo.pop()
        } else {
            session.undo.pop()
        };
        if let Some(restored) = restored {
            let previous = Snapshot {
                draft: session.draft.clone(),
                selection: current,
            };
            if redo {
                session.undo.push(previous);
            } else {
                session.redo.push(previous);
            }
            session.draft = restored.draft;
            session.typing = None;
            session.revision += 1;
            drop(session);
            self.sync_input(window, cx);
            self.input.update(cx, |input, cx| {
                input.set_selected_range(restored.selection, cx);
                input.focus(window, cx);
            });
            self.last_edit = Instant::now();
            cx.notify();
        }
    }
    /// 列表/引用中回车续项；空条目回车结束区块，交回编辑器正常换行。
    fn continue_block(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.loading || self.closing || self.composing(window, cx) {
            return false;
        }
        let range = self.input.read(cx).selected_range();
        if range.start != range.end {
            return false;
        }
        let byte = range.start;
        let (block, line_start, line_empty) = {
            let session = self.session.borrow();
            let text = session.draft.text();
            if byte > text.len() || !text.is_char_boundary(byte) {
                return false;
            }
            let line_start = text[..byte].rfind('\n').map_or(0, |index| index + 1);
            let line_end = text[byte..]
                .find('\n')
                .map_or(text.len(), |index| byte + index);
            let scalar = text[..byte].chars().count();
            let index = scalar
                .saturating_sub(1)
                .min(session.draft.attributes().len().saturating_sub(2));
            let block = session
                .draft
                .attributes()
                .get(index)
                .map(|attributes| attributes.block)
                .unwrap_or_default();
            (
                block,
                line_start,
                text[line_start..line_end].trim().is_empty(),
            )
        };
        if !matches!(block, Block::Bullet | Block::Quote) {
            return false;
        }
        {
            let mut session = self.session.borrow_mut();
            if line_empty {
                if session
                    .draft
                    .set_block(line_start..byte, Block::Paragraph)
                    .is_err()
                {
                    return false;
                }
            } else {
                if session.draft.replace(byte..byte, "\n", None).is_err() {
                    return false;
                }
                if session.draft.set_block(byte + 1..byte + 1, block).is_err() {
                    return false;
                }
            }
            session.revision += 1;
        }
        self.sync_input(window, cx);
        let cursor = if line_empty { byte } else { byte + 1 };
        self.input
            .update(cx, |input, cx| input.set_selected_range(cursor..cursor, cx));
        self.last_edit = Instant::now();
        cx.notify();
        !line_empty
    }
    /// 显式“粘贴 HTML”：读取文本剪贴板，清理后按受控模型插入光标处。
    fn paste_html(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.closing || self.composing(window, cx) {
            return;
        }
        let Some(source) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            self.status = "剪贴板没有可粘贴的文本".into();
            cx.notify();
            return;
        };
        let pasted = match Draft::from_html(&source) {
            Ok(pasted) if !pasted.text().trim().is_empty() => pasted,
            Ok(_) => {
                self.status = "粘贴内容没有可插入的文本".into();
                cx.notify();
                return;
            }
            Err(error) => {
                self.status = format!("无法清理粘贴的 HTML：{error}");
                cx.notify();
                return;
            }
        };
        let range = self.input.read(cx).selected_range();
        let result = {
            let mut session = self.session.borrow_mut();
            session
                .draft
                .splice(range.clone(), &pasted)
                .map(|()| session.revision += 1)
        };
        if let Err(error) = result {
            self.status = error;
            cx.notify();
            return;
        }
        let mut cursor = range.start + pasted.text().len();
        self.sync_input(window, cx);
        let length = self.session.borrow().draft.text().len();
        cursor = cursor.min(length);
        while cursor > 0 && !self.session.borrow().draft.text().is_char_boundary(cursor) {
            cursor -= 1;
        }
        self.input
            .update(cx, |input, cx| input.set_selected_range(cursor..cursor, cx));
        self.last_edit = Instant::now();
        self.status = "已清理 HTML 并插入正文".into();
        cx.notify();
    }
    fn export_text(&self) -> String {
        format!(
            "收件人：{}\n抄送：{}\n密送：{}\n主题：{}\n\n{}",
            self.header_values[0],
            self.header_values[1],
            self.header_values[2],
            self.header_values[3],
            self.session.borrow().draft.plain_export()
        )
    }
    fn save_as(&mut self, cx: &mut Context<Self>) {
        self.save_bytes_as(self.export_text().into_bytes(), "qingmail-draft.txt", cx);
    }
    fn save_bytes_as(&mut self, bytes: Vec<u8>, name: &str, cx: &mut Context<Self>) {
        let directory = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let suggested = name.to_owned();
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));
        let task = cx.background_executor().spawn(receiver);
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                match result {
                    Ok(Ok(Some(path))) => {
                        let written = std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(0o600)
                            .open(&path)
                            .and_then(|mut file| file.write_all(&bytes));
                        this.status = match written {
                            Ok(()) => format!("已另存到 {}", path.display()),
                            Err(error) => format!("另存失败：{error}"),
                        };
                    }
                    Ok(Ok(None)) => this.status = "已取消另存".into(),
                    Ok(Err(error)) => this.status = format!("无法打开另存对话框：{error}"),
                    Err(_) => this.status = "另存对话框已关闭".into(),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    /// 坏草稿恢复：取出原始正文字节另存，不要求正文能反序列化。
    fn save_raw_draft(&mut self, cx: &mut Context<Self>) {
        let account = self.account_scope.as_ref().map(|(id, _)| id.clone());
        let store = crate::lifecycle::storage(cx);
        self.status = "正在读取原始草稿数据…".into();
        let task = cx
            .background_executor()
            .spawn(async move { receive(store.selected_draft_body(account)) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                match result {
                    Ok(Some(bytes)) => {
                        this.save_bytes_as(bytes, "qingmail-draft-recovery.json", cx)
                    }
                    Ok(None) => this.status = "没有可另存的草稿数据".into(),
                    Err(error) => this.status = format!("读取原始草稿失败：{error}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    /// 坏草稿的明确放弃：只删草稿行，保留对象文件，再创建新草稿。
    fn discard_bad_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let account = self.account_scope.as_ref().map(|(id, _)| id.clone());
        let store = crate::lifecycle::storage(cx);
        self.status = "正在放弃坏草稿；对象文件保留…".into();
        let task = cx
            .background_executor()
            .spawn(async move { receive(store.discard_selected_draft(account)) });
        cx.spawn_in(window, async move |entity, cx| {
            let result = task.await;
            let _ = entity.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.load_failed = false;
                        this.loading = true;
                        this.force_new = true;
                        this.status = "已放弃坏草稿；正在创建新草稿…".into();
                        this.load_store(window, cx);
                    }
                    Err(error) => this.status = format!("无法放弃草稿：{error}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        if !k.modifiers.control || k.modifiers.alt {
            return;
        }
        match k.key.as_str() {
            "z" => self.history(k.modifiers.shift, window, cx),
            "y" => self.history(true, window, cx),
            "b" => self.format("bold", window, cx),
            "i" => self.format("italic", window, cx),
            "s" => {
                if !self.composing(window, cx) {
                    self.save(cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        window.prevent_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_ime_snapshot_preserves_unedited_marks_and_utf8_boundaries() {
        for (old, new) in [
            ("中文🙂\n第二段", "你🙂\n第二段"),
            ("哈哈哈哈", "哈你好哈哈"),
            ("", "🙂"),
            ("🙂", ""),
        ] {
            let mut draft = Draft::plain(old).unwrap();
            let last = old.char_indices().last().map(|(i, _)| i).unwrap_or(0);
            draft.inline(last..old.len(), |a| a.italic = true).unwrap();
            let (range, replacement) = difference(old, new);
            draft.replace(range, &new[replacement], None).unwrap();
            assert_eq!(draft.text(), new);
            assert_eq!(Draft::from_json(&draft.to_json().unwrap()).unwrap(), draft);
            if old.ends_with('段') {
                assert!(draft.attributes()[new.chars().count() - 1].italic);
            }
        }
    }
}
