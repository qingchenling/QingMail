use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use ammonia::{Builder, UrlRelative};
use scraper::{Html, Selector};

use crate::{Document, InlineImage, css, escape};

const TAGS: &[&str] = &[
    "div",
    "section",
    "article",
    "p",
    "span",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "s",
    "br",
    "hr",
    "pre",
    "code",
    "blockquote",
    "ul",
    "ol",
    "li",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "th",
    "td",
    "caption",
    "a",
    "img",
    "small",
    "sub",
    "sup",
    "center",
];

pub(crate) struct Sanitized {
    pub body: String,
    pub styles: String,
    pub document: Document,
    pub remote_images: Vec<String>,
}

pub(crate) fn sanitize(source: &str, images: &[InlineImage]) -> Result<Sanitized, &'static str> {
    if source.len() > crate::MAX_BODY_BYTES {
        return Err("HTML 正文超出 512 KiB，改用受限纯文本");
    }
    let dom = Html::parse_document(source);
    if dom.tree.nodes().count() > 20_000 {
        return Err("HTML 节点超过 20000，改用纯文本");
    }
    for node in dom.tree.nodes() {
        if node.ancestors().take(66).count() > 64 {
            return Err("HTML 嵌套超过 64 层，改用纯文本");
        }
    }
    let mut styles = String::new();
    for element in dom.select(&Selector::parse("style").unwrap()).take(32) {
        let source = element.text().collect::<String>();
        styles.push_str(&css::stylesheet(&source));
        if styles.len() > 64 * 1024 {
            return Err("样式超出限制，改用纯文本");
        }
    }
    // 邮件 body 本身的样式应用到受控容器；原始 html/body 标签不会进入副本正文。
    let body_style = dom
        .select(&Selector::parse("body").unwrap())
        .next()
        .and_then(|body| body.value().attr("style"))
        .map(css::declarations)
        .unwrap_or_default();
    let remote_images = dom
        .select(&Selector::parse("img[src]").unwrap())
        .filter_map(|image| crate::http_url(image.value().attr("src")?))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(32)
        .collect();
    let sources = Arc::new(
        images
            .iter()
            .map(|image| (image.key.clone(), image.data_uri.clone()))
            .collect::<BTreeMap<_, _>>(),
    );
    let expanded = Arc::new(AtomicUsize::new(0));
    let mut builder = Builder::default();
    builder
        .tags(TAGS.iter().copied().collect::<HashSet<_>>())
        .clean_content_tags(
            [
                "script", "style", "iframe", "object", "embed", "svg", "math", "template",
                "noscript", "audio", "video", "canvas",
            ]
            .into_iter()
            .collect(),
        )
        .generic_attributes(["style", "class", "title"].into_iter().collect())
        .tag_attributes(HashMap::from([
            ("a", ["href"].into_iter().collect()),
            (
                "img",
                ["src", "alt", "width", "height"].into_iter().collect(),
            ),
            (
                "td",
                ["colspan", "rowspan", "width", "height"]
                    .into_iter()
                    .collect(),
            ),
            (
                "th",
                ["colspan", "rowspan", "width", "height"]
                    .into_iter()
                    .collect(),
            ),
            (
                "table",
                [
                    "width",
                    "height",
                    "cellpadding",
                    "cellspacing",
                    "border",
                    "role",
                ]
                .into_iter()
                .collect(),
            ),
        ]))
        .url_schemes(
            ["http", "https", "mailto", "cid", "data"]
                .into_iter()
                .collect(),
        )
        .url_relative(UrlRelative::Deny)
        .link_rel(Some("noreferrer noopener"))
        .strip_comments(true)
        .attribute_filter(move |tag, attribute, value| match (tag, attribute) {
            ("a", "href") => crate::safe_link(value).map(Cow::Owned),
            ("img", "src") => {
                let key = crate::cid_url(value).or_else(|| crate::http_url(value))?;
                let source = sources.get(&key)?;
                expanded
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                        used.checked_add(source.len())
                            .filter(|total| *total <= 8 * 1024 * 1024)
                    })
                    .ok()?;
                Some(Cow::Owned(source.clone()))
            }
            (_, "style") => Some(Cow::Owned(css::declarations(value))),
            ("table", "role") => match value.trim().to_ascii_lowercase().as_str() {
                "presentation" => Some(Cow::Borrowed("presentation")),
                "none" => Some(Cow::Borrowed("none")),
                _ => None,
            },
            (
                _,
                "width" | "height" | "cellpadding" | "cellspacing" | "border" | "colspan"
                | "rowspan",
            ) => value
                .parse::<u32>()
                .ok()
                .filter(|number| *number <= 4096)
                .map(|number| Cow::Owned(number.to_string())),
            _ => Some(Cow::Borrowed(value)),
        });
    let cleaned = builder.clean(source).to_string();
    if cleaned.len() > 10 * 1024 * 1024 {
        return Err("派生正文超出限制，改用纯文本");
    }
    let document = Document::from_html(&cleaned, images);
    let image_count = dom.select(&Selector::parse("img").unwrap()).count();
    let retained_images = Html::parse_fragment(&cleaned)
        .select(&Selector::parse("img[src]").unwrap())
        .count();
    let notice = if image_count > retained_images {
        format!(
            "<p><small>已省略 {} 张远程、缺失或未通过校验的图片。</small></p>",
            image_count - retained_images
        )
    } else {
        String::new()
    };
    Ok(Sanitized {
        body: format!(
            "<section class=\"qingmail-body\" style=\"{}\">{cleaned}</section>{notice}",
            escape(&body_style)
        ),
        styles,
        document,
        remote_images,
    })
}
