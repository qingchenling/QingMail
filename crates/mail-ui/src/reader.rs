//! P0 合成邮件阅读窗口，不连接真实账号。
use gpui_kit::{
    App, Bounds, Context, Entity, Image, ImageFormat, Render, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
    base::{TextSelectionLayer, TextView, TextViewState},
    div, img,
    prelude::*,
    px, size,
};
use gpui_kit_material::{prelude::*, theme};
use mail_content::{CopyStore, MailContent};
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

struct Reader {
    selected: usize,
    content: MailContent,
    text: Entity<TextViewState>,
    plain: bool,
    font_size: f32,
    busy: bool,
    status: String,
    images: Vec<Arc<Image>>,
}

impl Reader {
    fn new(cx: &mut Context<Self>) -> Self {
        let content = mail_content::parse(SAMPLES[0].1).expect("bounded synthetic sample");
        let text = cx.new(|cx| TextViewState::html(&content.document.native_html(), cx));
        Self {
            selected: 0,
            content,
            text,
            plain: false,
            font_size: 16.0,
            busy: false,
            status: "合成邮件 · 不连接真实邮箱".into(),
            images: Vec::new(),
        }
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
        let task = cx.background_executor().spawn(async move {
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
        let task = cx.background_executor().spawn(async move {
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
        let task = cx
            .background_executor()
            .spawn(async { CopyStore::open(&cache_parent()?)?.clear() });
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

    fn save_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        let attachment = self.content.attachments[index].clone();
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

impl Render for Reader {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                MaterialButton::new("browser-copy")
                    .child("在浏览器中打开")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.browser_copy(cx))),
            )
            .child(
                MaterialButton::new("plain-toggle")
                    .variant(ButtonVariant::Tonal)
                    .child(if self.plain {
                        "简化阅读"
                    } else {
                        "纯文本"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.plain = !this.plain;
                        this.refresh_text(cx);
                    })),
            )
            .child(
                MaterialButton::new("font-smaller")
                    .child("字号 −")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.font_size = (this.font_size - 2.0).max(12.0);
                        cx.notify();
                    })),
            )
            .child(
                MaterialButton::new("font-larger")
                    .child("字号 ＋")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.font_size = (this.font_size + 2.0).min(28.0);
                        cx.notify();
                    })),
            )
            .child(
                MaterialButton::new("clear-copies")
                    .child("立即清理副本")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.clear_copies(cx))),
            );
        let samples = div()
            .id("content-samples")
            .w(px(240.0))
            .flex_shrink_0()
            .overflow_y_scroll()
            .children(SAMPLES.iter().enumerate().map(|(index, (label, _))| {
                ListItem::new(index, *label)
                    .selected(self.selected == index)
                    .on_click(cx.listener(move |this, _, _, cx| this.select(index, cx)))
            }));
        let body =
            div()
                .id("content-reader")
                .flex_1()
                .min_w_0()
                .overflow_y_scroll()
                .p_4()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_size(px(22.0))
                        .child(self.content.subject.clone()),
                )
                .child(div().child(self.content.sender.clone()))
                .child(div().child(if self.plain {
                    "纯文本 · 合成邮件"
                } else {
                    "简化阅读 · 合成邮件"
                }))
                .child(toolbar)
                .child(div().child(format!(
                    "远程资源默认阻止 · 可授权图片 {} 项",
                    self.content.remote_images.len()
                )))
                .when(!self.content.remote_images.is_empty(), |body| {
                    body.child(
                        MaterialButton::new("remote-consent")
                            .child("仅为此邮件加载远程图片")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.load_remote(cx))),
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
                                "附件：{} · {} · {} 字节",
                                attachment.name,
                                attachment.media_type,
                                attachment.bytes.len()
                            ))
                            .child(
                                MaterialButton::new(("save-attachment", index))
                                    .child("另存为")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.save_attachment(index, cx)
                                    })),
                            )
                    },
                ));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::color(ColorRole::SURFACE))
            .text_color(theme::color(ColorRole::ON_SURFACE))
            .child(TextSelectionLayer)
            .child(
                div()
                    .p_3()
                    .text_size(px(18.0))
                    .child("QingMail · P0 邮件内容验证"),
            )
            .child(div().flex().flex_1().min_h_0().child(samples).child(body))
            .child(div().p_3().text_size(px(12.0)).child(self.status.clone()))
    }
}

fn cache_parent() -> std::io::Result<PathBuf> {
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
