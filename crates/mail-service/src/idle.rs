//! 有界只读 IDLE；不支持时由调度器回退轮询。超时后丢弃会话，避免解析半帧后复用连接。
use crate::{
    accounts::{CheckError, authenticated, response},
    sync::{command, logout, until_done},
};
use imap_proto::{Capability, MailboxDatum, Response, Status};
use mail_core::ServerSettings;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleResult {
    Unsupported,
    Quiet,
    Changed,
}
pub fn watch(server: &ServerSettings, password: &str) -> Result<IdleResult, CheckError> {
    let tls = native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()
        .map_err(|_| CheckError::Tls)?;
    watch_using(server, password, &tls, std::time::Duration::from_secs(15))
}
fn watch_using(
    server: &ServerSettings,
    password: &str,
    tls: &native_tls::TlsConnector,
    timeout: std::time::Duration,
) -> Result<IdleResult, CheckError> {
    let mut stream = authenticated(server, password, tls)?;
    command(&mut stream, b"QM3 CAPABILITY\r\n")?;
    let mut supported = false;
    until_done(&mut stream, "QM3", |r| {
        if let Response::Capabilities(caps) = r {
            supported = caps
                .iter()
                .any(|c| matches!(c,Capability::Atom(atom) if atom.eq_ignore_ascii_case("IDLE")));
        }
        Ok(())
    })?;
    if !supported {
        logout(&mut stream);
        return Ok(IdleResult::Unsupported);
    }
    command(&mut stream, b"QM4 EXAMINE \"INBOX\"\r\n")?;
    until_done(&mut stream, "QM4", |_| Ok(()))?;
    command(&mut stream, b"QM5 IDLE\r\n")?;
    if !matches!(response(&mut stream, true)?, Response::Continue(_)) {
        return Err(CheckError::Protocol);
    }
    crate::accounts::idle_deadline(&mut stream, timeout)?;
    for _ in 0..256 {
        match response(&mut stream, true) {
            Ok(
                Response::Expunge(_)
                | Response::Fetch(_, _)
                | Response::MailboxData(MailboxDatum::Exists(_)),
            ) => {
                command(&mut stream, b"DONE\r\n")?;
                until_done(&mut stream, "QM5", |_| Ok(()))?;
                logout(&mut stream);
                return Ok(IdleResult::Changed);
            }
            Ok(Response::Data {
                status: Status::Bye,
                ..
            }) => return Err(CheckError::Connect),
            Ok(Response::Done { .. } | Response::Continue(_)) => return Err(CheckError::Protocol),
            Err(CheckError::Timeout) => return Ok(IdleResult::Quiet),
            Err(e) => return Err(e),
            _ => (),
        }
    }
    Err(CheckError::Limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::tests::{acceptor, certificate, line, listener, server, socket};
    use std::{
        io::{BufReader, Write},
        thread,
        time::Duration,
    };
    fn fixture(mode: u8) -> Result<IdleResult, CheckError> {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listener = listener();
        let settings = server(&listener, mail_core::ConnectionSecurity::Tls);
        let worker = thread::spawn(move || {
            let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
            r.get_mut().write_all(b"* OK synthetic\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM2 LOGIN "));
            r.get_mut().write_all(b"QM2 OK authenticated\r\n").unwrap();
            assert_eq!(line(&mut r), "QM3 CAPABILITY\r\n");
            r.get_mut()
                .write_all(if mode == 0 {
                    b"* CAPABILITY IMAP4rev1\r\nQM3 OK caps\r\n"
                } else {
                    b"* CAPABILITY IMAP4rev1 IDLE\r\nQM3 OK caps\r\n"
                })
                .unwrap();
            if mode == 0 {
                assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
                return;
            }
            assert_eq!(line(&mut r), "QM4 EXAMINE \"INBOX\"\r\n");
            r.get_mut()
                .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM4 OK selected\r\n")
                .unwrap();
            assert_eq!(line(&mut r), "QM5 IDLE\r\n");
            r.get_mut().write_all(b"+ idling\r\n").unwrap();
            if mode == 1 {
                r.get_mut().write_all(b"* 3 EXISTS\r\n").unwrap();
                assert_eq!(line(&mut r), "DONE\r\n");
                r.get_mut().write_all(b"QM5 OK idle finished\r\n").unwrap();
                assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
            } else {
                if mode == 3 {
                    r.get_mut().write_all(b"* 3 EX").unwrap();
                }
                // 不返回完整事件；客户端应关闭超时/半帧连接，不在不确定的解析位置重用。
                let mut after = String::new();
                let _ = std::io::BufRead::read_line(&mut r, &mut after);
                assert!(after.is_empty());
            }
        });
        let tls = native_tls::TlsConnector::builder()
            .add_root_certificate(native_tls::Certificate::from_pem(&cert).unwrap())
            .build()
            .unwrap();
        let result = watch_using(&settings, "synthetic", &tls, Duration::from_millis(80));
        worker.join().unwrap();
        result
    }
    #[test]
    fn unsupported_capability_returns_poll_fallback_without_idle() {
        assert_eq!(fixture(0), Ok(IdleResult::Unsupported));
    }
    #[test]
    fn idle_exists_finishes_before_requesting_sync() {
        assert_eq!(fixture(1), Ok(IdleResult::Changed));
    }
    #[test]
    fn quiet_and_partial_responses_do_not_publish_a_false_change() {
        assert_eq!(fixture(2), Ok(IdleResult::Quiet));
        assert_eq!(fixture(3), Ok(IdleResult::Quiet));
    }
}
