use super::*;
use crate::accounts::tests::{acceptor, certificate, line, listener, server, socket};
use mail_core::ConnectionSecurity;
use std::{
    io::{BufReader, Read, Write},
    thread,
};

const MESSAGE_ID: &str = "<0123456789abcdef0123456789abcdef@qingmail.invalid>";

fn frozen() -> Vec<u8> {
    b"Date: Mon, 16 Nov 2026 08:00:00 +0000\r\nMessage-ID: <0123456789abcdef0123456789abcdef@qingmail.invalid>\r\nSubject: copy\r\n\r\nbody".to_vec()
}
fn client(cert: &[u8]) -> native_tls::TlsConnector {
    native_tls::TlsConnector::builder()
        .add_root_certificate(native_tls::Certificate::from_pem(cert).unwrap())
        .build()
        .unwrap()
}
fn folder(name: &str) -> RemoteFolder {
    RemoteFolder {
        name: name.into(),
        display_name: name.into(),
        delimiter: Some("/".into()),
        selectable: true,
    }
}

#[test]
fn sent_folder_choice_is_deterministic_and_rejects_no_select() {
    let folders = vec![
        folder("INBOX"),
        folder("Drafts"),
        folder("Sent Items"),
        folder("Archive"),
        RemoteFolder {
            selectable: false,
            ..folder("Sent")
        },
    ];
    assert_eq!(
        choose_sent_folder(&folders, None),
        Some("Sent Items".to_string())
    );
    // 上次记录的文件夹优先，即使启发式会选另一个。
    assert_eq!(
        choose_sent_folder(&folders, Some("Archive")),
        Some("Archive".to_string())
    );
    assert_eq!(
        choose_sent_folder(&folders, Some("missing")),
        Some("Sent Items".to_string())
    );
    let nested = vec![
        RemoteFolder {
            name: "[Gmail]/All Mail".into(),
            display_name: "All".into(),
            delimiter: Some("/".into()),
            selectable: true,
        },
        RemoteFolder {
            name: "[Gmail]/Sent Mail".into(),
            display_name: "Sent".into(),
            delimiter: Some("/".into()),
            selectable: true,
        },
    ];
    assert_eq!(
        choose_sent_folder(&nested, None),
        Some("[Gmail]/Sent Mail".to_string())
    );
    // 中文文件夹在协议层是 modified UTF-7；解码后再匹配常见名称。
    assert_eq!(
        choose_sent_folder(&[folder("&XfJT0ZAB-")], None),
        Some("&XfJT0ZAB-".to_string())
    );
    for name in ["已发送", "已发送邮件", "Sent Messages", "Sent"] {
        assert_eq!(
            choose_sent_folder(&[folder(name)], None),
            Some(name.to_string()),
            "{name}"
        );
    }
    assert_eq!(
        choose_sent_folder(&[folder("INBOX"), folder("Drafts")], None),
        None
    );
}

#[test]
fn append_writes_the_exact_frozen_bytes_and_reports_appenduid() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let raw = frozen();
    let expected = raw.clone();
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert_eq!(line(&mut r), "QM3 LIST \"\" \"*\"\r\n");
        r.get_mut()
            .write_all(
                b"* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\HasNoChildren) \"/\" \"Sent\"\r\nQM3 OK listed\r\n",
            )
            .unwrap();
        assert_eq!(line(&mut r), "QM4 EXAMINE \"Sent\"\r\n");
        r.get_mut()
            .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM4 OK [READ-ONLY] examined\r\n")
            .unwrap();
        assert_eq!(
            line(&mut r),
            format!("QM5 UID SEARCH HEADER Message-ID \"{MESSAGE_ID}\"\r\n")
        );
        r.get_mut()
            .write_all(b"* SEARCH\r\nQM5 OK searched\r\n")
            .unwrap();
        assert_eq!(
            line(&mut r),
            format!("QM6 APPEND \"Sent\" (\\Seen) {{{}}}\r\n", expected.len())
        );
        r.get_mut().write_all(b"+ ready\r\n").unwrap();
        let mut literal = vec![0; expected.len()];
        r.read_exact(&mut literal).unwrap();
        assert_eq!(literal, expected);
        r.get_mut()
            .write_all(b"QM6 OK [APPENDUID 42 7] appended\r\n")
            .unwrap();
        assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
    });
    let result = save_copy_using(
        &settings,
        "synthetic-password",
        MESSAGE_ID,
        &raw,
        None,
        &client(&cert),
    );
    worker.join().unwrap();
    assert_eq!(
        result,
        CopyResult::Saved(CopiedMessage {
            folder: "Sent".into(),
            uidvalidity: 42,
            uid: Some(7),
            existed: false,
        })
    );
}

#[test]
fn existing_message_id_is_found_without_appending_a_second_copy() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM3 LIST "));
        r.get_mut()
            .write_all(b"* LIST () \"/\" \"Sent\"\r\nQM3 OK listed\r\n")
            .unwrap();
        assert!(line(&mut r).starts_with("QM4 EXAMINE "));
        r.get_mut()
            .write_all(b"* OK [UIDVALIDITY 7] stable\r\nQM4 OK examined\r\n")
            .unwrap();
        let search = line(&mut r);
        assert!(search.starts_with("QM5 UID SEARCH HEADER Message-ID "));
        r.get_mut()
            .write_all(b"* SEARCH 9\r\nQM5 OK searched\r\n")
            .unwrap();
        // 查重命中后必须直接退出，不能再出现 APPEND。
        assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
    });
    let result = save_copy_using(
        &settings,
        "synthetic-password",
        MESSAGE_ID,
        &frozen(),
        None,
        &client(&cert),
    );
    worker.join().unwrap();
    assert_eq!(
        result,
        CopyResult::Saved(CopiedMessage {
            folder: "Sent".into(),
            uidvalidity: 7,
            uid: Some(9),
            existed: true,
        })
    );
}

#[test]
fn missing_sent_folder_fails_before_any_append() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM3 LIST "));
        r.get_mut()
            .write_all(
                b"* LIST () \"/\" \"INBOX\"\r\n* LIST () \"/\" \"Drafts\"\r\nQM3 OK listed\r\n",
            )
            .unwrap();
        // 没有可用的已发送文件夹时不得尝试 EXAMINE 或 APPEND。
        assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
    });
    let result = save_copy_using(
        &settings,
        "synthetic-password",
        MESSAGE_ID,
        &frozen(),
        None,
        &client(&cert),
    );
    worker.join().unwrap();
    assert_eq!(result, CopyResult::Failed(CopyError::NoSentFolder));
}

#[test]
fn refused_append_and_failed_search_are_reported_without_retry() {
    for mode in ["append-no", "search-no"] {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listener = listener();
        let settings = server(&listener, ConnectionSecurity::Tls);
        let worker = thread::spawn(move || {
            let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
            r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM2 LOGIN "));
            r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM3 LIST "));
            r.get_mut()
                .write_all(b"* LIST () \"/\" \"Sent\"\r\nQM3 OK listed\r\n")
                .unwrap();
            assert!(line(&mut r).starts_with("QM4 EXAMINE "));
            r.get_mut()
                .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM4 OK examined\r\n")
                .unwrap();
            assert!(line(&mut r).starts_with("QM5 UID SEARCH "));
            if mode == "search-no" {
                r.get_mut()
                    .write_all(b"QM5 NO search disabled\r\n")
                    .unwrap();
            } else {
                r.get_mut()
                    .write_all(b"* SEARCH\r\nQM5 OK searched\r\n")
                    .unwrap();
                let append = line(&mut r);
                assert!(append.starts_with("QM6 APPEND "));
                let length: usize = append
                    .split('{')
                    .nth(1)
                    .unwrap()
                    .trim_end_matches("}\r\n")
                    .parse()
                    .unwrap();
                r.get_mut().write_all(b"+ ready\r\n").unwrap();
                let mut literal = vec![0; length];
                r.read_exact(&mut literal).unwrap();
                assert_eq!(literal, frozen());
                r.get_mut()
                    .write_all(b"QM6 NO [TOOBIG] too large\r\n")
                    .unwrap();
            }
            assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
        });
        let result = save_copy_using(
            &settings,
            "synthetic-password",
            MESSAGE_ID,
            &frozen(),
            None,
            &client(&cert),
        );
        worker.join().unwrap();
        assert_eq!(
            result,
            CopyResult::Failed(if mode == "search-no" {
                CopyError::SearchFailed
            } else {
                CopyError::Rejected
            }),
            "{mode}"
        );
    }
}

#[test]
fn disconnect_after_literal_is_reported_as_unknown_not_saved() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let raw = frozen();
    let length = raw.len();
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM3 LIST "));
        r.get_mut()
            .write_all(b"* LIST () \"/\" \"Sent\"\r\nQM3 OK listed\r\n")
            .unwrap();
        assert!(line(&mut r).starts_with("QM4 EXAMINE "));
        r.get_mut()
            .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM4 OK examined\r\n")
            .unwrap();
        assert!(line(&mut r).starts_with("QM5 UID SEARCH "));
        r.get_mut()
            .write_all(b"* SEARCH\r\nQM5 OK searched\r\n")
            .unwrap();
        assert!(line(&mut r).starts_with("QM6 APPEND "));
        r.get_mut().write_all(b"+ ready\r\n").unwrap();
        let mut literal = vec![0; length];
        r.read_exact(&mut literal).unwrap();
        // 读完正文后立即断开：客户端无法确认服务器是否已保存。
    });
    let result = save_copy_using(
        &settings,
        "synthetic-password",
        MESSAGE_ID,
        &raw,
        None,
        &client(&cert),
    );
    worker.join().unwrap();
    assert_eq!(result, CopyResult::Failed(CopyError::ResultUnknown));
    assert!(matches!(
        result,
        CopyResult::Failed(error) if error.is_uncertain()
    ));
}

#[test]
fn invalid_identifier_or_bytes_never_open_a_connection() {
    let settings = ServerSettings {
        host: "127.0.0.1".into(),
        port: 1,
        username: "synthetic".into(),
        security: ConnectionSecurity::Tls,
    };
    let connector = client(&certificate().0);
    for (message_id, raw) in [
        ("not a message id", frozen()),
        ("<a@b>", vec![]),
        (MESSAGE_ID, vec![b'x'; MAX_COPY_BYTES + 1]),
    ] {
        assert_eq!(
            save_copy_using(
                &settings,
                "synthetic-password",
                message_id,
                &raw,
                None,
                &connector
            ),
            CopyResult::Failed(CopyError::Invalid)
        );
    }
}
