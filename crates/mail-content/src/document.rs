//! 与 UI 框架无关的受控阅读树。原生渲染只接受这棵树导出的 HTML。
use scraper::{ElementRef, Html, Node as HtmlNode};
use serde::{Deserialize, Serialize};

use crate::{InlineImage, escape};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Element {
    Container,
    Paragraph,
    Heading(u8),
    Strong,
    Emphasis,
    Quote,
    Pre,
    Code,
    List {
        ordered: bool,
    },
    ListItem,
    Table,
    TableHead,
    TableBody,
    TableRow,
    TableCell {
        heading: bool,
    },
    Link(String),
    Image {
        alt: String,
        resource: Option<usize>,
    },
    Break,
    Rule,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Node {
    Text(String),
    Element { kind: Element, children: Vec<Node> },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub nodes: Vec<Node>,
}

impl Document {
    pub(crate) fn from_html(html: &str, images: &[InlineImage]) -> Self {
        let dom = Html::parse_fragment(html);
        Self {
            nodes: children(dom.root_element(), images),
        }
    }

    pub fn from_plain(text: &str) -> Self {
        Self {
            nodes: vec![Node::Element {
                kind: Element::Pre,
                children: vec![Node::Text(text.to_owned())],
            }],
        }
    }

    /// 内嵌图像由 UI 的受控资源入口单独渲染；这里不向 TextView 传递资源 URL。
    pub fn native_html(&self) -> String {
        let mut output = String::new();
        render(&self.nodes, &mut output, None);
        output
    }

    /// Only emits indices of validated images, never message-provided URLs or paths.
    /// The UI must install an authoritative image resolver for these private IDs.
    pub fn native_html_with_images(&self, images: &[InlineImage]) -> String {
        let mut output = String::new();
        render(&self.nodes, &mut output, Some(images));
        output
    }

    pub fn plain_text(&self) -> String {
        let mut output = String::new();
        text(&self.nodes, &mut output);
        output.trim().to_owned()
    }
}

fn children(element: ElementRef<'_>, images: &[InlineImage]) -> Vec<Node> {
    children_in(element, images, false)
}

fn children_in(element: ElementRef<'_>, images: &[InlineImage], layout: bool) -> Vec<Node> {
    let mut result = Vec::new();
    for child in element.children() {
        if let HtmlNode::Text(value) = child.value() {
            result.push(Node::Text(value.to_string()));
            continue;
        }
        let Some(element) = ElementRef::wrap(child) else {
            continue;
        };
        let value = element.value();
        let is_table = value.name() == "table";
        let layout_table = is_table
            && value.attr("role").is_some_and(|role| {
                role.eq_ignore_ascii_case("presentation") || role.eq_ignore_ascii_case("none")
            });
        let nested = children_in(
            element,
            images,
            if is_table { layout_table } else { layout },
        );
        // Explicit layout tables are containers, not bordered data grids. A nested real
        // table starts a new context so its headings/cells remain meaningful.
        if layout_table
            || (layout
                && matches!(
                    value.name(),
                    "thead" | "tbody" | "tfoot" | "tr" | "td" | "th"
                ))
        {
            if matches!(value.name(), "td" | "th") && nested.iter().any(readable) {
                result.push(Node::Element {
                    kind: Element::Container,
                    children: nested,
                });
            } else {
                result.extend(nested);
            }
            continue;
        }
        // Decorative inline wrappers must not turn a sentence into block paragraphs.
        if matches!(value.name(), "span" | "u" | "s" | "small" | "sub" | "sup") {
            result.extend(nested);
            continue;
        }
        let kind = match value.name() {
            "p" => Element::Paragraph,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                Element::Heading(value.name().as_bytes()[1] - b'0')
            }
            "strong" | "b" => Element::Strong,
            "em" | "i" => Element::Emphasis,
            "blockquote" => Element::Quote,
            "pre" => Element::Pre,
            "code" => Element::Code,
            "ul" => Element::List { ordered: false },
            "ol" => Element::List { ordered: true },
            "li" => Element::ListItem,
            "table" => Element::Table,
            "thead" => Element::TableHead,
            "tbody" | "tfoot" => Element::TableBody,
            "tr" => Element::TableRow,
            "td" | "th" => Element::TableCell {
                heading: value.name() == "th",
            },
            "a" => match value.attr("href").and_then(crate::safe_link) {
                Some(url) => Element::Link(url),
                None => {
                    result.extend(nested);
                    continue;
                }
            },
            "img" => Element::Image {
                alt: value.attr("alt").unwrap_or("内嵌图片").to_owned(),
                resource: value
                    .attr("src")
                    .and_then(|source| images.iter().position(|image| image.data_uri == source)),
            },
            "br" => Element::Break,
            "hr" => Element::Rule,
            _ => Element::Container,
        };
        // Discard spacer paragraphs used by email layouts, while preserving preformatted text.
        if matches!(
            kind,
            Element::Paragraph | Element::Container | Element::Heading(_)
        ) && !nested.iter().any(readable)
        {
            // Empty blocks between bare inline runs still separate their text.
            if result.last().is_some_and(|node| match node {
                Node::Text(value) => !value.trim().is_empty(),
                Node::Element { kind, .. } => matches!(
                    kind,
                    Element::Strong
                        | Element::Emphasis
                        | Element::Code
                        | Element::Link(_)
                        | Element::Image { .. }
                ),
            }) {
                result.push(Node::Element {
                    kind: Element::Break,
                    children: Vec::new(),
                });
            }
            continue;
        }
        result.push(Node::Element {
            kind,
            children: nested,
        });
    }
    result
}

fn readable(node: &Node) -> bool {
    match node {
        Node::Text(value) => !value.trim().is_empty(),
        Node::Element {
            kind: Element::Image { .. } | Element::Rule | Element::Pre,
            ..
        } => true,
        Node::Element { children, .. } => children.iter().any(readable),
    }
}

fn render(nodes: &[Node], output: &mut String, images: Option<&[InlineImage]>) {
    for node in nodes {
        match node {
            Node::Text(value) => output.push_str(&escape(value)),
            Node::Element { kind, children } => {
                let tag = match kind {
                    Element::Container => "div",
                    Element::Paragraph => "p",
                    Element::Heading(1) => "h1",
                    Element::Heading(2) => "h2",
                    Element::Heading(3) => "h3",
                    Element::Heading(4) => "h4",
                    Element::Heading(5) => "h5",
                    Element::Heading(_) => "h6",
                    Element::Strong => "strong",
                    Element::Emphasis => "em",
                    Element::Quote => "blockquote",
                    Element::Pre => "pre",
                    Element::Code => "code",
                    Element::List { ordered: true } => "ol",
                    Element::List { ordered: false } => "ul",
                    Element::ListItem => "li",
                    Element::Table => "table",
                    Element::TableHead => "thead",
                    Element::TableBody => "tbody",
                    Element::TableRow => "tr",
                    Element::TableCell { heading: true } => "th",
                    Element::TableCell { heading: false } => "td",
                    Element::Link(url) => {
                        if let Some(url) = crate::safe_link(url) {
                            output.push_str(&format!("<a href=\"{}\">", escape(&url)));
                            render(children, output, images);
                            output.push_str("</a>");
                        } else {
                            render(children, output, images);
                        }
                        continue;
                    }
                    Element::Image { alt, resource } => {
                        if let Some((index, image)) = resource
                            .and_then(|index| images?.get(index).map(|image| (index, image)))
                        {
                            output.push_str(&format!(
                                "<img src=\"qingmail-image://{index}\" alt=\"{}\" width=\"{}\">",
                                escape(alt),
                                image.width.min(640),
                            ));
                            continue;
                        }
                        let state = if resource.is_some() {
                            "内嵌图片"
                        } else {
                            "图片未加载"
                        };
                        if alt.trim().is_empty() {
                            output.push_str(&format!("[{state}]"));
                        } else {
                            output.push_str(&format!("[{}：{}]", state, escape(alt)));
                        }
                        continue;
                    }
                    Element::Break => {
                        output.push_str("<br>");
                        continue;
                    }
                    Element::Rule => {
                        output.push_str("<hr>");
                        continue;
                    }
                };
                output.push_str(&format!("<{tag}>"));
                render(children, output, images);
                output.push_str(&format!("</{tag}>"));
            }
        }
    }
}

fn text(nodes: &[Node], output: &mut String) {
    for node in nodes {
        match node {
            Node::Text(value) => output.push_str(value),
            Node::Element { kind, children } => {
                if let Element::Image { alt, .. } = kind {
                    output.push_str(&format!("[图片：{alt}]"));
                }
                if matches!(kind, Element::ListItem) {
                    output.push_str("• ");
                }
                text(children, output);
                if let Element::Link(url) = kind {
                    output.push_str(&format!(" ({url})"));
                }
                if matches!(kind, Element::TableCell { .. }) {
                    output.push('\t');
                }
                if !matches!(
                    kind,
                    Element::Strong
                        | Element::Emphasis
                        | Element::Code
                        | Element::Link(_)
                        | Element::Image { .. }
                        | Element::TableCell { .. }
                ) {
                    output.push('\n');
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_tables_flatten_but_nested_data_tables_keep_cells() {
        let content = crate::parse(b"Content-Type: text/html; charset=utf-8\r\n\r\n<table role='presentation'><tr><td><p>intro</p><table role='none'><tr><td>feature one</td><td>feature two<img src='https://example.invalid/hero.png' alt=''></td></tr></table><table><tr><th>item</th><th>price</th></tr><tr><td>book</td><td>12</td></tr></table></td></tr></table>").unwrap();
        let html = content.document.native_html();
        assert_eq!(html.matches("<table>").count(), 1, "{html}");
        assert!(html.contains("<div>feature one</div>"), "{html}");
        assert!(html.contains("<th>item</th><th>price</th>"), "{html}");
        assert!(html.contains("[图片未加载]"), "{html}");
        assert!(!html.contains("[图片未加载：]"));
        // Browser copy keeps the sender's table layout; only native reading is simplified.
        assert_eq!(content.browser_html().matches("<table").count(), 3);
        assert_eq!(content.remote_images.len(), 1);
    }

    #[test]
    fn newsletter_inline_wrappers_do_not_split_sentences() {
        let doc = Document::from_html(
            "<p><span>《航空学报》</span><span>创刊于</span><span><strong>1965</strong></span><span>年，</span><span>面向国内外公开发行。</span></p>",
            &[],
        );
        assert_eq!(
            doc.native_html(),
            "<p>《航空学报》创刊于<strong>1965</strong>年，面向国内外公开发行。</p>"
        );
        assert_eq!(
            doc.plain_text(),
            "《航空学报》创刊于1965年，面向国内外公开发行。"
        );
    }

    #[test]
    fn layout_spacers_are_removed_without_merging_paragraphs_or_losing_pre() {
        let doc = Document::from_html(
            "<div><p>&nbsp;</p><p><br></p><p>第一段</p><p><span>&nbsp;</span></p><p>第二段<br>同段换行</p><pre> a\n  b </pre></div>",
            &[],
        );
        assert_eq!(
            doc.native_html(),
            "<div><p>第一段</p><p>第二段<br>同段换行</p><pre> a\n  b </pre></div>"
        );
        assert_eq!(
            Document::from_html("A<p><br></p>B", &[]).native_html(),
            "A<br>B"
        );
    }

    #[test]
    fn native_images_keep_document_order_and_only_use_validated_indices() {
        let content = crate::parse(include_bytes!(
            "../../../tests/fixtures/mime/03-alternative-related.eml"
        ))
        .unwrap();
        let html = content.document.native_html_with_images(&content.images);
        let pos = html.find("<img src=\"qingmail-image://0\"").unwrap();
        assert!(html[..pos].contains("每周开发简报"));
        assert!(html[pos..].contains("阅读详情"));
        assert!(!html.contains("data:image"));
        assert!(!html.contains("src=\"cid:"));
        let forged = Document {
            nodes: vec![Node::Element {
                kind: Element::Image {
                    alt: "<script>".into(),
                    resource: Some(999),
                },
                children: vec![],
            }],
        };
        let html = forged.native_html_with_images(&content.images);
        assert!(!html.contains("<img"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
