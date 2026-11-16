//! 回复/全部回复/转发的收件人、主题与草稿文档；引用只使用清理后的纯文本。
use mail_content::MailContent;
use mail_core::DraftHeaders;
use mail_storage::DraftDocument;
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyMode {
    Reply,
    ReplyAll,
    Forward,
}

pub(crate) fn document(content: &MailContent, own: &str, mode: ReplyMode) -> DraftDocument {
    DraftDocument {
        headers: headers(content, own, mode),
        body: match mode {
            ReplyMode::Forward => content.forward_draft(),
            ReplyMode::Reply | ReplyMode::ReplyAll => content.reply_draft(),
        },
        attachments: vec![],
    }
}

pub(crate) fn headers(content: &MailContent, own: &str, mode: ReplyMode) -> DraftHeaders {
    let mut to = Vec::new();
    let mut cc = Vec::new();
    let mut seen = HashSet::new();
    if mode != ReplyMode::Forward {
        // Reply-To 优先于 From；两者都是解析出的独立地址。
        let primary = if content.reply_to.is_empty() {
            &content.from
        } else {
            &content.reply_to
        };
        push(&mut to, primary, own, &mut seen);
        if mode == ReplyMode::ReplyAll {
            push(&mut to, &content.to, own, &mut seen);
            push(&mut cc, &content.cc, own, &mut seen);
        }
    }
    DraftHeaders {
        subject: subject(content, mode),
        to,
        cc,
        bcc: vec![],
    }
}

fn push(target: &mut Vec<String>, source: &[String], own: &str, seen: &mut HashSet<String>) {
    let own = own.trim().to_ascii_lowercase();
    for address in source {
        let address = address.trim();
        let key = address.to_ascii_lowercase();
        if key.is_empty() || key == own {
            continue;
        }
        if seen.insert(key) {
            target.push(address.to_owned());
        }
    }
}

fn subject(content: &MailContent, mode: ReplyMode) -> String {
    let base = content.subject.trim();
    let (prefix, known): (&str, &[&str]) = match mode {
        ReplyMode::Forward => ("Fwd:", &["fwd:", "fw:", "转发:"]),
        ReplyMode::Reply | ReplyMode::ReplyAll => ("Re:", &["re:"]),
    };
    if known.iter().any(|known| {
        base.get(..known.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(known))
    }) {
        return base.to_owned();
    }
    if base.is_empty() {
        prefix.into()
    } else {
        format!("{prefix} {base}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content() -> MailContent {
        mail_content::parse(
            b"From: Alice <alice@example.org>\r\n\
              To: Bob <bob@example.org>, Me <me@example.org>\r\n\
              Cc: Carol <carol@example.org>, Me <me@example.org>\r\n\
              Reply-To: reply@example.org\r\n\
              Subject: Original\r\n\
              Date: Mon, 16 Nov 2026 08:00:00 +0000\r\n\
              \r\n\
              body\r\n",
        )
        .unwrap()
    }

    #[test]
    fn reply_prefers_reply_to_and_excludes_own_address() {
        let built = headers(&content(), "me@example.org", ReplyMode::Reply);
        assert_eq!(built.to, vec!["reply@example.org"]);
        assert!(built.cc.is_empty());
        assert_eq!(built.subject, "Re: Original");
    }

    #[test]
    fn reply_all_merges_to_and_cc_without_duplicates() {
        let built = headers(&content(), "me@example.org", ReplyMode::ReplyAll);
        assert_eq!(
            built.to,
            vec![
                "reply@example.org".to_string(),
                "bob@example.org".to_string()
            ]
        );
        assert_eq!(built.cc, vec!["carol@example.org"]);
        assert!(!built.to.contains(&"me@example.org".to_string()));
        assert!(!built.cc.contains(&"me@example.org".to_string()));
    }

    #[test]
    fn forward_has_no_recipients_and_does_not_double_prefix() {
        let built = headers(&content(), "me@example.org", ReplyMode::Forward);
        assert!(built.to.is_empty() && built.cc.is_empty());
        assert_eq!(built.subject, "Fwd: Original");
        let mut already = content();
        already.subject = "FWD: Original".into();
        assert_eq!(
            headers(&already, "me@example.org", ReplyMode::Forward).subject,
            "FWD: Original"
        );
        let ordinary = ReplyMode::Reply;
        let mut re = content();
        re.subject = "RE: Original".into();
        assert_eq!(
            headers(&re, "me@example.org", ordinary).subject,
            "RE: Original"
        );
    }
}
