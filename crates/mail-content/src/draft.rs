//! P0 受控富文本草稿。索引是 Unicode scalar，UI 边界使用 UTF-8 字节；不存原始 HTML。
use serde::{Deserialize, Serialize};
use std::ops::Range;

pub const MAX_TEXT: usize = 32 * 1024;
pub const MAX_JSON: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Block {
    #[default]
    Paragraph,
    Bullet,
    Quote,
    Signature,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attributes {
    pub bold: bool,
    pub italic: bool,
    pub link: Option<String>,
    pub block: Block,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    version: u8,
    text: String,
    // 每个 scalar 一个属性，最后一个哨兵保存空文档/尾段属性。P0 为易验证而未压缩。
    attributes: Vec<Attributes>,
}

fn checked_link(value: &str) -> bool {
    value.len() <= 2048
        && url::Url::parse(value).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https" | "mailto")
                && url.username().is_empty()
                && url.password().is_none()
        })
}

impl Draft {
    pub fn plain(text: &str) -> Result<Self, String> {
        if text.len() > MAX_TEXT {
            return Err("草稿超过 P0 的 32 KiB 正文上限".into());
        }
        Ok(Self {
            version: 1,
            text: text.into(),
            attributes: vec![Attributes::default(); text.chars().count() + 1],
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn attributes(&self) -> &[Attributes] {
        &self.attributes
    }

    pub fn scalar_range(&self, range: Range<usize>) -> Result<Range<usize>, String> {
        if range.start > range.end || self.text.get(range.clone()).is_none() {
            return Err("无效的 UTF-8 选区".into());
        }
        Ok(self.text[..range.start].chars().count()..self.text[..range.end].chars().count())
    }

    pub fn replace(
        &mut self,
        bytes: Range<usize>,
        value: &str,
        typing: Option<&Attributes>,
    ) -> Result<(), String> {
        let range = self.scalar_range(bytes.clone())?;
        if self.text.len() - bytes.len() + value.len() > MAX_TEXT {
            return Err("草稿超过 P0 的 32 KiB 正文上限".into());
        }
        let inherited = typing.cloned().unwrap_or_else(|| {
            self.attributes[if range.start > 0 && range.is_empty() {
                range.start - 1
            } else {
                range.start
            }]
            .clone()
        });
        self.attributes
            .splice(range, std::iter::repeat_n(inherited, value.chars().count()));
        self.text.replace_range(bytes, value);
        Ok(())
    }

    pub fn inline(
        &mut self,
        bytes: Range<usize>,
        update: impl Fn(&mut Attributes),
    ) -> Result<(), String> {
        let range = self.scalar_range(bytes)?;
        let previous = self.attributes[range.clone()].to_vec();
        for attributes in &mut self.attributes[range.clone()] {
            update(attributes);
        }
        if let Err(error) = self.validate() {
            self.attributes[range].clone_from_slice(&previous);
            return Err(error);
        }
        Ok(())
    }

    pub fn set_block(&mut self, bytes: Range<usize>, block: Block) -> Result<(), String> {
        self.scalar_range(bytes.clone())?;
        let start = self.text[..bytes.start].rfind('\n').map_or(0, |i| i + 1);
        let last = if bytes.end > bytes.start {
            bytes.end - 1
        } else {
            bytes.end
        };
        let end = self.text.as_bytes()[last..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(self.text.len(), |i| last + i);
        let range = self.scalar_range(start..end)?;
        for attr in &mut self.attributes[range.start..=range.end] {
            attr.block = block;
        }
        Ok(())
    }

    pub fn set_link(&mut self, bytes: Range<usize>, link: Option<&str>) -> Result<(), String> {
        if link.is_some_and(|link| !checked_link(link)) {
            return Err("链接仅允许无凭据的 HTTP(S) 或 mailto 地址".into());
        }
        self.inline(bytes, |attr| attr.link = link.map(str::to_owned))
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.text.len() > MAX_TEXT
            || self.attributes.len() != self.text.chars().count() + 1
            || self
                .attributes
                .iter()
                .any(|a| a.link.as_deref().is_some_and(|l| !checked_link(l)))
        {
            return Err("草稿版本、长度、属性或链接无效".into());
        }
        Ok(())
    }

    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_JSON {
            return Err("草稿序列化超限".into());
        }
        Ok(bytes)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_JSON {
            return Err("草稿文件超限".into());
        }
        let draft: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        draft.validate()?;
        Ok(draft)
    }

    /// 输出来自枚举/属性的允许列表；粘贴的 HTML 只会被视为普通文本。
    pub fn html(&self) -> String {
        let mut html = String::new();
        let mut scalar = 0;
        for paragraph in self.text.split('\n') {
            let block = self.attributes[scalar].block;
            let (open, close) = match block {
                Block::Paragraph => ("<p>", "</p>"),
                Block::Bullet => ("<ul><li>", "</li></ul>"),
                Block::Quote => ("<blockquote><p>", "</p></blockquote>"),
                Block::Signature => ("<div class=\"signature\"><p>", "</p></div>"),
            };
            html.push_str(open);
            if paragraph.is_empty() {
                html.push_str("<br>");
            }
            let mut run = String::new();
            let mut previous: Option<&Attributes> = None;
            for ch in paragraph.chars() {
                let attr = &self.attributes[scalar];
                if previous.is_some_and(|old| old != attr) {
                    append_run(&mut html, &run, previous.unwrap());
                    run.clear();
                }
                previous = Some(attr);
                run.push(ch);
                scalar += 1;
            }
            if let Some(attr) = previous {
                append_run(&mut html, &run, attr);
            }
            html.push_str(close);
            scalar += 1;
        }
        html
    }

    pub fn plain_export(&self) -> String {
        let mut scalar = 0;
        self.text
            .split('\n')
            .map(|line| {
                let prefix = match self.attributes[scalar].block {
                    Block::Bullet => "• ",
                    Block::Quote => "> ",
                    Block::Signature => "-- \n",
                    _ => "",
                };
                scalar += line.chars().count() + 1;
                format!("{prefix}{line}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 把另一份草稿插入当前字节选区；用于清理后的 HTML 粘贴，属性随文本一起搬运。
    pub fn splice(&mut self, bytes: Range<usize>, other: &Draft) -> Result<(), String> {
        let range = self.scalar_range(bytes.clone())?;
        let inserted_chars = other.text.chars().count();
        if self.text.len() - bytes.len() + other.text.len() > MAX_TEXT {
            return Err("草稿超过正文上限".into());
        }
        self.attributes
            .splice(range, other.attributes.iter().take(inserted_chars).cloned());
        self.text.replace_range(bytes, &other.text);
        self.validate()
    }

    /// 清理后的基础 HTML 粘贴：只接受受控标签，导出为段落/强调/链接/列表/引用。
    pub fn from_html(source: &str) -> Result<Self, String> {
        if source.trim().is_empty() {
            return Err("HTML 为空".into());
        }
        let sanitized =
            crate::sanitize::sanitize(source, &[]).map_err(|error| error.to_string())?;
        let mut builder = HtmlDraft::default();
        builder.nodes(&sanitized.document.nodes, &HtmlStyle::default());
        builder.finish()
    }
}

fn append_run(html: &mut String, text: &str, attr: &Attributes) {
    if let Some(link) = &attr.link {
        html.push_str("<a href=\"");
        html.push_str(&escape(link));
        html.push_str("\">");
    }
    if attr.bold {
        html.push_str("<strong>");
    }
    if attr.italic {
        html.push_str("<em>");
    }
    html.push_str(&escape(text));
    if attr.italic {
        html.push_str("</em>");
    }
    if attr.bold {
        html.push_str("</strong>");
    }
    if attr.link.is_some() {
        html.push_str("</a>");
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Default)]
struct HtmlDraft {
    text: String,
    attributes: Vec<Attributes>,
    overflow: bool,
}
#[derive(Clone, Default)]
struct HtmlStyle {
    bold: bool,
    italic: bool,
    link: Option<String>,
    block: Block,
}
impl HtmlDraft {
    fn push(&mut self, value: &str, style: &HtmlStyle) {
        for ch in value.chars() {
            if self.text.len() + ch.len_utf8() > MAX_TEXT {
                self.overflow = true;
                return;
            }
            self.text.push(ch);
            self.attributes.push(Attributes {
                bold: style.bold,
                italic: style.italic,
                link: style.link.clone(),
                block: style.block,
            });
        }
    }
    fn newline(&mut self, style: &HtmlStyle) {
        if !self.text.ends_with('\n') {
            self.push("\n", style);
        }
    }
    fn ensure_line(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.push("\n", &HtmlStyle::default());
        }
    }
    fn nodes(&mut self, nodes: &[crate::Node], style: &HtmlStyle) {
        for node in nodes {
            match node {
                crate::Node::Text(value) => self.push(value, style),
                crate::Node::Element { kind, children } => match kind {
                    crate::Element::Container => self.nodes(children, style),
                    crate::Element::Paragraph => {
                        self.ensure_line();
                        self.nodes(children, style);
                        self.newline(style);
                    }
                    crate::Element::Heading(_) => {
                        self.ensure_line();
                        let mut heading = style.clone();
                        heading.bold = true;
                        self.nodes(children, &heading);
                        self.newline(style);
                    }
                    crate::Element::Strong => {
                        let mut strong = style.clone();
                        strong.bold = true;
                        self.nodes(children, &strong);
                    }
                    crate::Element::Emphasis => {
                        let mut emphasis = style.clone();
                        emphasis.italic = true;
                        self.nodes(children, &emphasis);
                    }
                    crate::Element::Quote => {
                        self.ensure_line();
                        let mut quote = style.clone();
                        quote.block = Block::Quote;
                        self.nodes(children, &quote);
                        self.newline(&quote);
                    }
                    crate::Element::List { .. } => {
                        self.ensure_line();
                        self.nodes(children, style);
                    }
                    crate::Element::ListItem => {
                        self.ensure_line();
                        let mut item = style.clone();
                        item.block = Block::Bullet;
                        self.nodes(children, &item);
                        self.newline(&item);
                    }
                    crate::Element::Link(url) => {
                        let mut link = style.clone();
                        if link.link.is_none() {
                            link.link = crate::safe_link(url);
                        }
                        self.nodes(children, &link);
                    }
                    crate::Element::Break => self.newline(style),
                    crate::Element::Rule => {
                        self.ensure_line();
                        self.push("————", style);
                        self.newline(style);
                    }
                    crate::Element::Pre | crate::Element::Code => self.nodes(children, style),
                    crate::Element::Image { alt, .. } => {
                        if !alt.is_empty() {
                            self.push(alt, style);
                        }
                    }
                    crate::Element::Table => self.nodes(children, style),
                    crate::Element::TableHead | crate::Element::TableBody => {
                        self.nodes(children, style)
                    }
                    crate::Element::TableRow => {
                        self.ensure_line();
                        self.nodes(children, style);
                        self.newline(style);
                    }
                    crate::Element::TableCell { .. } => {
                        self.nodes(children, style);
                        self.push(" ", style);
                    }
                },
            }
        }
    }
    fn finish(mut self) -> Result<Draft, String> {
        if self.overflow {
            return Err("HTML 粘贴超过正文上限".into());
        }
        self.attributes.push(Attributes::default());
        let draft = Draft {
            version: 1,
            text: self.text,
            attributes: self.attributes,
        };
        draft.validate()?;
        Ok(draft)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_edits_preserve_formats_and_roundtrip() {
        let mut d = Draft::plain("中文🙂\n第二段").unwrap();
        d.inline(0..6, |a| a.bold = true).unwrap();
        d.replace(3..6, "国", None).unwrap();
        d.set_block(11..11, Block::Quote).unwrap();
        assert_eq!(d.text(), "中国🙂\n第二段");
        assert!(d.attributes()[1].bold);
        assert!(d.replace(1..2, "x", None).is_err());
        assert_eq!(Draft::from_json(&d.to_json().unwrap()).unwrap(), d);
        assert!(d.html().contains("<blockquote>"));
        d.replace(0..10, "替换", Some(&Attributes::default()))
            .unwrap();
        assert!(!d.attributes()[0].bold);
    }

    #[test]
    fn export_escapes_and_rejects_active_links_and_corrupt_drafts() {
        let mut d = Draft::plain("<script>\n条目").unwrap();
        assert!(d.set_link(0..8, Some("javascript:alert(1)")).is_err());
        assert!(
            d.set_link(0..8, Some("https://user:pass@example.org/"))
                .is_err()
        );
        d.set_link(0..8, Some("https://example.org/?x=1&y=2"))
            .unwrap();
        d.set_block(9..9, Block::Bullet).unwrap();
        assert!(!d.html().contains("<script>"));
        assert!(d.html().contains("&amp;y"));
        assert!(d.plain_export().contains("• 条目"));
        let json = String::from_utf8(d.to_json().unwrap()).unwrap();
        assert!(
            Draft::from_json(json.replace("\"version\":1", "\"version\":2").as_bytes()).is_err()
        );
        assert!(Draft::from_json(b"{}").is_err());
        assert!(Draft::plain(&"x".repeat(MAX_TEXT + 1)).is_err());
    }
}

#[cfg(test)]
mod html_tests {
    use super::*;

    #[test]
    fn html_paste_is_cleaned_and_keeps_basic_attributes() {
        let draft = Draft::from_html(
            "<p>你好 <b>世界</b> <a href=\"https://example.org/?a=1&amp;b=2\">链接</a></p>\
             <script>alert(1)</script><ul><li>条目</li></ul>",
        )
        .unwrap();
        assert!(draft.text().contains("你好 世界 链接"));
        assert!(!draft.text().contains("alert"));
        let scalar =
            |text: &str, needle: &str| draft.text()[..text.find(needle).unwrap()].chars().count();
        let world = scalar(draft.text(), "世界");
        assert!(draft.attributes()[world].bold);
        let link = scalar(draft.text(), "链接");
        assert_eq!(
            draft.attributes()[link].link.as_deref(),
            Some("https://example.org/?a=1&b=2")
        );
        let bullet = scalar(draft.text(), "条目");
        assert_eq!(draft.attributes()[bullet].block, Block::Bullet);
        assert!(
            Draft::from_html("<script>alert(1)</script>")
                .unwrap()
                .text()
                .is_empty()
        );
        assert!(Draft::from_html("   ").is_err());
    }

    #[test]
    fn splice_inserts_text_and_attributes_at_a_byte_range() {
        let mut base = Draft::plain("A C").unwrap();
        let insertion = Draft::from_html("<b>B</b>").unwrap();
        base.splice(2..2, &insertion).unwrap();
        // 字节 2 位于空格与 C 之间：插入结果紧邻 C。
        assert_eq!(base.text(), "A BC");
        let index = base.text()[..base.text().find('B').unwrap()]
            .chars()
            .count();
        assert!(base.attributes()[index].bold);
        assert!(!base.attributes()[0].bold);
        // 越界或超限必须拒绝，保留原正文。
        let before = base.text().to_owned();
        assert!(base.splice(0..99, &insertion).is_err());
        assert_eq!(base.text(), before);
    }
}
