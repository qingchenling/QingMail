//! 不可信 MIME → 受控原生阅读树 / 单文件浏览器阅读副本。
//! 原始字节保留，输出均为可重建的派生结果。
mod copies;
pub mod draft;
mod draft_store;
pub use draft_store::DraftStore;
mod css;
mod document;
mod remote;
mod sanitize;

pub use copies::CopyStore;
pub use document::{Document, Element, Node};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageFormat, ImageReader, Limits};
use mail_parser::{MessageParser, PartKind};
use std::{collections::BTreeMap, fmt, io::Cursor, sync::Arc};
use url::Url;

pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_BODY_BYTES: usize = 512 * 1024;
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_IMAGES: usize = 32;
pub const COPY_CSP: &str = "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src 'none'; media-src 'none'; object-src 'none'; frame-src 'none'; connect-src 'none'; base-uri 'none'; form-action 'none'";

#[derive(Debug)]
pub struct ContentError(pub String);
impl fmt::Display for ContentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for ContentError {}

#[derive(Clone, Debug)]
pub struct Attachment {
    /// 仅供展示，不作为路径使用。
    pub name: String,
    pub media_type: String,
    pub content_id: Option<String>,
    pub bytes: Arc<[u8]>,
}

#[derive(Clone, Debug)]
pub struct InlineImage {
    /// 只在当前邮件内查找：cid: 标识或明确授权后的绝对 URL。
    pub key: String,
    pub png: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    data_uri: String,
}

#[derive(Clone, Debug)]
pub struct MailContent {
    pub raw: Arc<[u8]>,
    pub subject: String,
    pub sender: String,
    pub plain: String,
    pub document: Document,
    pub attachments: Vec<Attachment>,
    pub images: Vec<InlineImage>,
    pub warnings: Vec<String>,
    pub remote_images: Vec<String>,
    pub is_html: bool,
    original_html: Option<String>,
    browser_body: String,
    browser_styles: String,
}

pub fn parse(raw: &[u8]) -> Result<MailContent, ContentError> {
    if raw.len() > MAX_MESSAGE_BYTES {
        return Err(ContentError("邮件超过 8 MiB 的原型解析上限".into()));
    }
    let mut result = MailContent {
        raw: Arc::from(raw),
        subject: "（无主题）".into(),
        sender: "（未知发件人）".into(),
        plain: String::new(),
        document: Document::default(),
        attachments: Vec::new(),
        images: Vec::new(),
        warnings: Vec::new(),
        remote_images: Vec::new(),
        is_html: false,
        original_html: None,
        browser_body: String::new(),
        browser_styles: String::new(),
    };
    let Some(message) = MessageParser::new()
        .max_depth(32)
        .max_parts(256)
        .max_encoded_nesting(4)
        .parse(raw)
    else {
        result
            .warnings
            .push("邮件结构无法识别，显示受限原始文本".into());
        result.plain = bounded(&String::from_utf8_lossy(raw), MAX_BODY_BYTES).to_owned();
        result.finish_plain();
        return Ok(result);
    };
    result.subject = bounded(message.subject().unwrap_or("（无主题）"), 1024).to_owned();
    if let Some(sender) = message.from().and_then(|senders| senders.first()) {
        result.sender = bounded(
            &format!(
                "{} <{}>",
                sender.name().unwrap_or(""),
                sender.address().unwrap_or("")
            ),
            1024,
        )
        .to_owned();
    }
    if message.parts().any(|part| part.has_problems()) {
        result
            .warnings
            .push("MIME 结构损坏或达到解析限制；已保留可读取部分".into());
    }
    let root_parts = message.root().parts().collect::<Vec<_>>();
    let mut cid_counts = BTreeMap::<String, usize>::new();
    for part in &root_parts {
        if let Some(cid) = part.content_id().and_then(normalize_cid) {
            *cid_counts.entry(cid).or_default() += 1;
        }
    }
    let mut total_png_bytes = 0;
    for part in &root_parts {
        if part.is_multipart()
            || matches!(part.kind(), PartKind::Text | PartKind::Html)
                && part.attachment_name().is_none()
        {
            continue;
        }
        let (bytes, problems) = part.decoded_checked();
        if !problems.is_empty() {
            result.warnings.push("附件的传输编码存在损坏".into());
        }
        let media_type = part
            .content_type()
            .map(|value| {
                format!(
                    "{}/{}",
                    value.ctype(),
                    value.subtype().unwrap_or("octet-stream")
                )
            })
            .unwrap_or_else(|| "application/octet-stream".into());
        let cid = part.content_id().and_then(normalize_cid);
        result.attachments.push(Attachment {
            name: bounded(part.attachment_name().unwrap_or("未命名附件"), 255).to_owned(),
            media_type: media_type.clone(),
            content_id: cid.clone(),
            bytes: Arc::from(bytes.as_ref()),
        });
        let Some(cid) = cid else {
            continue;
        };
        if cid_counts[&cid] != 1 {
            result
                .warnings
                .push("重复 Content-ID 的图片已阻止，避免歧义映射".into());
            continue;
        }
        if result.images.len() >= MAX_IMAGES {
            result
                .warnings
                .push("内嵌资源超过 32 项，后续图片已省略".into());
            continue;
        }
        match validate_image(&bytes, &media_type, format!("cid:{cid}")) {
            Ok(image) if total_png_bytes + image.png.len() <= MAX_MESSAGE_BYTES => {
                total_png_bytes += image.png.len();
                result.images.push(image);
            }
            _ => result
                .warnings
                .push("内嵌图片格式不支持、损坏或超过尺寸/体积限制，已省略".into()),
        }
    }
    if let Some(part) = message
        .text_body()
        .find(|part| matches!(part.kind(), PartKind::Text))
        && let Some((plain, problems)) = part.text_checked()
    {
        if !problems.is_empty() {
            result
                .warnings
                .push("纯文本编码存在损坏或未知字符集，已尽量解码".into());
        }
        if plain.len() > MAX_BODY_BYTES {
            result
                .warnings
                .push("纯文本超过 512 KiB，显示受限预览".into());
        }
        result.plain = bounded(&plain, MAX_BODY_BYTES).to_owned();
    }
    if let Some(part) = message
        .html_body()
        .find(|part| matches!(part.kind(), PartKind::Html))
        && let Some((html, problems)) = part.text_checked()
    {
        result.is_html = true;
        if !problems.is_empty() {
            result
                .warnings
                .push("HTML 编码存在损坏或未知字符集，已尽量解码".into());
        }
        match sanitize::sanitize(&html, &result.images) {
            Ok(sanitized) => {
                result.document = sanitized.document;
                result.browser_body = sanitized.body;
                result.browser_styles = sanitized.styles;
                result.remote_images = sanitized.remote_images;
                result.original_html = Some(html.into_owned());
                if result.plain.trim().is_empty() {
                    result.plain = result.document.plain_text();
                }
                if result.document.plain_text().is_empty() && result.images.is_empty() {
                    result
                        .warnings
                        .push("HTML 清理后没有可读正文，改用纯文本或明确占位".into());
                    result.finish_plain();
                }
            }
            Err(reason) => {
                result.warnings.push(reason.into());
                if result.plain.trim().is_empty() {
                    result.plain = mail_parser::html_to_text(bounded(&html, MAX_BODY_BYTES));
                }
                result.finish_plain();
            }
        }
    } else {
        result.finish_plain();
    }
    result.warnings.sort();
    result.warnings.dedup();
    Ok(result)
}

impl MailContent {
    fn finish_plain(&mut self) {
        if self.plain.trim().is_empty() {
            self.plain = "（没有可读取的正文，原始邮件已保留）".into();
        }
        self.document = Document::from_plain(&self.plain);
        self.browser_body = format!(
            "<section class=\"qingmail-body\"><pre>{}</pre></section>",
            escape(&self.plain)
        );
        self.browser_styles.clear();
    }

    /// 内容清理与 CSP 相互独立；本地副本不默认发起网络请求。
    pub fn browser_html(&self) -> String {
        format!(
            "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{}\"><meta name=\"referrer\" content=\"no-referrer\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>body{{margin:24px auto;padding:0 20px;max-width:1000px;font-family:sans-serif;line-height:1.6;background:white;color:#202124}}pre{{white-space:pre-wrap;overflow-wrap:anywhere}}img{{max-width:100%;height:auto}}table{{max-width:100%}}.qingmail-meta{{border-bottom:1px solid #ccc;padding-bottom:12px;margin-bottom:20px}}{}</style></head><body><header class=\"qingmail-meta\"><h1>{}</h1><p>{}</p><small>本地阅读副本 · 远程资源默认阻止 · 不执行邮件脚本</small></header>{}</body></html>",
            COPY_CSP,
            escape(&self.subject),
            self.browser_styles,
            escape(&self.subject),
            escape(&self.sender),
            self.browser_body
        )
    }
}

pub(crate) fn validate_image(
    bytes: &[u8],
    media_type: &str,
    key: String,
) -> Result<InlineImage, ContentError> {
    let invalid = || ContentError("图片未通过格式、尺寸或体积校验".into());
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(invalid());
    }
    let format = image::guess_format(bytes).map_err(|_| invalid())?;
    let expected = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => return Err(invalid()),
    };
    if media_type.split(';').next().unwrap_or("").trim() != expected {
        return Err(invalid());
    }
    let dimensions = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| invalid())?;
    if dimensions.0 == 0
        || dimensions.1 == 0
        || dimensions.0 > 4096
        || dimensions.1 > 4096
        || u64::from(dimensions.0) * u64::from(dimensions.1) > 4_000_000
    {
        return Err(invalid());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(32 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| invalid())?;
    let mut png = Cursor::new(Vec::new());
    decoded
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|_| invalid())?;
    let png = png.into_inner();
    if png.len() > MAX_IMAGE_BYTES {
        return Err(invalid());
    }
    Ok(InlineImage {
        key,
        width: dimensions.0,
        height: dimensions.1,
        data_uri: format!("data:image/png;base64,{}", STANDARD.encode(&png)),
        png: Arc::from(png),
    })
}

pub(crate) fn normalize_cid(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches('<').trim_end_matches('>');
    (!value.is_empty()
        && value.len() <= 256
        && !value.chars().any(|character| {
            character.is_control() || character.is_whitespace() || matches!(character, '<' | '>')
        }))
    .then(|| value.to_owned())
}
pub(crate) fn cid_url(value: &str) -> Option<String> {
    let (scheme, id) = value.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("cid") {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(id)
        .decode_utf8()
        .ok()?;
    normalize_cid(&decoded).map(|id| format!("cid:{id}"))
}
pub(crate) fn safe_link(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https" | "mailto")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}
pub(crate) fn http_url(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}
pub(crate) fn bounded(value: &str, max_bytes: usize) -> &str {
    &value[..value.floor_char_boundary(value.len().min(max_bytes))]
}
pub(crate) fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
