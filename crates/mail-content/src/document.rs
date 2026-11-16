//! 与 UI 框架无关的受控阅读树。原生渲染只接受这棵树导出的 HTML。
use scraper::{ElementRef, Html, Node as HtmlNode};

use crate::{InlineImage, escape};

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Text(String),
    Element { kind: Element, children: Vec<Node> },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
        render(&self.nodes, &mut output);
        output
    }

    pub fn plain_text(&self) -> String {
        let mut output = String::new();
        text(&self.nodes, &mut output);
        output.trim().to_owned()
    }
}

fn children(element: ElementRef<'_>, images: &[InlineImage]) -> Vec<Node> {
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
        let nested = children(element, images);
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
        result.push(Node::Element {
            kind,
            children: nested,
        });
    }
    result
}

fn render(nodes: &[Node], output: &mut String) {
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
                            render(children, output);
                            output.push_str("</a>");
                        } else {
                            render(children, output);
                        }
                        continue;
                    }
                    Element::Image { alt, resource } => {
                        let state = if resource.is_some() {
                            "内嵌图片"
                        } else {
                            "图片已省略"
                        };
                        output.push_str(&format!("[{}：{}]", state, escape(alt)));
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
                render(children, output);
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
