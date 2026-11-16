use super::*;
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    hash::MessageDigest,
    pkey::PKey,
    rsa::Rsa,
    x509::{
        X509, X509NameBuilder,
        extension::{BasicConstraints, SubjectAlternativeName},
    },
};
use std::{
    io::{BufRead, BufReader},
    net::TcpListener,
    thread,
};

pub(crate) fn certificate() -> (Vec<u8>, Vec<u8>) {
    let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "localhost").unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::from_unix(1577836800).unwrap())
        .unwrap();
    cert.set_not_after(&Asn1Time::from_unix(4102444800).unwrap())
        .unwrap();
    cert.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    let san = SubjectAlternativeName::new()
        .dns("localhost")
        .build(&cert.x509v3_context(None, None))
        .unwrap();
    cert.append_extension(san).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    (
        cert.build().to_pem().unwrap(),
        key.private_key_to_pem_pkcs8().unwrap(),
    )
}
pub(crate) fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}
pub(crate) fn server(l: &TcpListener, security: ConnectionSecurity) -> ServerSettings {
    ServerSettings {
        host: "localhost".into(),
        port: l.local_addr().unwrap().port(),
        username: "synthetic-user".into(),
        security,
    }
}
pub(crate) fn socket(l: TcpListener) -> TcpStream {
    let (s, _) = l.accept().unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    s
}
pub(crate) fn line(r: &mut impl BufRead) -> String {
    let mut s = String::new();
    r.read_line(&mut s).unwrap();
    s
}
pub(crate) fn acceptor(cert: &[u8], key: &[u8]) -> native_tls::TlsAcceptor {
    native_tls::TlsAcceptor::new(native_tls::Identity::from_pkcs8(cert, key).unwrap()).unwrap()
}
#[test]
fn imap_tls_and_starttls_authenticate_list_and_logout_only() {
    for security in [ConnectionSecurity::Tls, ConnectionSecurity::StartTls] {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listener = listener();
        let settings = server(&listener, security);
        let worker = thread::spawn(move || {
            let mut socket = socket(listener);
            if security == ConnectionSecurity::StartTls {
                socket.write_all(b"* OK synthetic\r\n").unwrap();
                assert_eq!(line(&mut BufReader::new(&mut socket)), "QM1 STARTTLS\r\n");
                socket.write_all(b"QM1 OK upgrade\r\n").unwrap();
            }
            let tls = accept.accept(socket).unwrap();
            let mut r = BufReader::new(tls);
            if security == ConnectionSecurity::Tls {
                r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
            }
            let login = line(&mut r);
            assert!(login.contains(" LOGIN "));
            let tag = login.split_whitespace().next().unwrap();
            r.get_mut()
                .write_all(format!("{tag} OK authenticated\r\n").as_bytes())
                .unwrap();
            let list = line(&mut r);
            assert!(list.contains(" LIST "));
            let tag = list.split_whitespace().next().unwrap();
            r.get_mut().write_all(format!("* LIST () \"/\" \"INBOX\"\r\n* LIST (\\Noselect) \"/\" \"Archive\"\r\n{tag} OK list\r\n").as_bytes()).unwrap();
            let logout = line(&mut r);
            assert!(logout.contains(" LOGOUT"));
            let tag = logout.split_whitespace().next().unwrap();
            let _ = r
                .get_mut()
                .write_all(format!("* BYE goodbye\r\n{tag} OK logout\r\n").as_bytes());
        });
        let connector = native_tls::TlsConnector::builder()
            .add_root_certificate(native_tls::Certificate::from_pem(&cert).unwrap())
            .build()
            .unwrap();
        let folders =
            check_imap_with_connector(&settings, "synthetic-password", &connector).unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].name, "INBOX");
        assert!(!folders[1].selectable);
        worker.join().unwrap();
    }
}
#[test]
fn starttls_rejection_never_sends_credentials() {
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::StartTls);
    let worker = thread::spawn(move || {
        let mut socket = socket(listener);
        socket.write_all(b"* OK synthetic\r\n").unwrap();
        let mut r = BufReader::new(socket);
        assert_eq!(line(&mut r), "QM1 STARTTLS\r\n");
        r.get_mut()
            .write_all(b"QM1 NO TLS unavailable\r\n")
            .unwrap();
        assert_eq!(line(&mut r), "");
    });
    assert_eq!(
        check_imap(&settings, "must-not-appear"),
        Err(CheckError::Tls)
    );
    worker.join().unwrap();
}
#[test]
fn certificate_failures_precede_authentication() {
    for trusted in [false, true] {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listener = listener();
        let mut settings = server(&listener, ConnectionSecurity::Tls);
        if trusted {
            settings.host = "127.0.0.1".into();
        }
        let worker = thread::spawn(move || {
            assert!(accept.accept(socket(listener)).is_err());
        });
        let mut builder = native_tls::TlsConnector::builder();
        if trusted {
            builder.add_root_certificate(native_tls::Certificate::from_pem(&cert).unwrap());
        }
        assert_eq!(
            check_imap_with_connector(&settings, "must-not-appear", &builder.build().unwrap()),
            Err(CheckError::Tls)
        );
        worker.join().unwrap();
    }
}
#[test]
fn smtp_tls_and_starttls_authenticate_and_noop_without_mail() {
    use lettre::transport::smtp::{
        SmtpTransport,
        client::{Certificate, Tls, TlsParameters},
    };
    for security in [ConnectionSecurity::Tls, ConnectionSecurity::StartTls] {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listener = listener();
        let settings = server(&listener, security);
        let worker = thread::spawn(move || {
            let mut socket = socket(listener);
            if security == ConnectionSecurity::StartTls {
                socket.write_all(b"220 synthetic\r\n").unwrap();
                assert!(line(&mut BufReader::new(&mut socket)).starts_with("EHLO "));
                socket
                    .write_all(b"250-localhost\r\n250 STARTTLS\r\n")
                    .unwrap();
                assert_eq!(line(&mut BufReader::new(&mut socket)), "STARTTLS\r\n");
                socket.write_all(b"220 upgrade\r\n").unwrap();
            }
            let tls = accept.accept(socket).unwrap();
            let mut r = BufReader::new(tls);
            if security == ConnectionSecurity::Tls {
                r.get_mut().write_all(b"220 synthetic\r\n").unwrap();
            }
            assert!(line(&mut r).starts_with("EHLO "));
            r.get_mut()
                .write_all(b"250-localhost\r\n250 AUTH PLAIN\r\n")
                .unwrap();
            assert!(line(&mut r).starts_with("AUTH PLAIN "));
            r.get_mut().write_all(b"235 authenticated\r\n").unwrap();
            assert_eq!(line(&mut r), "NOOP\r\n");
            r.get_mut().write_all(b"250 ok\r\n").unwrap();
            let quit = line(&mut r);
            assert!(quit.is_empty() || quit == "QUIT\r\n");
            if !quit.is_empty() {
                let _ = r.get_mut().write_all(b"221 bye\r\n");
            }
        });
        let params = TlsParameters::builder("localhost".into())
            .add_root_certificate(Certificate::from_pem(&cert).unwrap())
            .build()
            .unwrap();
        let tls = if security == ConnectionSecurity::Tls {
            Tls::Wrapper(params)
        } else {
            Tls::Required(params)
        };
        assert_eq!(
            smtp_using_builder(
                &settings,
                "synthetic-password",
                SmtpTransport::builder_dangerous("localhost").tls(tls)
            ),
            Ok(())
        );
        worker.join().unwrap();
    }
}
#[test]
fn smtp_missing_starttls_does_not_authenticate() {
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::StartTls);
    let worker = thread::spawn(move || {
        let mut socket = socket(listener);
        socket.write_all(b"220 synthetic\r\n").unwrap();
        let mut r = BufReader::new(socket);
        assert!(line(&mut r).starts_with("EHLO "));
        r.get_mut()
            .write_all(b"250-localhost\r\n250 AUTH PLAIN\r\n")
            .unwrap();
        let next = line(&mut r);
        assert!(next.is_empty() || next == "QUIT\r\n");
    });
    assert!(check_smtp(&settings, "must-not-appear").is_err());
    worker.join().unwrap();
}
#[test]
fn malformed_and_oversized_responses_are_bounded() {
    struct Transcript {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Transcript {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.input.read(b)
        }
    }
    impl Write for Transcript {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.output.extend(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    for (input, error) in [
        (b"* PREAUTH denied\r\n".to_vec(), CheckError::Protocol),
        (vec![b'x'; 8193], CheckError::Limit),
        (
            b"* OK ready\r\nOTHER OK bogus\r\n".to_vec(),
            CheckError::Protocol,
        ),
    ] {
        let mut t = Transcript {
            input: io::Cursor::new(input),
            output: vec![],
        };
        assert_eq!(starttls(&mut t), Err(error));
        assert!(!String::from_utf8_lossy(&t.output).contains("LOGIN"));
    }
    let mut b = Bounded::new(io::Cursor::new(vec![0; 1024 * 1024 + 1]));
    assert_eq!(
        io_error(io::copy(&mut b, &mut io::sink()).unwrap_err()),
        CheckError::Limit
    );
}

#[test]
fn folder_display_decodes_chinese_without_changing_remote_identifier() {
    assert_eq!(folder_label("&g0l6P3ux-"), "草稿箱");
    assert_eq!(folder_label("Inbox &- Archive"), "Inbox & Archive");
    assert_eq!(folder_label("&broken"), "&broken");
    assert_eq!(folder_label("中文 UTF8"), "中文 UTF8");
}
#[test]
fn imap_authentication_failure_redacts_server_response() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
        let login = line(&mut r);
        let tag = login.split_whitespace().next().unwrap();
        r.get_mut()
            .write_all(format!("{tag} NO echoed synthetic-secret\r\n").as_bytes())
            .unwrap();
    });
    let connector = native_tls::TlsConnector::builder()
        .add_root_certificate(native_tls::Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let error = check_imap_with_connector(&settings, "synthetic-secret", &connector).unwrap_err();
    assert_eq!(error, CheckError::Authentication);
    assert!(!error.to_string().contains("synthetic-secret"));
    worker.join().unwrap();
}

#[test]
fn smtp_authentication_failure_redacts_server_response() {
    use lettre::transport::smtp::{
        SmtpTransport,
        client::{Certificate, Tls, TlsParameters},
    };
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
        r.get_mut().write_all(b"220 synthetic\r\n").unwrap();
        assert!(line(&mut r).starts_with("EHLO "));
        r.get_mut()
            .write_all(b"250-localhost\r\n250 AUTH PLAIN\r\n")
            .unwrap();
        assert!(line(&mut r).starts_with("AUTH PLAIN "));
        r.get_mut()
            .write_all(b"535 echoed synthetic-secret\r\n")
            .unwrap();
    });
    let params = TlsParameters::builder("localhost".into())
        .add_root_certificate(Certificate::from_pem(&cert).unwrap())
        .build()
        .unwrap();
    let error = smtp_using_builder(
        &settings,
        "synthetic-secret",
        SmtpTransport::builder_dangerous("localhost").tls(Tls::Wrapper(params)),
    )
    .unwrap_err();
    assert_eq!(error, CheckError::Authentication);
    assert!(!error.to_string().contains("synthetic-secret"));
    worker.join().unwrap();
}
#[test]
fn smtp_untrusted_certificate_precedes_authentication() {
    let (cert, key) = certificate();
    let accept = acceptor(&cert, &key);
    let listener = listener();
    let settings = server(&listener, ConnectionSecurity::Tls);
    let worker = thread::spawn(move || {
        assert!(accept.accept(socket(listener)).is_err());
    });
    assert_eq!(
        check_smtp(&settings, "must-not-appear"),
        Err(CheckError::Tls)
    );
    worker.join().unwrap();
}

#[test]
fn tagged_mismatch_and_unexpected_nested_responses_return_errors() {
    let mut wrong = io::Cursor::new(b"WRONG OK echoed synthetic-secret\r\n");
    assert_eq!(
        imap_done(&mut wrong, "QM2", true, false),
        Err(CheckError::Protocol)
    );
    let mut fetch = io::Cursor::new(b"* 1 FETCH (BODYSTRUCTURE ((((((\r\n");
    assert!(matches!(
        imap_response(&mut fetch),
        Err(CheckError::Protocol)
    ));
    let mut list =
        io::Cursor::new(b"* LIST () \"/\" {6}\r\n\xe4\xb8\xad\xe6\x96\x87\r\nQM3 OK listed\r\n");
    let folders = imap_done(&mut list, "QM3", false, true).unwrap();
    assert_eq!(folders[0].name, "中文");
    let mut escaped = zeroize::Zeroizing::new(Vec::new());
    quoted(&mut escaped, "a\"b\\c");
    assert_eq!(&*escaped, b"\"a\\\"b\\\\c\"");
}
