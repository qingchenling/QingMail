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
