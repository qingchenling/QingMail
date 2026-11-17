use super::*;
use crate::accounts::tests::{acceptor, certificate, line, listener, server, socket};
use mail_core::ConnectionSecurity;
use std::{
    io::{BufReader, Write},
    thread,
};

fn tls(cert: &[u8]) -> native_tls::TlsConnector {
    native_tls::TlsConnector::builder()
        .add_root_certificate(native_tls::Certificate::from_pem(cert).unwrap())
        .build()
        .unwrap()
}

#[test]
fn store_flags_sends_directional_updates_after_matching_epoch() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert_eq!(line(&mut r), "QM3 SELECT \"INBOX\"\r\n");
        r.get_mut()
            .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM3 OK selected\r\n")
            .unwrap();
        assert_eq!(line(&mut r), "QM4 UID STORE 5,7 +FLAGS.SILENT (\\Seen)\r\n");
        r.get_mut().write_all(b"QM4 OK stored\r\n").unwrap();
        assert_eq!(
            line(&mut r),
            "QM4 UID STORE 5,7 -FLAGS.SILENT (\\Flagged)\r\n"
        );
        r.get_mut().write_all(b"QM4 OK stored\r\n").unwrap();
        assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
    });
    store_flags_using(
        &settings,
        "synthetic-password",
        "INBOX",
        42,
        &[5, 7],
        FlagChange {
            seen: Some(true),
            flagged: Some(false),
        },
        &tls(&cert),
    )
    .unwrap();
    worker.join().unwrap();
}

#[test]
fn move_prefers_uid_move_and_falls_back_to_copy_store_expunge() {
    for mode in ["move", "fallback"] {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listener = listener();
        let settings = server(&listener, ConnectionSecurity::Tls);
        let worker = thread::spawn(move || {
            let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
            r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM2 LOGIN "));
            r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM3 SELECT "));
            r.get_mut()
                .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM3 OK selected\r\n")
                .unwrap();
            assert_eq!(line(&mut r), "QM4 UID MOVE 5,7 \"Archive\"\r\n");
            if mode == "move" {
                r.get_mut().write_all(b"QM4 OK moved\r\n").unwrap();
                assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
                return;
            }
            r.get_mut().write_all(b"QM4 NO unsupported\r\n").unwrap();
            assert_eq!(line(&mut r), "QM4 UID COPY 5,7 \"Archive\"\r\n");
            r.get_mut().write_all(b"QM4 OK copied\r\n").unwrap();
            assert_eq!(
                line(&mut r),
                "QM5 UID STORE 5,7 +FLAGS.SILENT (\\Deleted)\r\n"
            );
            r.get_mut().write_all(b"QM5 OK stored\r\n").unwrap();
            assert_eq!(line(&mut r), "QM6 UID EXPUNGE 5,7\r\n");
            r.get_mut()
                .write_all(b"QM6 NO expunge unsupported\r\n")
                .unwrap();
            assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
        });
        let outcome = move_messages_using(
            &settings,
            "synthetic-password",
            "INBOX",
            42,
            &[5, 7],
            "Archive",
            &tls(&cert),
        )
        .unwrap();
        worker.join().unwrap();
        assert_eq!(
            outcome,
            if mode == "move" {
                MoveOutcome::Moved
            } else {
                MoveOutcome::CopiedOnly
            }
        );
    }
}

#[test]
fn changed_epoch_or_invalid_input_never_stores() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM2 LOGIN "));
        r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
        assert!(line(&mut r).starts_with("QM3 SELECT "));
        r.get_mut()
            .write_all(b"* OK [UIDVALIDITY 43] changed\r\nQM3 OK selected\r\n")
            .unwrap();
        assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
    });
    assert_eq!(
        store_flags_using(
            &settings,
            "synthetic-password",
            "INBOX",
            42,
            &[5],
            FlagChange {
                seen: Some(true),
                flagged: None,
            },
            &tls(&cert)
        ),
        Err(CheckError::EpochChanged)
    );
    worker.join().unwrap();

    let offline = ServerSettings {
        host: "127.0.0.1".into(),
        port: 1,
        username: "synthetic".into(),
        security: ConnectionSecurity::Tls,
    };
    let connector = tls(&certificate().0);
    let _ = connector;
    for (folder, uids, seen, flagged) in [
        ("", vec![1], Some(true), None),
        ("INBOX", vec![], Some(true), None),
        ("INBOX", vec![0], Some(true), None),
        ("INBOX", vec![1], None, None),
    ] {
        assert_eq!(
            store_flags(
                &offline,
                "synthetic-password",
                folder,
                42,
                &uids,
                FlagChange { seen, flagged }
            ),
            Err(CheckError::Invalid)
        );
    }
    assert_eq!(
        move_messages(&offline, "synthetic-password", "INBOX", 42, &[1], ""),
        Err(CheckError::Invalid)
    );
}

#[test]
fn queued_move_distinguishes_preflight_epoch_failure_from_lost_move_response() {
    for changed in [true, false] {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listener = listener();
        let settings = server(&listener, ConnectionSecurity::Tls);
        let worker = thread::spawn(move || {
            let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
            r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM2 LOGIN "));
            r.get_mut().write_all(b"QM2 OK login\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM3 SELECT "));
            r.get_mut()
                .write_all(if changed {
                    b"* OK [UIDVALIDITY 43] new\r\nQM3 OK selected\r\n"
                } else {
                    b"* OK [UIDVALIDITY 42] old\r\nQM3 OK selected\r\n"
                })
                .unwrap();
            if changed {
                assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
            } else {
                assert_eq!(line(&mut r), "QM4 UID MOVE 7 \"Trash\"\r\n");
            }
            // 丢失确认，不能推断该 MOVE 未执行。
        });
        let failure = move_checked_using(
            &settings,
            "fixture",
            "INBOX",
            42,
            &[7],
            "Trash",
            &tls(&cert),
        )
        .unwrap_err();
        assert_eq!(failure.may_have_changed, !changed);
        if changed {
            assert_eq!(failure.error, CheckError::EpochChanged);
        }
        worker.join().unwrap();
    }
}

#[test]
fn reconciliation_probe_is_read_only_and_checks_uid_and_flags() {
    for exists in [true, false] {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listener = listener();
        let settings = server(&listener, ConnectionSecurity::Tls);
        let worker = thread::spawn(move || {
            let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
            r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM2 LOGIN "));
            r.get_mut().write_all(b"QM2 OK login\r\n").unwrap();
            assert_eq!(line(&mut r), "QM3 SELECT \"INBOX\"\r\n");
            r.get_mut()
                .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM3 OK selected\r\n")
                .unwrap();
            assert_eq!(line(&mut r), "QM4 UID FETCH 7 (UID FLAGS)\r\n");
            if exists {
                r.get_mut()
                    .write_all(b"* 1 FETCH (UID 7 FLAGS (\\Seen \\Flagged))\r\n")
                    .unwrap();
            }
            r.get_mut().write_all(b"QM4 OK fetched\r\n").unwrap();
            assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
        });
        assert_eq!(
            probe_using(&settings, "fixture", "INBOX", 42, 7, &tls(&cert)).unwrap(),
            if exists {
                Some(FlagChange {
                    seen: Some(true),
                    flagged: Some(true),
                })
            } else {
                None
            }
        );
        worker.join().unwrap();
    }
}
