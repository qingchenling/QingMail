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
use draft::{Block, Draft, MAX_TEXT};
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
    /// 解析出的独立邮箱地址；回复/转发只用地址，不把显示名拼进字段。
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub reply_to: Vec<String>,
    pub date_unix: i64,
    pub date_text: String,
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
        from: vec![],
        to: vec![],
        cc: vec![],
        reply_to: vec![],
        date_unix: 0,
        date_text: String::new(),
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
    result.from = address_list(message.from());
    result.to = address_list(message.to());
    result.cc = address_list(message.cc());
    result.reply_to = address_list(message.reply_to());
    if let Some(date) = message.date() {
        result.date_unix = date.to_timestamp();
        result.date_text = date.to_rfc3339();
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

fn address_list(list: Option<mail_parser::AddressList<'_>>) -> Vec<String> {
    let mut addresses = Vec::new();
    let Some(list) = list else {
        return addresses;
    };
    let mut push = |mailbox: mail_parser::Mailbox<'_>| {
        if let Some(address) = mailbox.address() {
            let address = address.trim();
            if !address.is_empty() && address.len() <= 320 && !address.chars().any(char::is_control)
            {
                let address = address.to_owned();
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
    };
    for entry in list.iter() {
        match entry {
            mail_parser::Address::Mailbox(mailbox) => push(mailbox),
            mail_parser::Address::Group(group) => {
                for mailbox in group.mailboxes() {
                    push(mailbox);
                }
            }
        }
    }
    addresses
}

fn bounded_draft(mut text: String) -> Draft {
    const NOTE: &str = "\n…（引用过长，已截断）";
    let limit = MAX_TEXT.saturating_sub(NOTE.len());
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(NOTE);
    }
    Draft::plain(&text).unwrap_or_else(|_| Draft::plain("").expect("empty"))
}

impl MailContent {
    /// 回复正文只用清理后的纯文本；引用行标记为 Quote，由受控模型导出。
    pub fn reply_draft(&self) -> Draft {
        let date = if self.date_text.is_empty() {
            "（未知时间）"
        } else {
            &self.date_text
        };
        let sender = if self.sender.trim().is_empty() {
            "（未知发件人）"
        } else {
            &self.sender
        };
        let attribution = format!("\n\n在 {date}，{sender} 写道：\n");
        let body_start = attribution.len();
        let text = format!("{attribution}{}", self.plain);
        let mut draft = bounded_draft(text);
        if body_start < draft.text().len() {
            let _ = draft.set_block(body_start..draft.text().len(), Block::Quote);
        }
        draft
    }

    /// 转发正文带最小头部块；原始正文同样走纯文本与 Quote，不引入原始 HTML。
    pub fn forward_draft(&self) -> Draft {
        let date = if self.date_text.is_empty() {
            "（未知时间）"
        } else {
            &self.date_text
        };
        let mut text = String::from("\n\n---------- 转发邮件 ----------\n");
        text.push_str(&format!("发件人：{}\n", self.sender));
        text.push_str(&format!("日期：{date}\n"));
        text.push_str(&format!("主题：{}\n", self.subject));
        text.push_str(&format!("收件人：{}\n\n", self.to.join(", ")));
        let body_start = text.len();
        text.push_str(&self.plain);
        let mut draft = bounded_draft(text);
        if body_start < draft.text().len() {
            let _ = draft.set_block(body_start..draft.text().len(), Block::Quote);
        }
        draft
    }
}

#[cfg(test)]
mod reply_tests {
    use super::*;

    fn parsed() -> MailContent {
        parse(
            b"From: Alice <alice@example.org>\r\n\
              To: Bob <bob@example.org>, me@example.org\r\n\
              Cc: Carol <carol@example.org>\r\n\
              Reply-To: reply@example.org\r\n\
              Subject: Original subject\r\n\
              Date: Mon, 16 Nov 2026 08:00:00 +0000\r\n\
              \r\n\
              hello\r\nworld\r\n",
        )
        .unwrap()
    }

    #[test]
    fn reply_data_uses_independent_addresses_and_clean_plain_text() {
        let content = parsed();
        assert_eq!(content.from, vec!["alice@example.org"]);
        assert_eq!(
            content.to,
            vec!["bob@example.org".to_string(), "me@example.org".to_string()]
        );
        assert_eq!(content.cc, vec!["carol@example.org"]);
        assert_eq!(content.reply_to, vec!["reply@example.org"]);
        assert!(content.date_unix > 0);
        assert!(!content.date_text.is_empty());
        let reply = content.reply_draft();
        assert!(reply.text().contains("写道"));
        assert!(reply.text().contains("hello"));
        let body_start = reply.text().find("hello").unwrap();
        let scalar = reply.text()[..body_start].chars().count();
        assert_eq!(reply.attributes()[scalar].block, Block::Quote);
        let forward = content.forward_draft();
        assert!(forward.text().contains("转发邮件"));
        assert!(forward.text().contains("Original subject"));
        assert!(forward.text().contains("hello"));
        assert!(!forward.html().contains("<script"));
    }

    #[test]
    fn reply_quote_is_truncated_instead_of_rejecting_the_draft() {
        let mut content = parsed();
        content.plain = "x".repeat(MAX_TEXT + 1024);
        let reply = content.reply_draft();
        assert!(reply.text().len() <= MAX_TEXT);
        assert!(reply.text().contains("已截断"));
    }
}
