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
