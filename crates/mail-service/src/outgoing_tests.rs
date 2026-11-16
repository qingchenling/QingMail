use super::*;
use crate::accounts::tests::{acceptor, certificate, line, listener, server, socket};
use mail_core::{AttachmentId, AttachmentInfo, DraftHeaders, OutgoingAttachment};
use std::{
    io::{BufReader, Write},
    thread,
    time::{Duration, Instant},
};
fn assert_closed(reader: &mut impl std::io::BufRead) {
    let mut trailing = String::new();
    if let Err(e) = reader.read_line(&mut trailing) {
        assert!(
            !matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ),
            "client must close rejected transaction"
        );
    }
    assert!(
        trailing.is_empty(),
        "must close without further commands: {trailing:?}"
    );
}
fn snapshot() -> OutgoingSnapshot {
    OutgoingSnapshot {
        message_id: "<0123456789abcdef@qingmail.invalid>".into(),
        date_unix: 1794816000,
        sender: "sender@example.invalid".into(),
        headers: DraftHeaders {
            subject: "中文主题🙂".into(),
            to: vec!["to@example.invalid".into()],
            cc: vec!["cc@example.invalid".into()],
            bcc: vec!["hidden@example.invalid".into()],
        },
        plain: "你好🙂\n.dot\n.\n最后一行".into(),
        html: "<p><strong>你好🙂</strong></p>".into(),
        attachments: vec![OutgoingAttachment {
            info: AttachmentInfo {
                id: AttachmentId("fixture".into()),
                name: "中文附件🙂.bin".into(),
                media_type: "application/octet-stream".into(),
                size: 5,
            },
            bytes: vec![0, 13, 10, 255, 42],
        }],
    }
}
#[test]
fn mime_roundtrip_keeps_headers_bodies_attachments_but_never_bcc() {
    let source = snapshot();
    let prepared = prepare(&source, false).unwrap();
    assert_eq!(
        prepared.raw,
        prepare(&source, false).unwrap().raw,
        "same snapshot must produce same copy bytes"
    );
    assert_eq!(prepared.envelope.to().len(), 3);
    let wire = String::from_utf8(prepared.raw.clone()).unwrap();
    assert!(!wire.to_ascii_lowercase().contains("bcc:"));
    assert!(!wire.contains("hidden@example.invalid"));
    let parsed = mail_parser::MessageParser::default()
        .parse(&prepared.raw)
        .unwrap();
    assert_eq!(parsed.subject(), Some("中文主题🙂"));
    assert_eq!(
        parsed.message_id(),
        Some("0123456789abcdef@qingmail.invalid")
    );
    assert!(parsed.body_text(0).unwrap().contains("你好🙂"));
    assert!(
        parsed
            .body_html(0)
            .unwrap()
            .contains("<strong>你好🙂</strong>")
    );
    let attachment = parsed.attachments().next().unwrap();
    assert_eq!(attachment.decoded().as_ref(), source.attachments[0].bytes);
    assert_eq!(attachment.attachment_name(), Some("中文附件🙂.bin"));
    assert_eq!(parsed.date().unwrap().to_timestamp(), source.date_unix);
    assert!(prepared.raw.split(|b| *b == b'\n').all(|l| l.len() <= 999));
}
#[test]
fn preflight_rejects_injection_invalid_addresses_metadata_and_oversize() {
    let mut s = snapshot();
    s.headers.to[0] = "bad\r\nBcc: leak@example.invalid".into();
    assert_eq!(
        prepare(&s, false).err(),
        Some(PrepareError::Recipient {
            field: "收件人",
            index: 0
        })
    );
    s = snapshot();
    s.headers.cc[0] = "partial@".into();
    assert_eq!(
        prepare(&s, false).err(),
        Some(PrepareError::Recipient {
            field: "抄送",
            index: 0
        })
    );
    s = snapshot();
    s.headers.bcc[0] = "中文@example.invalid".into();
    assert!(matches!(
        prepare(&s, false).err(),
        Some(PrepareError::Recipient {
            field: "密送", ..
        })
    ));
    s = snapshot();
    s.headers.subject = "\r\nX-Evil: yes".into();
    assert_eq!(prepare(&s, false).err(), Some(PrepareError::Subject));
    s = snapshot();
    s.headers.subject = "  ".into();
    assert_eq!(
        prepare(&s, false).err(),
        Some(PrepareError::EmptySubjectNeedsConfirmation)
    );
    assert!(prepare(&s, true).is_ok());
    s = snapshot();
    s.headers.to.clear();
    s.headers.cc.clear();
    assert!(
        prepare(&s, false).is_ok(),
        "Bcc-only must use explicit envelope"
    );
    s.headers.bcc.clear();
    assert_eq!(prepare(&s, false).err(), Some(PrepareError::NoRecipients));
    s = snapshot();
    s.sender = "Name <sender@example.invalid>".into();
    assert_eq!(prepare(&s, false).err(), Some(PrepareError::Sender));
    s = snapshot();
    s.message_id = "<id@invalid>\r\nInjected: x".into();
    assert_eq!(prepare(&s, false).err(), Some(PrepareError::Metadata));
    s = snapshot();
    s.date_unix = i64::MAX;
    assert_eq!(prepare(&s, false).err(), Some(PrepareError::Metadata));
    s = snapshot();
    s.attachments[0].info.size = 6;
    assert_eq!(
        prepare(&s, false).err(),
        Some(PrepareError::Attachment { index: 0 })
    );
    s = snapshot();
    s.attachments[0].info.name = "../outside".into();
    assert!(prepare(&s, false).is_err());
    s = snapshot();
    s.attachments.push(s.attachments[0].clone());
    assert!(prepare(&s, false).is_err());
    s = snapshot();
    s.headers.to = vec!["to@example.invalid".into(); 101];
    assert_eq!(prepare(&s, false).err(), Some(PrepareError::MessageLimit));
    s = snapshot();
    s.plain = "x".repeat(2 * 1024 * 1024 + 1);
    assert_eq!(prepare(&s, false).err(), Some(PrepareError::BodyLimit));
}
#[derive(Clone, Copy, Debug)]
enum Case {
    Accept,
    RejectRecipient,
    RejectData,
    RejectFinal,
    DisconnectFinal,
    UnexpectedFinal,
    Authentication,
}
fn smtp_case(security: ConnectionSecurity, case: Case) -> Delivery {
    smtp_case_message(security, case, prepare(&snapshot(), false).unwrap())
}
fn smtp_case_message(
    security: ConnectionSecurity,
    case: Case,
    message: PreparedMessage,
) -> Delivery {
    use lettre::transport::smtp::client::Certificate;
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, security);
    let params = TlsParameters::builder("localhost".into())
        .add_root_certificate(Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let expected = message.raw.clone();
    let worker = thread::spawn(move || {
        let mut socket = socket(listener);
        if security == ConnectionSecurity::StartTls {
            socket.write_all(b"220 synthetic\r\n").unwrap();
            let mut r = BufReader::new(socket);
            assert!(line(&mut r).starts_with("EHLO "));
            r.get_mut()
                .write_all(b"250-localhost\r\n250 STARTTLS\r\n")
                .unwrap();
            assert_eq!(line(&mut r), "STARTTLS\r\n");
            r.get_mut().write_all(b"220 upgrade\r\n").unwrap();
            socket = r.into_inner();
        }
        let mut r = BufReader::new(accept.accept(socket).unwrap());
        if security == ConnectionSecurity::Tls {
            r.get_mut().write_all(b"220 synthetic\r\n").unwrap();
        }
        assert!(line(&mut r).starts_with("EHLO "));
        r.get_mut()
            .write_all(b"250-localhost\r\n250 AUTH PLAIN\r\n")
            .unwrap();
        assert!(line(&mut r).starts_with("AUTH PLAIN "));
        if matches!(case, Case::Authentication) {
            r.get_mut()
                .write_all(b"535 secret-server-text\r\n")
                .unwrap();
            assert_closed(&mut r);
            return;
        }
        r.get_mut().write_all(b"235 authenticated\r\n").unwrap();
        assert_eq!(line(&mut r), "MAIL FROM:<sender@example.invalid>\r\n");
        r.get_mut().write_all(b"250 ok\r\n").unwrap();
        for (index, address) in [
            "to@example.invalid",
            "cc@example.invalid",
            "hidden@example.invalid",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(line(&mut r), format!("RCPT TO:<{address}>\r\n"));
            if matches!(case, Case::RejectRecipient) && index == 1 {
                r.get_mut().write_all(b"550 no recipient\r\n").unwrap();
                assert_closed(&mut r);
                return;
            }
            r.get_mut().write_all(b"250 ok\r\n").unwrap();
        }
        assert_eq!(line(&mut r), "DATA\r\n");
        if matches!(case, Case::RejectData) {
            r.get_mut().write_all(b"554 no data\r\n").unwrap();
            assert_closed(&mut r);
            return;
        }
        r.get_mut().write_all(b"354 continue\r\n").unwrap();
        let mut data = vec![];
        loop {
            let value = line(&mut r);
            assert!(!value.is_empty());
            if value == ".\r\n" {
                break;
            }
            data.extend_from_slice(
                value
                    .strip_prefix('.')
                    .filter(|_| value.starts_with(".."))
                    .unwrap_or(&value)
                    .as_bytes(),
            );
        }
        // 还原SMTP dot-stuffing和终止符后必须与用于副本保存的MIME逐字节一致。
        assert_eq!(data, expected);
        assert!(
            !String::from_utf8(data)
                .unwrap()
                .contains("hidden@example.invalid")
        );
        if matches!(case, Case::DisconnectFinal) {
            return;
        }
        let reply = match case {
            Case::RejectFinal => b"554 rejected\r\n".as_slice(),
            Case::UnexpectedFinal => b"354 unexpected\r\n".as_slice(),
            _ => b"250 accepted\r\n".as_slice(),
        };
        r.get_mut().write_all(reply).unwrap();
        assert_eq!(line(&mut r), "QUIT\r\n");
        // 故意不返回221：QUIT失败不能把已接受变回失败/重试。
    });
    let delivery = deliver_using(
        &settings,
        "synthetic-password",
        message,
        &params,
        Instant::now() + Duration::from_secs(30),
    );
    worker.join().unwrap();
    delivery
}
#[test]
fn tls_and_starttls_submit_once_and_ignore_quit_failure_after_acceptance() {
    for security in [ConnectionSecurity::Tls, ConnectionSecurity::StartTls] {
        let result = smtp_case(security, Case::Accept);
        assert!(matches!(result.outcome, SmtpOutcome::Accepted));
        assert_eq!(result.detail, DeliveryDetail::Accepted);
    }
}
#[test]
fn rejection_and_uncertain_results_are_distinct_and_never_retried() {
    for case in [
        Case::RejectRecipient,
        Case::RejectData,
        Case::RejectFinal,
        Case::Authentication,
    ] {
        assert!(matches!(
            smtp_case(ConnectionSecurity::Tls, case).outcome,
            SmtpOutcome::DefinitelyRejected
        ));
    }
    for case in [Case::DisconnectFinal, Case::UnexpectedFinal] {
        let result = smtp_case(ConnectionSecurity::Tls, case);
        assert!(matches!(result.outcome, SmtpOutcome::Uncertain));
        assert_eq!(result.detail, DeliveryDetail::ResultUnknown);
    }
}
#[test]
fn missing_starttls_never_authenticates_or_submits() {
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::StartTls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(socket(listener));
        r.get_mut().write_all(b"220 synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("EHLO "));
        r.get_mut()
            .write_all(b"250-localhost\r\n250 AUTH PLAIN\r\n")
            .unwrap();
        assert_closed(&mut r);
    });
    let result = deliver(
        &settings,
        "synthetic-password",
        prepare(&snapshot(), false).unwrap(),
    );
    assert!(matches!(result.outcome, SmtpOutcome::DefinitelyRejected));
    assert_eq!(result.detail, DeliveryDetail::Tls);
    worker.join().unwrap();
}

#[test]
fn persisted_attempt_maps_results_and_restart_never_reopens_smtp_permission() {
    use mail_content::draft::Draft;
    use mail_core::{AccountSettings, SendState};
    use mail_storage::{DraftDocument, Store};
    for (case, expected) in [
        (Case::Accept, SendState::CopyPending),
        (Case::RejectFinal, SendState::Failed),
        (Case::DisconnectFinal, SendState::Uncertain),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut store = Store::open(&path).unwrap();
        let server = ServerSettings {
            host: "example.invalid".into(),
            port: 993,
            username: "synthetic".into(),
            security: ConnectionSecurity::Tls,
        };
        let account = store
            .create_account(&AccountSettings {
                display_name: "synthetic".into(),
                address: "sender@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        let source = snapshot();
        let draft = store
            .create_draft(
                &account,
                &DraftDocument {
                    headers: source.headers,
                    body: Draft::plain("hello").unwrap(),
                    attachments: vec![],
                },
            )
            .unwrap();
        let task = store
            .enqueue_draft(&account, &draft.id, draft.revision)
            .unwrap();
        let pending = store.task_snapshot(&account, &task).unwrap();
        // 校验先完成；之后从持久化Sending许可读取同一冻结快照。
        let validated = prepare(&pending, false).unwrap();
        let attempt = store.begin_smtp(&account, &task).unwrap();
        assert_eq!(
            validated.raw(),
            prepare(attempt.snapshot(), false).unwrap().raw()
        );
        assert!(store.begin_smtp(&account, &task).is_err());
        let delivery = smtp_case_message(ConnectionSecurity::Tls, case, validated);
        store.finish_smtp(attempt, delivery.outcome).unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.task(&account, &task).unwrap().state, expected);
        assert!(store.begin_smtp(&account, &task).is_err());
        if expected == SendState::CopyPending {
            assert!(store.begin_copy(&account, &task).is_ok());
        }
    }
}

#[test]
fn untrusted_certificate_prevents_authentication_and_message_commands() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        // 验证失败应在SMTP认证前关闭TLS握手。
        if let Ok(mut tls) = accept.accept(socket(listener)) {
            let _ = tls.write_all(b"220 synthetic\r\n");
            assert_closed(&mut BufReader::new(tls));
        }
    });
    let delivery = deliver(
        &settings,
        "synthetic-password",
        prepare(&snapshot(), false).unwrap(),
    );
    assert!(matches!(delivery.outcome, SmtpOutcome::DefinitelyRejected));
    worker.join().unwrap();
}

#[test]
fn smtp_total_deadline_stops_before_any_message_bytes() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        // 接受 TCP/TLS 后不发送问候；客户端必须在总期限内放弃，而不是无限等待。
        let _stream = accept.accept(socket(listener));
        thread::sleep(Duration::from_millis(1_500));
    });
    use lettre::transport::smtp::client::Certificate;
    let params = TlsParameters::builder("localhost".into())
        .add_root_certificate(Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let delivery = deliver_using(
        &settings,
        "synthetic-password",
        prepare(&snapshot(), false).unwrap(),
        &params,
        Instant::now() + Duration::from_millis(600),
    );
    worker.join().unwrap();
    assert!(matches!(delivery.outcome, SmtpOutcome::DefinitelyRejected));
}
