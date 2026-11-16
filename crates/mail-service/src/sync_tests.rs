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
                "QM5 UID FETCH {expected} (UID BODY.PEEK[HEADER.FIELDS (SUBJECT FROM DATE)])\r\n"
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
