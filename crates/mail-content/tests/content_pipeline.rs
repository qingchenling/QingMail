use mail_content::{MAX_MESSAGE_BYTES, parse};
use scraper::{Html, Selector};
use std::{fs, path::Path};

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/mime")
            .join(name),
    )
    .unwrap()
}

#[test]
fn chinese_charsets_headers_alternatives_and_attachments() {
    let raw = fixture("02-gb18030.eml");
    let content = parse(&raw).unwrap();
    assert_eq!(content.raw.as_ref(), raw);
    assert!(content.subject.contains("字符集与折行"));
    assert!(content.plain.contains("你好，旧字符集邮件"));
    let content = parse(&fixture("03-alternative-related.eml")).unwrap();
    assert!(content.is_html);
    assert!(content.plain.contains("Plain alternative"));
    assert!(content.document.plain_text().contains("每周开发简报"));
    assert_eq!(content.images.len(), 1);
    assert_eq!(
        (content.images[0].width, content.images[0].height),
        (32, 20)
    );
    assert!(
        content
            .attachments
            .iter()
            .any(|attachment| attachment.name == "../../测试.txt")
    );
    let html = content.browser_html();
    assert!(html.contains("data:image/png;base64,"));
    assert!(html.contains("background-color:#f2f6ff"));
    assert!(html.contains(".qingmail-body .card"));
    assert!(html.contains("colspan=\"2\""));
    assert!(!content.document.native_html().contains("<img"));
}

#[test]
fn malicious_content_is_removed_independently_of_csp() {
    let content = parse(&fixture("04-adversarial.eml")).unwrap();
    assert_eq!(content.images.len(), 1);
    assert!(
        content
            .remote_images
            .iter()
            .any(|url| url.ends_with("/redirect"))
    );
    let output = content.browser_html();
    let dom = Html::parse_document(&output);
    for tag in [
        "script", "iframe", "object", "embed", "video", "audio", "svg", "form", "input", "base",
        "link",
    ] {
        assert_eq!(
            dom.select(&Selector::parse(tag).unwrap()).count(),
            0,
            "{tag}"
        );
    }
    assert_eq!(
        dom.select(&Selector::parse("meta[http-equiv=refresh]").unwrap())
            .count(),
        0
    );
    let anchors = dom
        .select(&Selector::parse("a[href]").unwrap())
        .map(|node| node.value().attr("href").unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        anchors,
        [
            "https://example.invalid/safe",
            "mailto:demo@example.invalid"
        ]
    );
    for image in dom.select(&Selector::parse("img[src]").unwrap()) {
        assert!(
            image
                .value()
                .attr("src")
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }
    for node in dom.select(&Selector::parse("*").unwrap()) {
        assert!(
            node.value()
                .attrs()
                .all(|(name, _)| !name.starts_with("on") && name != "ping" && name != "srcset")
        );
    }
    assert!(output.contains("安全探针正文仍然可读"));
    assert!(output.contains("表单说明保留"));
    assert!(!output.contains("http://127.0.0.1:18765"));
    assert!(!content.document.native_html().contains("style="));
}

#[test]
fn cid_is_unique_current_message_only_and_validated() {
    let duplicate = parse(&fixture("05-duplicate-missing-cid.eml")).unwrap();
    assert!(duplicate.images.is_empty());
    assert!(
        duplicate
            .warnings
            .iter()
            .any(|warning| warning.contains("重复"))
    );
    assert!(!duplicate.browser_html().contains("data:image"));
    let nested = parse(&fixture("10-nested-message.eml")).unwrap();
    assert!(nested.images.is_empty());
    assert!(!nested.browser_html().contains("data:image"));
    let other =
        parse(b"Content-Type: text/html\r\n\r\n<img src='cid:logo@demo'><p>separate</p>").unwrap();
    assert!(other.images.is_empty());
    assert!(!other.browser_html().contains("data:image"));
}

#[test]
fn damaged_html_only_deep_and_oversized_inputs_keep_readable_fallbacks() {
    for name in [
        "01-plain-utf8.eml",
        "06-broken.eml",
        "07-html-only-long.eml",
        "08-deep.eml",
        "09-large.eml",
    ] {
        let content = parse(&fixture(name)).unwrap();
        assert!(!content.plain.trim().is_empty(), "{name}");
        assert!(!content.document.plain_text().trim().is_empty(), "{name}");
        if matches!(name, "06-broken.eml" | "08-deep.eml" | "09-large.eml") {
            assert!(!content.warnings.is_empty(), "{name}");
        }
    }
    assert!(parse(&vec![b'a'; MAX_MESSAGE_BYTES + 1]).is_err());
    let empty = parse(b"Subject: empty\r\n\r\n").unwrap();
    assert!(empty.plain.contains("没有可读取"));
}

#[test]
fn consent_does_not_enable_local_network_resource_fetching() {
    let mut content = parse(&fixture("04-adversarial.eml")).unwrap();
    assert_eq!(content.load_remote_images_after_consent().unwrap(), 0);
    assert_eq!(content.images.len(), 1);
    assert!(!content.browser_html().contains("http://127.0.0.1"));
}
