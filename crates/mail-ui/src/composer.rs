//! 原生本地写信草稿；不连接邮箱，不发送邮件。
use gpui_kit::{
    App, Bounds, Context, Entity, EntityInputHandler, FontStyle, FontWeight, HighlightStyle,
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
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};

#[path = "composer_fields.rs"]
mod fields;
#[path = "composer_probe.rs"]
mod probe;

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
    if crate::lifecycle::reuse_editor(cx) {
        return;
    }
    let bounds = Bounds::centered(None, size(px(1180.), px(820.)), cx);
    let result = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("QingMail — 本地写信草稿".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        |window, cx| cx.new(|cx| Composer::new(window, cx)),
    );
    match result {
        Ok((window, entity)) => crate::lifecycle::register_editor(window, entity.downgrade(), cx),
        Err(e) => eprintln!("无法打开编辑验证窗口：{e}"),
    }
}

pub(super) struct Composer {
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
    drafts: Vec<StoredDraft>,
    saved: Option<u64>,
    saving: bool,
    auto_save_blocked: bool,
    loading: bool,
    last_edit: Instant,
    status: String,
    _subscription: Subscription,
    probe_step: usize,
}

impl Composer {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let names = window.text_system().all_font_names();
        let font_family: SharedString = ["Noto Sans", "Liberation Sans", "DejaVu Sans"]
            .into_iter()
            .find(|name| names.iter().any(|available| available == name))
            .unwrap_or(".SystemUIFont")
            .into();
        let draft = Draft::plain(SAMPLE).expect("bounded sample");
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
                .default_value(SAMPLE);
            let session = session.clone();
            input.set_highlighter_factory(
                Rc::new(move |_| {
                    Some(Box::new(RichStyles {
                        session: session.clone(),
                        projection: None,
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
            drafts: vec![],
            saved: None,
            saving: false,
            auto_save_blocked: false,
            loading: true,
            last_edit: Instant::now(),
            status: "正在检查本地验证草稿…".into(),
            _subscription: subscription,
            probe_step: 0,
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
        let request = store.open_editor(self.session.borrow().draft.clone());
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
                        this.sync_input(window, cx);
                        this.status = "已恢复本地草稿 · 不关联真实邮箱".into();
                        this.refresh_drafts(cx);
                    }
                    Err(e) => this.status = format!("自动保存不可用，原文件未改动：{e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_drafts(&mut self, cx: &mut Context<Self>) {
        let request = crate::lifecycle::storage(cx).list();
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
            client.select(self.drafts[index].id.clone())
        } else {
            client.new_draft(Draft::plain("").expect("empty draft"))
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
                    .child("新建本地草稿")
                    .disabled(self.loading || self.saving || self.closing || self.importing)
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
                    .disabled(self.loading || self.saving || self.closing || self.importing)
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
            self.input
                .update(cx, |input, cx| input.set_readonly(false, cx));
        }
        if probe::enabled() {
            self.probe(window, cx);
        }
        if self.composing(window, cx) {
            return;
        }
        if self.closing && !self.saving && self.saved == Some(self.session.borrow().revision) {
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
        if self.loading || self.closing || self.store.is_none() || self.composing(window, cx) {
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
        if self.loading || self.closing || self.store.is_none() || self.composing(window, cx) {
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
            if event.keystroke.modifiers.control && !event.keystroke.modifiers.alt && event.keystroke.key == "s" {
                if !this.composing(window, cx) { this.save(cx); }
                cx.stop_propagation();
                window.prevent_default();
            }
        })).size_full().child(TextSelectionLayer).flex().flex_col().gap_3().p_4()
            .bg(theme::color(ColorRole::SURFACE)).text_color(theme::color(ColorRole::ON_SURFACE))
            .child(div().text_size(px(22.)).child("QingMail · 本地写信草稿"))
            .child(div().text_size(px(13.)).child("选中后设置格式；Ctrl+B / I、Ctrl+Z / Shift+Z、Ctrl+S。收件人填写独立邮箱地址，以逗号或分号分隔。"))
            .child(toolbar)
            .child(self.draft_toolbar(cx))
            .child(self.fields_toolbar(cx))
            .child(TextField::new("link-url", &self.link))
            .child(div().flex().gap_4().flex_1().min_h_0()
                .child(div().flex().flex_col().gap_2().flex_1().min_w_0()
                    .child("原生编辑（粗斜体直接显示，段落结构见预览）")
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
            .child(div().text_size(px(12.)).child("本地草稿尚未关联发件账号，不会发送。附件最多32个，总计25MiB。关闭重开恢复最后确认版本。粘贴仅纯文本。"))
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
