//! 有界只读 IDLE；不支持时由调度器回退轮询。超时后丢弃会话，避免解析半帧后复用连接。
use crate::{
    accounts::{CheckError, ImapStream, authenticated, response, with_read_cancellation},
    sync::{command, logout, until_done},
};
use imap_proto::{Capability, MailboxDatum, Response, Status};
use mail_core::ServerSettings;
use std::sync::atomic::{AtomicBool, Ordering};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleResult {
    Unsupported,
    Quiet,
    Changed,
}
pub fn watch(server: &ServerSettings, password: &str) -> Result<IdleResult, CheckError> {
    watch_with_cancel(server, password, &AtomicBool::new(false))
}
pub fn watch_with_cancel(
    server: &ServerSettings,
    password: &str,
    cancel: &AtomicBool,
) -> Result<IdleResult, CheckError> {
    if cancel.load(Ordering::Acquire) {
        return Err(CheckError::Cancelled);
    }
    let tls = native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()
        .map_err(|_| CheckError::Tls)?;
    watch_using(
        server,
        password,
        &tls,
        std::time::Duration::from_secs(15),
        cancel,
    )
}
fn watch_using(
    server: &ServerSettings,
    password: &str,
    tls: &native_tls::TlsConnector,
    timeout: std::time::Duration,
    cancel: &AtomicBool,
) -> Result<IdleResult, CheckError> {
    let mut stream = authenticated(server, password, tls)?;
    with_read_cancellation(&mut stream, cancel, |stream| watch_stream(stream, timeout))
}
fn watch_stream(
    stream: &mut ImapStream,
    timeout: std::time::Duration,
) -> Result<IdleResult, CheckError> {
    command(stream, b"QM3 CAPABILITY\r\n")?;
    let mut supported = false;
    until_done(stream, "QM3", |r| {
        if let Response::Capabilities(caps) = r {
            supported = caps
                .iter()
                .any(|c| matches!(c,Capability::Atom(atom) if atom.eq_ignore_ascii_case("IDLE")));
        }
        Ok(())
    })?;
    if !supported {
        logout(stream);
        return Ok(IdleResult::Unsupported);
    }
    command(stream, b"QM4 EXAMINE \"INBOX\"\r\n")?;
    until_done(stream, "QM4", |_| Ok(()))?;
    command(stream, b"QM5 IDLE\r\n")?;
    if !matches!(response(stream, true)?, Response::Continue(_)) {
        return Err(CheckError::Protocol);
    }
    crate::accounts::idle_deadline(stream, timeout)?;
    for _ in 0..256 {
        match response(stream, true) {
            Ok(
                Response::Expunge(_)
                | Response::Fetch(_, _)
                | Response::MailboxData(MailboxDatum::Exists(_)),
            ) => {
                command(stream, b"DONE\r\n")?;
                until_done(stream, "QM5", |_| Ok(()))?;
                logout(stream);
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
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
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
                if matches!(mode, 3 | 5) {
                    r.get_mut().write_all(b"* 3 EX").unwrap();
                }
                if mode >= 4 {
                    signal.store(true, Ordering::Release);
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
        let started = std::time::Instant::now();
        let result = watch_using(
            &settings,
            "synthetic",
            &tls,
            if mode >= 4 {
                Duration::from_secs(15)
            } else {
                Duration::from_millis(80)
            },
            &cancel,
        );
        if mode >= 4 {
            eprintln!("IDLE cancellation mode={mode}: {:?}", started.elapsed());
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "waited for IDLE timeout"
            );
        }
        worker.join().unwrap();
        result
    }
    #[test]
    fn shutdown_cancels_quiet_and_partial_idle_without_waiting_for_timeout() {
        assert_eq!(fixture(4), Err(CheckError::Cancelled));
        assert_eq!(fixture(5), Err(CheckError::Cancelled));
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
