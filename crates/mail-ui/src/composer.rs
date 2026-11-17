//! 原生写信与明确发送确认；后台持久任务禁止自动重发。
use gpui_kit::{
    App, Bounds, Context, Entity, EntityInputHandler, FontStyle, FontWeight, HighlightStyle, Hsla,
    KeyDownEvent, Render, SharedString, Subscription, TitlebarOptions, UnderlineStyle, Window,
    WindowBounds, WindowOptions,
    base::{
        TextSelectionLayer, TextView, TextViewState,
        input::{
            Editor, EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputEditsEvent,
            InputHighlighter, InputState, Redo, Undo,
        },
    },
    div,
    prelude::*,
    px, size,
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

#[path = "composer_fields.rs"]
mod fields;
#[path = "composer_probe.rs"]
mod probe;
#[path = "composer_send.rs"]
mod send;

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
/// 回复/转发总是新建草稿窗口，不覆盖用户正在编辑的草稿。
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
    if !force
        && document.is_none()
        && crate::lifecycle::reuse_editor(account.as_ref().map(|(id, _)| id), cx)
    {
        return;
    }
    let bounds = Bounds::centered(None, size(px(1180.), px(820.)), cx);
    let result = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("QingMail — 写信草稿".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        |window, cx| cx.new(|cx| Composer::new(account, document, window, cx)),
    );
    match result {
        Ok((window, entity)) => crate::lifecycle::register_editor(window, entity.downgrade(), cx),
        Err(e) => eprintln!("无法打开编辑验证窗口：{e}"),
    }
}

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
    close_error: Option<String>,
    font_family: SharedString,
    input: Entity<EditorState>,
    link: Entity<InputState>,
    preview: Entity<TextViewState>,
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
    pub(super) fn account_id(&self) -> Option<&mail_core::AccountId> {
        self.account_scope.as_ref().map(|(id, _)| id)
    }
    pub(super) fn explain_scope(
        &mut self,
        requested: Option<&mail_core::AccountId>,
        cx: &mut Context<Self>,
    ) {
        if self.account_scope.as_ref().map(|(id, _)| id) != requested {
            self.status = "已有另一个草稿空间的窗口；请保存并关闭此窗口，再从目标账号打开写信。当前发件账号未改变".into();
            cx.notify();
        }
    }
    fn new(
        account_scope: Option<(mail_core::AccountId, String)>,
        initial_document: Option<mail_storage::DraftDocument>,
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
        let preview = cx.new(|cx| TextViewState::html(&draft.html(), cx));
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
                    this.refresh_preview(cx);
                }
                cx.notify();
            },
        );
        let header_inputs = (0..4)
            .map(|_| cx.new(|cx| InputState::new(window, cx)))
            .collect();
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
            close_error: None,
            font_family,
            input,
            link,
            preview,
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
            status: "正在检查本地验证草稿…".into(),
            _subscription: subscription,
            probe_step: 0,
            show_cc_bcc: false,
            field_errors: [None, None, None, None],
        };
        this.subscribe_headers(cx);
        this.input
            .update(cx, |input, cx| input.set_readonly(true, cx));
        this.load_store(window, cx);
        let entity = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = entity.update(cx, |this, cx| {
                let _ = this.prepare_close(window, cx);
            });
            false
        });
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
                    Ok(drafts) => this.drafts = drafts,
                    Err(e) => this.status = e,
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn switch_draft(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading
            || self.saving
            || self.closing
            || self.importing
            || self.send_locked()
            || self.composing(window, cx)
        {
            return;
        }
        if self.saved != Some(self.session.borrow().revision) {
            self.status = "请先保存当前修改，再切换草稿".into();
            self.save(cx);
            return;
        }
        let client = crate::lifecycle::storage(cx);
        let request = if let Some(index) = index {
            if let Some((account, _)) = &self.account_scope {
                client.select_account_draft(account.clone(), self.drafts[index].id.clone())
            } else {
                client.select(self.drafts[index].id.clone())
            }
        } else {
            if let Some((account, _)) = &self.account_scope {
                client.new_account_draft(account.clone())
            } else {
                client.new_draft(Draft::plain("").expect("empty draft"))
            }
        };
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
                        *this.session.borrow_mut() = Session::new(draft.document.body.clone());
                        this.stored = Some(draft);
                        this.restore_fields(window, cx);
                        this.saved = Some(0);
                        this.close_error = None;
                        this.sync_input(window, cx);
                        this.status = "已打开本地草稿".into();
                        this.refresh_drafts(cx);
                        this.refresh_send_tasks(cx);
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

    fn draft_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                MaterialButton::new("new-local-draft")
                    .child("新建草稿")
                    .disabled(
                        self.loading
                            || self.saving
                            || self.closing
                            || self.importing
                            || self.send_locked(),
                    )
                    .on_click(cx.listener(|this, _, w, cx| this.switch_draft(None, w, cx))),
            )
            .child(
                MaterialButton::new("retry-local-save")
                    .child("重试 / 立即保存")
                    .disabled(self.loading || self.saving)
                    .on_click(cx.listener(|this, _, w, cx| {
                        if this.store.is_none() {
                            this.loading = true;
                            this.load_store(w, cx);
                        } else {
                            this.save(cx);
                        }
                    })),
            );
        for (index, draft) in self.drafts.iter().enumerate() {
            let label = format!(
                "{}草稿 {}",
                if self.stored.as_ref().is_some_and(|d| d.id == draft.id) {
                    "● "
                } else {
                    ""
                },
                &draft.id.0[..8]
            );
            row = row.child(
                MaterialButton::new(("local-draft", index))
                    .child(label)
                    .disabled(
                        self.loading
                            || self.saving
                            || self.closing
                            || self.importing
                            || self.send_locked(),
                    )
                    .on_click(
                        cx.listener(move |this, _, w, cx| this.switch_draft(Some(index), w, cx)),
                    ),
            );
        }
        row
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
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        let html = self.session.borrow().draft.html();
        self.preview
            .update(cx, |preview, cx| preview.set_text(&html, cx));
    }
    fn sync_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.session.borrow().draft.text().to_owned();
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.refresh(cx);
        });
        self.refresh_preview(cx);
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
        if let Some(error) = self.close_error.take() {
            return Err(error);
        }
        if self.importing {
            self.status = "请先完成或取消附件选择，等待附件导入结束后再关闭".into();
            cx.notify();
            return Err(self.status.clone());
        }
        if self.loading {
            self.status = "正在恢复草稿，请稍后关闭".into();
            return Ok(());
        }
        if self.composing(window, cx) {
            self.status = "请先完成或取消中文候选词，再关闭窗口".into();
            cx.notify();
            return Err(self.status.clone());
        }
        if self.store.is_none() && self.session.borrow().revision == 0 {
            // 初始加载失败时输入保持只读；未产生用户改动，可以关闭。
            window.remove_window();
            return Ok(());
        }
        if self.store.is_none() && self.saved != Some(self.session.borrow().revision) {
            self.status = "关闭已取消：草稿存储不可用，请复制保留内容并排除问题".into();
            cx.notify();
            return Err(self.status.clone());
        }
        // 关闭不会撤回已确认发送；预览尚未确认时只丢弃预览。
        self.send_review = None;
        self.closing = true;
        self.input
            .update(cx, |input, cx| input.set_readonly(true, cx));
        self.save(cx);
        cx.notify();
        Ok(())
    }
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        if !self.closing && self.store.is_some() {
            let locked = self.send_locked();
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
            window.remove_window();
            return;
        }
        self.session.borrow_mut().finish_composition();
        if !self.saving
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
                        this.stored = Some(stored);
                        this.saved = Some(revision);
                        this.close_error = None;
                        this.status = format!(
                            "已保存版本 {revision}（后台 {:.1} ms）",
                            elapsed.as_secs_f64() * 1000.
                        );
                    }
                    Err(e) => {
                        this.auto_save_blocked = true;
                        this.closing = false;
                        this.status = format!("保存失败，关闭已取消，保留上次确认版本：{e}");
                        this.close_error = Some(this.status.clone());
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
        self.refresh_preview(cx);
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
    fn current_block(&self, cx: &Context<Self>) -> Block {
        let session = self.session.borrow();
        let text = session.draft.text();
        let mut byte = self.input.read(cx).selected_range().start.min(text.len());
        while byte > 0 && !text.is_char_boundary(byte) {
            byte -= 1;
        }
        let scalar = text[..byte].chars().count();
        let index = scalar
            .saturating_sub(1)
            .min(session.draft.attributes().len().saturating_sub(2));
        session
            .draft
            .attributes()
            .get(index)
            .map(|attributes| attributes.block)
            .unwrap_or_default()
    }
    fn block_label(&self, cx: &Context<Self>) -> &'static str {
        match self.current_block(cx) {
            Block::Paragraph => "段落",
            Block::Bullet => "列表",
            Block::Quote => "引用",
            Block::Signature => "签名",
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

impl Render for Composer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut toolbar = div().flex().flex_wrap().gap_2();
        for (id, label) in [
            ("bold", "粗体"),
            ("italic", "斜体"),
            ("paragraph", "段落"),
            ("bullet", "列表"),
            ("quote", "引用"),
            ("signature", "签名"),
            ("link", "加链接"),
            ("unlink", "去链接"),
        ] {
            toolbar = toolbar.child(
                MaterialButton::new(id)
                    .variant(ButtonVariant::Tonal)
                    .child(label)
                    .disabled(self.loading)
                    .on_click(cx.listener(move |this, _, window, cx| this.format(id, window, cx))),
            );
        }
        toolbar = toolbar
            .child(
                MaterialButton::new("undo")
                    .child("撤销")
                    .on_click(cx.listener(|this, _, w, cx| this.history(false, w, cx))),
            )
            .child(
                MaterialButton::new("redo")
                    .child("重做")
                    .on_click(cx.listener(|this, _, w, cx| this.history(true, w, cx))),
            )
            .child(
                MaterialButton::new("paste-html")
                    .variant(ButtonVariant::Tonal)
                    .child("粘贴 HTML")
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, window, cx| this.paste_html(window, cx))),
            )
            .child(
                MaterialButton::new("save-as")
                    .child("另存草稿…")
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _, cx| this.save_as(cx))),
            );
        let revision = self.session.borrow().revision;
        let state = if self.loading {
            "正在恢复"
        } else if self.saving {
            "正在保存"
        } else if self.saved == Some(revision) {
            "已保存"
        } else {
            "有未保存修改"
        };
        div().capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if event.keystroke.key == "enter"
                && !event.keystroke.modifiers.shift
                && !event.keystroke.modifiers.control
                && !event.keystroke.modifiers.alt
                && this.continue_block(window, cx)
            {
                cx.stop_propagation();
                window.prevent_default();
                return;
            }
            if event.keystroke.modifiers.control && !event.keystroke.modifiers.alt && event.keystroke.key == "enter" {
                this.check_send(window,cx);
                cx.stop_propagation();
                window.prevent_default();
            }
            if event.keystroke.modifiers.control && !event.keystroke.modifiers.alt && event.keystroke.key == "s" {
                if !this.composing(window, cx) { this.save(cx); }
                cx.stop_propagation();
                window.prevent_default();
            }
        })).size_full().child(TextSelectionLayer).flex().flex_col().gap_3().p_4()
            .bg(theme::color(ColorRole::SURFACE)).text_color(theme::color(ColorRole::ON_SURFACE))
            .child(div().text_size(px(22.)).child("QingMail · 写信草稿"))
            .child(div().text_size(px(13.)).child("选中后设置格式；Ctrl+B / I、Ctrl+Z / Shift+Z、Ctrl+S。收件人填写独立邮箱地址，以逗号或分号分隔。"))
            .child(toolbar)
            .when(self.load_failed && !self.loading, |root| {
                root.child(
                    div()
                        .flex()
                        .gap_2()
                        .flex_wrap()
                        .p_2()
                        .child(
                            MaterialButton::new("save-raw-draft")
                                .child("另存原始数据…")
                                .on_click(cx.listener(|this, _, _, cx| this.save_raw_draft(cx))),
                        )
                        .child(
                            MaterialButton::new("discard-bad-draft")
                                .child("放弃并新建草稿")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.discard_bad_draft(window, cx);
                                })),
                        ),
                )
            })
            .child(self.draft_toolbar(cx))
            .child(div().text_size(px(14.)).child(self.account_scope.as_ref().map(|(_,address)| format!("发件账号：{address} · 草稿仅本地保存")).unwrap_or_else(||"未关联发件账号 · 本地草稿".into())))
            .child(self.fields_toolbar(cx))
            .child(self.send_toolbar(cx))
            .child(TextField::new("link-url", &self.link))
            .child(div().flex().gap_4().flex_1().min_h_0()
                .child(div().flex().flex_col().gap_2().flex_1().min_w_0()
                    .child(format!(
                        "原生编辑（粗斜体直接显示；当前区块：{}；列表/引用中回车续项）",
                        self.block_label(cx)
                    ))
                    .child(div().id("rich-editor").flex_1().min_h_0().border_1().border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                        .p_3().text_size(px(18.)).font_family(self.font_family.clone())
                        .capture_action(cx.listener(|this, _: &Undo, window, cx| { this.history(false, window, cx); cx.stop_propagation(); }))
                        .capture_action(cx.listener(|this, _: &Redo, window, cx| { this.history(true, window, cx); cx.stop_propagation(); }))
                        .capture_key_down(cx.listener(Self::key))
                        .child(Editor::new(&self.input))))
                .child(div().flex().flex_col().gap_2().flex_1().min_w_0()
                    .child("受控 HTML 预览（链接仅显示，不打开）")
                    .child(div().id("draft-preview").flex_1().min_h_0().overflow_y_scroll().p_3()
                        .child(TextView::new(&self.preview).font_family(self.font_family.clone()).on_link_click(|_, _, _, _| {})))))
            .child(div().text_size(px(13.)).child(format!("{state} · {}", self.status)))
            .child(div().text_size(px(12.)).child("草稿保存在本机；只有确认发送才提交，不自动重发。附件最多32个，总计25MiB。关闭重开恢复最后确认版本。粘贴仅纯文本。"))
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
