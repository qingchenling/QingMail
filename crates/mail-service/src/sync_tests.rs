use super::*;
use crate::accounts::tests::{acceptor, certificate, line, listener, server, socket};
use mail_core::ConnectionSecurity;
use std::{
    io::{BufReader, Write},
    thread,
};

fn fixture(epoch: u32, broken: bool, before: Option<u32>) -> Result<FolderBatch, CheckError> {
    fixture_expected(epoch, broken, before, Some(epoch))
}
fn fixture_expected(
    epoch: u32,
    broken: bool,
    before: Option<u32>,
    expected: Option<u32>,
) -> Result<FolderBatch, CheckError> {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        let login = line(&mut r);
        assert!(login.starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert_eq!(line(&mut r), "QM3 EXAMINE \"INBOX\"\r\n");
        r.get_mut().write_all(format!("* FLAGS (\\Seen)\r\n* 4 EXISTS\r\n* OK [UIDVALIDITY {epoch}] stable\r\nQM3 OK [READ-ONLY] examined\r\n").as_bytes()).unwrap();
        if before.is_some() && expected.is_some_and(|old| old != epoch) {
            let mut after = String::new();
            let _ = std::io::BufRead::read_line(&mut r, &mut after);
            assert!(after.is_empty());
            return;
        }
        let search = line(&mut r);
        assert!(search.starts_with("QM4 UID SEARCH SINCE "));
        r.get_mut()
            .write_all(b"* SEARCH 9 3 7\r\nQM4 OK searched\r\n")
            .unwrap();
        let fetch = line(&mut r);
        let expected = if before.is_some() { "3" } else { "3,7,9" };
        assert_eq!(
            fetch,
            format!(
                "QM5 UID FETCH {expected} (UID FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT FROM DATE)])\r\n"
            )
        );
        for uid in if before.is_some() {
            vec![3]
        } else {
            vec![9, 3, 7]
        } {
            let header = format!(
                "Subject: =?UTF-8?B?5Lit5paH?= {uid}\r\nFrom: =?UTF-8?B?5rWL6K+V?= <sender@example.invalid>\r\nDate: Mon, 16 Nov 2026 08:00:00 +0000\r\n\r\n"
            );
            r.get_mut().write_all(format!("* 1 FETCH (UID {uid} BODY[HEADER.FIELDS (SUBJECT FROM DATE)] {{{}}}\r\n{header})\r\n",header.len()).as_bytes()).unwrap();
            if broken {
                return;
            }
        }
        r.get_mut().write_all(b"QM5 OK fetched\r\n").unwrap();
        assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
    });
    let tls = native_tls::TlsConnector::builder()
        .add_root_certificate(native_tls::Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let result = folder_using(
        &settings,
        "synthetic-password",
        "INBOX",
        before,
        expected,
        1794816000,
        &tls,
    );
    worker.join().unwrap();
    result
}
#[test]
fn read_only_recent_headers_are_decoded_and_use_uid_not_sequence() {
    let result = fixture(42, false, None).unwrap();
    assert_eq!(result.uidvalidity, 42);
    assert_eq!(
        result
            .messages
            .iter()
            .map(|(uid, _)| *uid)
            .collect::<Vec<_>>(),
        vec![3, 7, 9]
    );
    assert_eq!(result.messages[0].1.subject, "中文 3");
    assert_eq!(result.messages[0].1.sender, "测试 <sender@example.invalid>");
    assert!(result.messages[0].1.received_at > 0);
    assert_eq!(fixture(43, false, Some(7)).unwrap().messages.len(), 1);
}
#[test]
fn disconnect_before_tagged_success_does_not_return_partial_epoch() {
    assert!(fixture(99, true, None).is_err());
}
#[test]
fn protocol_depth_guard_skips_literal_and_quoted_data() {
    assert_eq!(guard_response(&[b'('; 17]), Err(CheckError::Limit));
    assert!(
        guard_response(b"* 1 FETCH (BODY[HEADER] {20}\r\n((((((((((((((((((((\r\n)\r\n").is_ok()
    );
    assert!(guard_response(b"* LIST () \"/\" \"((((((((((((((((((((\"\r\n").is_ok());
    assert_eq!(
        guard_response(b"* 1 FETCH (BODY[HEADER] {999999999}\r\n"),
        Err(CheckError::Limit)
    );
}

#[test]
fn changed_epoch_rejects_old_paging_cursor_before_search() {
    assert_eq!(
        fixture_expected(99, false, Some(7), Some(42)),
        Err(CheckError::EpochChanged)
    );
}

#[test]
fn more_than_200_results_are_bounded_without_claiming_full_sync() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM3 EXAMINE "));
        r.get_mut()
            .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM3 OK examined\r\n")
            .unwrap();
        assert!(line(&mut r).starts_with("QM4 UID SEARCH SINCE "));
        let uids = (1..=201)
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        r.get_mut()
            .write_all(format!("* SEARCH {uids}\r\nQM4 OK searched\r\n").as_bytes())
            .unwrap();
        let fetch = line(&mut r);
        assert!(!fetch.contains("FETCH 1,"));
        assert!(fetch.starts_with("QM5 UID FETCH 2,3,"));
        assert!(fetch.contains(",200,201 "));
        for uid in 2..=201 {
            let header = format!("Subject: recent {uid}\r\n\r\n");
            r.get_mut().write_all(format!("* {uid} FETCH (UID {uid} BODY[HEADER.FIELDS (SUBJECT FROM DATE)] {{{}}}\r\n{header})\r\n",header.len()).as_bytes()).unwrap();
        }
        r.get_mut().write_all(b"QM5 OK fetched\r\n").unwrap();
        assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
    });
    let tls = native_tls::TlsConnector::builder()
        .add_root_certificate(native_tls::Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let result = folder_using(
        &settings,
        "synthetic-password",
        "INBOX",
        None,
        None,
        1794816000,
        &tls,
    )
    .unwrap();
    assert_eq!(result.messages.len(), 200);
    assert_eq!(result.messages[0].0, 2);
    assert!(result.has_more);
    worker.join().unwrap();
}

fn body_fixture(mode: &str) -> Result<Vec<u8>, CheckError> {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let mode = mode.to_owned();
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert_eq!(line(&mut r), "QM3 EXAMINE \"INBOX\"\r\n");
        let epoch = if mode == "epoch" { 99 } else { 42 };
        r.get_mut()
            .write_all(
                format!("* OK [UIDVALIDITY {epoch}] stable\r\nQM3 OK [READ-ONLY] examined\r\n")
                    .as_bytes(),
            )
            .unwrap();
        if mode == "epoch" {
            let mut next = String::new();
            let _ = std::io::BufRead::read_line(&mut r, &mut next);
            assert!(next.is_empty());
            return;
        }
        assert_eq!(line(&mut r), "QM4 UID FETCH 7 (UID BODY.PEEK[])\r\n");
        if mode == "missing" {
            r.get_mut()
                .write_all(b"QM4 OK no matching UID\r\n")
                .unwrap();
            return;
        }
        if mode == "oversize" {
            r.get_mut()
                .write_all(b"* 1 FETCH (UID 7 BODY[] {8388609}\r\n")
                .unwrap();
            return;
        }
        let mut raw="Subject: downloaded\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n正文🙂\n((((( \"quoted\" { literal data".as_bytes().to_vec();
        if mode == "large" {
            raw.extend(vec![b'x'; 2 * 1024 * 1024]);
        }
        let uid = if mode == "wrong-uid" { 8 } else { 7 };
        r.get_mut()
            .write_all(format!("* 88 FETCH (UID {uid} BODY[] {{{}}}\r\n", raw.len()).as_bytes())
            .unwrap();
        if mode == "partial" {
            r.get_mut().write_all(&raw[..20]).unwrap();
            return;
        }
        r.get_mut().write_all(&raw).unwrap();
        r.get_mut().write_all(b")\r\nQM4 OK fetched\r\n").unwrap();
        if mode == "wrong-uid" {
            return;
        }
        assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
    });
    let tls = native_tls::TlsConnector::builder()
        .add_root_certificate(native_tls::Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let result = message_using(&settings, "synthetic-password", "INBOX", 42, 7, &tls);
    worker.join().unwrap();
    result
}
#[test]
fn full_mime_download_is_read_only_and_preserves_literal_bytes_above_index_budget() {
    let small = body_fixture("good").unwrap();
    assert!(small.ends_with(b"literal data"));
    assert!(String::from_utf8_lossy(&small).contains("正文🙂"));
    let large = body_fixture("large").unwrap();
    assert_eq!(large.len(), small.len() + 2 * 1024 * 1024);
}
#[test]
fn changed_epoch_wrong_uid_truncation_and_oversize_never_return_a_body() {
    assert_eq!(body_fixture("epoch"), Err(CheckError::EpochChanged));
    assert_eq!(body_fixture("missing"), Err(CheckError::Missing));
    assert_eq!(body_fixture("wrong-uid"), Err(CheckError::Protocol));
    assert_eq!(body_fixture("oversize"), Err(CheckError::Limit));
    assert!(body_fixture("partial").is_err());
}

#[test]
fn history_search_uses_before_and_caches_remote_flags() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert_eq!(line(&mut r), "QM3 EXAMINE \"INBOX\"\r\n");
        r.get_mut()
            .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM3 OK examined\r\n")
            .unwrap();
        let search = line(&mut r);
        assert!(search.starts_with("QM4 UID SEARCH BEFORE "), "{search}");
        r.get_mut()
            .write_all(b"* SEARCH 5 8\r\nQM4 OK searched\r\n")
            .unwrap();
        assert_eq!(
            line(&mut r),
            "QM5 UID FETCH 5,8 (UID FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT FROM DATE)])\r\n"
        );
        for (uid, flags) in [(5, ""), (8, "\\Seen \\Flagged")] {
            let header = format!(
                "Subject: old {uid}\r\nFrom: old@example.invalid\r\nDate: Mon, 01 Jan 2024 08:00:00 +0000\r\n\r\n"
            );
            r.get_mut().write_all(format!("* {uid} FETCH (UID {uid} FLAGS ({flags}) BODY[HEADER.FIELDS (SUBJECT FROM DATE)] {{{}}}\r\n{header})\r\n", header.len()).as_bytes()).unwrap();
        }
        r.get_mut().write_all(b"QM5 OK fetched\r\n").unwrap();
        assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
    });
    let tls = native_tls::TlsConnector::builder()
        .add_root_certificate(native_tls::Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let result = fetch_folder(
        &settings,
        "synthetic-password",
        FolderQuery {
            name: "INBOX",
            before: None,
            expected_epoch: None,
            kind: BatchKind::History,
        },
        1794816000,
        &tls,
    )
    .unwrap();
    worker.join().unwrap();
    assert_eq!(result.kind, BatchKind::History);
    assert_eq!(
        result
            .messages
            .iter()
            .map(|(uid, _)| *uid)
            .collect::<Vec<_>>(),
        vec![5, 8]
    );
    assert!(result.flags[&8].seen);
    assert!(result.flags[&8].flagged);
    assert!(!result.flags[&5].seen);
    assert!(!result.flags[&5].flagged);
}
