//! 复用安全阅读组件：合成验证与按需下载的邮件均使用同一受控文档。
use crate::{
    reply::ReplyMode,
    ui::{icon, quiet, tool},
};
use gpui_kit::{
    App, Bounds, Context, Entity, Image, ImageFormat, KeyDownEvent, Render, TitlebarOptions,
    Window, WindowBounds, WindowOptions,
    base::{TextSelectionLayer, TextView, TextViewState},
    div, img,
    prelude::*,
    px, size,
};
use gpui_kit_material::{prelude::*, theme};
use mail_content::{CopyStore, MailContent};
use mail_core::AccountId;
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

const SAMPLES: &[(&str, &[u8])] = &[
    (
        "中文纯文本",
        include_bytes!("../../../tests/fixtures/mime/01-plain-utf8.eml"),
    ),
    (
        "GB18030 / 折行头",
        include_bytes!("../../../tests/fixtures/mime/02-gb18030.eml"),
    ),
    (
        "HTML / CID / 附件",
        include_bytes!("../../../tests/fixtures/mime/03-alternative-related.eml"),
    ),
    (
        "恶意资源安全探针",
        include_bytes!("../../../tests/fixtures/mime/04-adversarial.eml"),
    ),
    (
        "重复 CID / 超大图",
        include_bytes!("../../../tests/fixtures/mime/05-duplicate-missing-cid.eml"),
    ),
    (
        "损坏 MIME",
        include_bytes!("../../../tests/fixtures/mime/06-broken.eml"),
    ),
    (
        "HTML-only 长引用",
        include_bytes!("../../../tests/fixtures/mime/07-html-only-long.eml"),
    ),
    (
        "超限嵌套",
        include_bytes!("../../../tests/fixtures/mime/08-deep.eml"),
    ),
    (
        "大正文降级",
        include_bytes!("../../../tests/fixtures/mime/09-large.eml"),
    ),
    (
        "嵌套邮件 CID 隔离",
        include_bytes!("../../../tests/fixtures/mime/10-nested-message.eml"),
    ),
];

pub(super) fn open(cx: &mut App) {
    if crate::lifecycle::reuse_reader(cx) {
        return;
    }
    let bounds = Bounds::centered(None, size(px(1100.0), px(760.0)), cx);
    let result = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("QingMail — P0 邮件内容验证".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        |_, cx| cx.new(Reader::new),
    );
    match result {
        Ok((window, _)) => crate::lifecycle::register_reader(window, cx),
        Err(error) => eprintln!("无法打开阅读验证窗口：{error}"),
    }
}

pub(super) fn open_content(
    content: MailContent,
    account: Option<(AccountId, String)>,
    cx: &mut App,
) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1100.), px(760.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("QingMail — 简化阅读".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    if gpui_kit::open_window(options, cx, |_, cx| {
        cx.new(|cx| Reader::from_content(content, true, account, cx))
    })
    .is_err()
    {
        eprintln!("无法打开邮件阅读窗口");
    }
}

pub(super) struct Reader {
    external: bool,
    embedded: bool,
    account: Option<(AccountId, String)>,
    selected: usize,
    content: MailContent,
    text: Entity<TextViewState>,
    plain: bool,
    font_size: f32,
    busy: bool,
    status: String,
    images: Vec<Arc<Image>>,
    active_downloads: std::collections::HashSet<String>,
    downloads: std::collections::HashMap<String, mail_protocol::DownloadInfo>,
    download_cancel: std::collections::HashMap<String, Arc<std::sync::atomic::AtomicBool>>,
}

impl Reader {
    fn new(cx: &mut Context<Self>) -> Self {
        let content = mail_content::parse(SAMPLES[0].1).expect("bounded synthetic sample");
        Self::from_content(content, false, None, cx)
    }
    pub(super) fn from_content(
        content: MailContent,
        external: bool,
        account: Option<(AccountId, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = cx.new(|cx| TextViewState::html(&content.document.native_html(), cx));
        let mut this = Self {
            active_downloads: Default::default(),
            download_cancel: Default::default(),
            downloads: Default::default(),
            external,
            embedded: false,
            account,
            selected: 0,
            content,
            text,
            plain: false,
            font_size: crate::settings::prefs(cx).reader_font,
            busy: false,
            status: if external {
                "已打开缓存邮件 · 简化阅读 · 远程图片默认阻止".into()
            } else {
                "合成邮件 · 不连接真实邮箱".into()
            },
            images: Vec::new(),
        };
        this.refresh_text(cx);
        if external && std::env::args().any(|a| a == "--body-probe") {
            cx.spawn(async move |entity, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
                entity.update(cx, |this, cx| {
                    assert!(this.external);
                    assert!(!this.content.plain.is_empty());
                    assert!(!this.images.is_empty());
                    assert!(!this.content.attachments.is_empty());
                    this.plain = true;
                    this.refresh_text(cx);
                    this.plain = false;
                    this.refresh_text(cx);
                    CopyStore::open(&cache_parent().unwrap()).unwrap().write(&this.content).unwrap();
                    eprintln!("QINGMAIL_BODY_PROBE: PASS · offline cache / native reader / CID / attachments / plain toggle / browser copy");
                    cx.quit();
                }).expect("body reader probe");
            }).detach();
        }
        this
    }

    pub(super) fn embedded(
        content: MailContent,
        account: Option<(AccountId, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut reader = Self::from_content(content, true, account, cx);
        reader.embedded = true;
        reader
    }

    fn refresh_text(&mut self, cx: &mut Context<Self>) {
        let html = if self.plain {
            mail_content::Document::from_plain(&self.content.plain).native_html()
        } else {
            self.content.document.native_html()
        };
        self.text.update(cx, |text, cx| text.set_text(&html, cx));
        self.images = self
            .content
            .images
            .iter()
            .map(|image| Arc::new(Image::from_bytes(ImageFormat::Png, image.png.to_vec())))
            .collect();
        cx.notify();
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.selected = index;
        self.busy = true;
        self.status = "正在解析合成邮件…".into();
        cx.notify();
        let task = cx
            .background_executor()
            .spawn(async move { mail_content::parse(SAMPLES[index].1) });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(content) => {
                        this.content = content;
                        this.plain = false;
                        this.status = "合成邮件已解析，远程资源默认阻止".into();
                        this.refresh_text(cx);
                    }
                    Err(error) => this.status = format!("无法解析：{error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn browser_copy(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = "正在生成本地阅读副本…".into();
        cx.notify();
        let content = self.content.clone();
        let client = crate::lifecycle::storage(cx).ipc_client();
        let task = cx.background_executor().spawn(async move {
            if let (Some(client), Some(handle)) = (client, content.display_handle.clone()) {
                let reply = mail_daemon::client::run(
                    &client,
                    mail_protocol::BusinessOperation::BrowserCopy { content: handle },
                )
                .map_err(std::io::Error::other)?;
                let mail_protocol::Reply::BrowserCopy { url } = reply else {
                    return Err(std::io::Error::other("副本响应无效"));
                };
                return url::Url::parse(&url)
                    .map_err(std::io::Error::other)?
                    .to_file_path()
                    .map_err(|_| std::io::Error::other("副本URL无效"));
            }
            let parent = cache_parent()?;
            CopyStore::open(&parent)?.write(&content)
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(path) => match url::Url::from_file_path(path) {
                        Ok(url) => {
                            cx.open_url(url.as_str());
                            this.status =
                                "已交给系统默认浏览器；若未打开，请检查默认浏览器配置".into();
                        }
                        Err(()) => this.status = "副本已生成，但本地地址转换失败".into(),
                    },
                    Err(error) => this.status = format!("无法生成阅读副本：{error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_remote(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = "仅为当前邮件加载远程图片…".into();
        cx.notify();
        let mut content = self.content.clone();
        let client = crate::lifecycle::storage(cx).ipc_client();
        let task = cx.background_executor().spawn(async move {
            if let (Some(client), Some(handle)) = (client, content.display_handle.clone()) {
                return match mail_daemon::client::content(
                    &client,
                    mail_protocol::BusinessOperation::RemoteImages { content: handle },
                ) {
                    Ok(loaded) => {
                        let count = loaded.images.len();
                        (loaded, Ok(count))
                    }
                    Err(e) => (content, Err(mail_content::ContentError(e))),
                };
            }
            let result = content.load_remote_images_after_consent();
            (content, result)
        });
        cx.spawn(async move |entity, cx| {
            let (content, result) = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.content = content;
                this.status = match result {
                    Ok(count) => {
                        format!("已校验并加载 {count} 张图片；本地/私有网络和主动资源继续阻止")
                    }
                    Err(error) => format!("加载失败：{error}"),
                };
                this.refresh_text(cx);
            });
        })
        .detach();
    }

    fn clear_copies(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        let client = crate::lifecycle::storage(cx).ipc_client();
        let task = cx.background_executor().spawn(async move {
            if let Some(client) = client {
                return match mail_daemon::client::run(
                    &client,
                    mail_protocol::BusinessOperation::ClearCopies,
                )
                .map_err(std::io::Error::other)?
                {
                    mail_protocol::Reply::Count(count) => Ok(count as usize),
                    _ => Err(std::io::Error::other("清理响应无效")),
                };
            }
            CopyStore::open(&cache_parent()?)?.clear()
        });
        cx.spawn(async move |entity, cx| {
            let result = task.await;
            let _ = entity.update(cx, |this, cx| {
                this.busy = false;
                this.status = match result {
                    Ok(count) => {
                        format!("已清理 {count} 份阅读副本；浏览器中尚未读取的副本可能受影响")
                    }
                    Err(error) => format!("清理失败：{error}"),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel_download(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(cancel) = self.download_cancel.get(id) {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        let store = crate::lifecycle::storage(cx);
        let id = id.to_string();
        cx.background_executor()
            .spawn(async move {
                let _ = store.control(mail_protocol::Command::CancelResource { resource: id });
            })
            .detach();
        self.status = "正在取消，当前有界网络请求结束后停止".into();
        cx.notify();
    }
    fn save_remote_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        let attachment = self.content.attachments[index].clone();
        let Some(id) = attachment.remote else { return };
        if !self.active_downloads.insert(id.clone()) {
            return;
        }
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.download_cancel.insert(id.clone(), cancel.clone());
        let store = crate::lifecycle::storage(cx);
        let directory = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let prompt = cx.prompt_for_new_path(&directory, Some("qingmail-attachment.bin"));
        cx.spawn(async move |entity, cx| {
            let result = async {
                let path = match prompt.await {
                    Ok(Ok(Some(path))) => path,
                    Ok(Ok(None)) => return Err("已取消保存".to_string()),
                    _ => return Err("文件选择器不可用".to_string()),
                };
                let db = store.clone();
                let key = id.clone();
                let mut info = match cx
                    .background_executor()
                    .spawn(async move {
                        db.control(mail_protocol::Command::DownloadAttachment { resource: key })
                    })
                    .await?
                {
                    mail_protocol::Reply::Download(info) => info,
                    _ => return Err("下载响应无效".into()),
                };
                loop {
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        return Err("已取消下载/保存".into());
                    }
                    let display = info.clone();
                    let key = id.clone();
                    if entity
                        .update(cx, |this, cx| {
                            this.downloads.insert(key, display);
                            cx.notify();
                        })
                        .is_err()
                    {
                        let db = store.clone();
                        let key = id.clone();
                        cx.background_executor()
                            .spawn(async move {
                                let _ = db.control(mail_protocol::Command::CancelResource {
                                    resource: key,
                                });
                            })
                            .await;
                        return Err("阅读窗口已关闭，下载取消".into());
                    }
                    match info.resource.state {
                        mail_protocol::ResourceState::Ready => break,
                        mail_protocol::ResourceState::Failed => {
                            return Err(info.error.unwrap_or_else(|| "下载失败，可重试".into()));
                        }
                        _ => (),
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(400))
                        .await;
                    let db = store.clone();
                    let key = id.clone();
                    info = match cx
                        .background_executor()
                        .spawn(async move {
                            db.control(mail_protocol::Command::ResourceStatus { resource: key })
                        })
                        .await?
                    {
                        mail_protocol::Reply::Download(info) => info,
                        _ => return Err("下载状态无效".into()),
                    };
                }
                let client = store.ipc_client().ok_or("后台连接不可用")?;
                let resource = info.resource;
                cx.background_executor()
                    .spawn(async move { save_resource(&client, &resource, &path, &cancel) })
                    .await?;
                Ok::<_, String>(())
            }
            .await;
            let _ = entity.update(cx, |this, cx| {
                this.active_downloads.remove(&id);
                this.download_cancel.remove(&id);
                if let Err(error) = &result
                    && let Some(info) = this.downloads.get_mut(&id)
                    && info.resource.state != mail_protocol::ResourceState::Ready
                {
                    info.resource.state = mail_protocol::ResourceState::Failed;
                    info.error = Some(error.clone());
                }
                this.status = match result {
                    Ok(()) => "附件已完整保存（不自动打开）".into(),
                    Err(e) => format!("附件未保存：{e}"),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn save_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        let attachment = self.content.attachments[index].clone();
        if attachment.remote.is_some() {
            self.save_remote_attachment(index, cx);
            return;
        }
        let directory = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        // 不以邮件提供的名字构建路径，保存位置由用户的文件对话框决定。
        let prompt = cx.prompt_for_new_path(&directory, Some("qingmail-attachment.bin"));
        cx.spawn(async move |entity, cx| {
            let result = prompt.await;
            let status = match result {
                Ok(Ok(Some(path))) => {
                    let task = cx.background_executor().spawn(async move {
                        let mut file = fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(0o600)
                            .open(path)?;
                        file.write_all(&attachment.bytes)?;
                        file.sync_all()
                    });
                    match task.await {
                        Ok(()) => "附件已保存（不自动打开）".into(),
                        Err(error) => format!("保存失败（不覆盖已有文件）：{error}"),
                    }
                }
                Ok(Ok(None)) => "已取消保存".into(),
                Ok(Err(error)) => format!("文件选择器不可用：{error}"),
                Err(error) => format!("文件选择器未响应：{error}"),
            };
            let _ = entity.update(cx, |this, cx| {
                this.status = status;
                cx.notify();
            });
        })
        .detach();
    }
}

impl Reader {
    pub(super) fn compose(&mut self, mode: ReplyMode, cx: &mut Context<Self>) {
        let Some((account, address)) = self.account.clone() else {
            return;
        };
        let mut document = crate::reply::document(&self.content, &address, mode);
        let own = crate::lifecycle::own_addresses(cx);
        crate::reply::exclude_own(&mut document.headers, &own);
        self.status = match mode {
            ReplyMode::Forward => "已创建转发草稿，请在写信窗口确认收件人".into(),
            ReplyMode::Reply => "已创建回复草稿".into(),
            ReplyMode::ReplyAll => "已创建全部回复草稿".into(),
        };
        cx.defer(move |cx| {
            crate::composer::open_document(account, address, document, cx);
        });
        cx.notify();
    }
    fn reply_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let key = &event.keystroke;
        if !key.modifiers.control || key.modifiers.alt || self.account.is_none() {
            return false;
        }
        let mode = match key.key.as_str() {
            "r" if key.modifiers.shift => ReplyMode::ReplyAll,
            "r" => ReplyMode::Reply,
            "f" if key.modifiers.shift => ReplyMode::Forward,
            _ => return false,
        };
        self.compose(mode, cx);
        true
    }
}

impl Render for Reader {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut replies = div().flex().flex_wrap().gap_2();
        if self.account.is_some() && !self.embedded {
            for (id, label, symbol, mode) in [
                ("reply", "回复", "reply", ReplyMode::Reply),
                ("reply-all", "回复全部", "reply_all", ReplyMode::ReplyAll),
                ("forward", "转发", "forward", ReplyMode::Forward),
            ] {
                replies = replies.child(
                    quiet(id, label)
                        .icon(icon(symbol))
                        .on_click(cx.listener(move |this, _, _, cx| this.compose(mode, cx))),
                );
            }
        }
        let toolbar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(
                div()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .text_size(px(12.))
                    .bg(theme::color(ColorRole::SECONDARY_CONTAINER))
                    .text_color(theme::color(ColorRole::ON_SECONDARY_CONTAINER))
                    .child(if self.plain {
                        "纯文本"
                    } else {
                        "简化阅读"
                    }),
            )
            .child(
                quiet(
                    "plain-toggle",
                    if self.plain {
                        "简化阅读"
                    } else {
                        "纯文本"
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.plain = !this.plain;
                    this.refresh_text(cx);
                })),
            )
            .child(
                tool("font-smaller", "text_decrease", "缩小字号")
                    .button_size(ButtonSize::ExtraSmall)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.font_size = (this.font_size - 2.).max(12.);
                        let mut preferences = crate::settings::prefs(cx);
                        preferences.reader_font = this.font_size;
                        crate::settings::set(cx, preferences);
                        cx.notify();
                    })),
            )
            .child(
                tool("font-larger", "text_increase", "放大字号")
                    .button_size(ButtonSize::ExtraSmall)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.font_size = (this.font_size + 2.).min(28.);
                        let mut preferences = crate::settings::prefs(cx);
                        preferences.reader_font = this.font_size;
                        crate::settings::set(cx, preferences);
                        cx.notify();
                    })),
            )
            .child(
                quiet("browser-copy", "在浏览器中打开")
                    .icon(icon("open_in_new"))
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.browser_copy(cx))),
            )
            .child(
                tool("clear-copies", "cleaning_services", "清理浏览器副本")
                    .button_size(ButtonSize::ExtraSmall)
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.clear_copies(cx))),
            );
        let samples = div()
            .id("content-samples")
            .w(px(if self.external { 0. } else { 240. }))
            .flex_shrink_0()
            .overflow_y_scroll()
            .children(
                SAMPLES
                    .iter()
                    .enumerate()
                    .take(if self.external { 0 } else { SAMPLES.len() })
                    .map(|(index, (label, _))| {
                        ListItem::new(index, *label)
                            .selected(self.selected == index)
                            .on_click(cx.listener(move |this, _, _, cx| this.select(index, cx)))
                    }),
            );
        let body =
            div()
                .id("content-reader")
                .flex_1()
                .min_w_0()
                .overflow_y_scroll()
                .px_6()
                .py_5()
                .flex()
                .flex_col()
                .gap_4()
                .when(!self.embedded && self.account.is_some(), |body| {
                    body.child(replies)
                })
                .child(
                    div()
                        .text_size(px(26.0))
                        .font_weight(gpui_kit::FontWeight::BOLD)
                        .child(self.content.subject.clone()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .pb_4()
                        .border_b_1()
                        .border_color(theme::color(ColorRole::OUTLINE_VARIANT))
                        .child(
                            div()
                                .w(px(44.))
                                .h(px(44.))
                                .flex_none()
                                .rounded_full()
                                .bg(theme::color(ColorRole::SECONDARY_CONTAINER))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(icon("person")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(14.))
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .child(self.content.sender.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                                        .child(format!("收件人：{}", self.content.to.join("、"))),
                                ),
                        )
                        .when(!self.content.date_text.is_empty(), |header| {
                            header.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                                    .child(self.content.date_text.clone()),
                            )
                        }),
                )
                .child(toolbar)
                .when(!self.content.remote_images.is_empty(), |body| {
                    body.child(
                        div()
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(theme::color(ColorRole::TERTIARY_CONTAINER))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(icon("shield"))
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .child("已阻止远程图片，加载可能暴露阅读行为"),
                            )
                            .child(
                                quiet("remote-consent", "仅此邮件加载")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.load_remote(cx))),
                            ),
                    )
                })
                .children(self.content.warnings.iter().map(|warning| {
                    div()
                        .text_color(theme::color(ColorRole::ERROR))
                        .child(warning.clone())
                }))
                .child(
                    div()
                        .text_size(px(self.font_size))
                        .child(TextView::new(&self.text).selectable(true)),
                )
                .when(!self.plain && !self.images.is_empty(), |body| {
                    body.child(
                        div()
                            .child("已校验的内嵌图片（简化视图集中显示）")
                            .children(self.images.iter().zip(&self.content.images).map(
                                |(image, resource)| {
                                    img(image.clone())
                                        .w(px(resource.width.min(600) as f32))
                                        .h(px(resource.height.min(400) as f32))
                                },
                            )),
                    )
                })
                .children(self.content.attachments.iter().enumerate().map(
                    |(index, attachment)| {
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(format!(
                                "附件：{} · {} · 大小约 {} 字节",
                                attachment.name, attachment.media_type, attachment.declared_size
                            ))
                            .child(
                                quiet(
                                    ("save-attachment", index),
                                    if attachment.remote.is_some() {
                                        "下载/另存为"
                                    } else {
                                        "另存为"
                                    },
                                )
                                .disabled(
                                    attachment
                                        .remote
                                        .as_ref()
                                        .is_some_and(|id| self.active_downloads.contains(id)),
                                )
                                .icon(icon("download"))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.save_attachment(index, cx),
                                )),
                            )
                            .when_some(attachment.remote.clone(), |row, id| {
                                let info = self.downloads.get(&id);
                                row.child(match info {
                                    Some(info) => format!(
                                        "{} · {}/{} 编码字节{}",
                                        download_state(info.resource.state),
                                        info.received,
                                        info.total,
                                        info.error
                                            .as_ref()
                                            .map(|e| format!(" · {e}"))
                                            .unwrap_or_default()
                                    ),
                                    None => "按需获取，不随正文下载".into(),
                                })
                                .when(
                                    self.active_downloads.contains(&id),
                                    |row| {
                                        row.child(
                                            quiet(("cancel-attachment", index), "取消下载")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.cancel_download(&id, cx)
                                                })),
                                        )
                                    },
                                )
                            })
                    },
                ));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.reply_key(event, cx) {
                    cx.stop_propagation();
                    window.prevent_default();
                }
            }))
            .child(TextSelectionLayer)
            .when(!self.embedded, |root| {
                root.child(div().p_3().text_size(px(18.0)).child(if self.external {
                    "QingMail · 简化阅读"
                } else {
                    "QingMail · P0 邮件内容验证"
                }))
            })
            .child(div().flex().flex_1().min_h_0().child(samples).child(body))
            .child(
                div()
                    .px_6()
                    .py_2()
                    .text_size(px(11.0))
                    .text_color(theme::color(ColorRole::ON_SURFACE_VARIANT))
                    .child(self.status.clone()),
            )
    }
}

fn cache_parent() -> std::io::Result<PathBuf> {
    if let Some(root) = std::env::var_os("QINGMAIL_P0_DRAFT_ROOT") {
        let path = PathBuf::from(root).join("reading-cache");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)?;
        return Ok(path);
    }
    let root = match std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        Some(path) => path,
        None => std::env::var_os("HOME")
            .map(|home| Path::new(&home).join(".cache"))
            .ok_or_else(|| std::io::Error::other("找不到用户缓存目录"))?,
    };
    let path = root.join("qingmail-p0");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&path)?;
    Ok(path)
}

fn download_state(state: mail_protocol::ResourceState) -> &'static str {
    match state {
        mail_protocol::ResourceState::NotCached => "未缓存",
        mail_protocol::ResourceState::Downloading => "下载中",
        mail_protocol::ResourceState::Ready => "已下载",
        mail_protocol::ResourceState::Failed => "失败",
    }
}
fn save_resource(
    client: &mail_protocol::transport::Client,
    resource: &mail_protocol::ResourceRef,
    path: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let parent = path.parent().ok_or("保存目录无效")?;
    let temporary = parent.join(format!(
        ".qingmail-{}.partial",
        mail_protocol::transport::request_id().map_err(|e| e.to_string())?
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|e| format!("无法创建临时文件，请检查空间：{e}"))?;
    let result = (|| {
        let mut offset = 0;
        loop {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err("已取消保存，未发布部分文件".into());
            }
            let (chunk, bytes) = client
                .read_chunk(&resource.id, offset, mail_protocol::MAX_CHUNK_BYTES)
                .map_err(|e| e.to_string())?;
            if chunk.total != resource.size {
                return Err("附件总量已变化，未发布文件".to_string());
            }
            file.write_all(&bytes)
                .map_err(|e| format!("保存失败，请检查磁盘空间：{e}"))?;
            offset += bytes.len() as u64;
            if chunk.complete {
                break;
            }
        }
        file.sync_all().map_err(|e| e.to_string())?;
        // hard_link原子且不覆盖已有文件；不把部分数据发布为用户选择的最终文件。
        fs::hard_link(&temporary, path)
            .map_err(|e| format!("无法发布附件（不覆盖已有文件）：{e}"))?;
        Ok(())
    })();
    drop(file);
    let _ = fs::remove_file(temporary);
    result
}
