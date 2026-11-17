//! 服务器草稿只读入口：特殊用途识别不猜名称；扫描与原件获取不改远端。
use crate::accounts::{self, CheckError, authenticated, quoted};
use crate::sync::{command, logout, until_done};
use imap_proto::{MailboxDatum, NameAttribute, Response, ResponseCode};
use mail_core::{RemoteFolder, ServerSettings};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct DraftFolders {
    pub folders: Vec<RemoteFolder>,
    pub special: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct DraftIndex {
    pub uidvalidity: u32,
    /// 最新200个UID；没有时间截断。has_more时不能据此推断远端删除。
    pub uids: Vec<u32>,
    pub has_more: bool,
}
fn connector() -> Result<native_tls::TlsConnector, CheckError> {
    native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()
        .map_err(|_| CheckError::Tls)
}
pub fn folders(server: &ServerSettings, password: &str) -> Result<DraftFolders, CheckError> {
    folders_using(server, password, &connector()?)
}
fn folders_using(
    server: &ServerSettings,
    password: &str,
    tls: &native_tls::TlsConnector,
) -> Result<DraftFolders, CheckError> {
    let mut stream = authenticated(server, password, tls)?;
    command(&mut stream, b"QM3 LIST \"\" \"*\"\r\n")?;
    let mut result = DraftFolders {
        folders: vec![],
        special: vec![],
    };
    until_done(&mut stream, "QM3", |response| {
        if let Response::MailboxData(MailboxDatum::List(folder)) = response {
            if result.folders.len() >= 2048 {
                return Err(CheckError::Limit);
            }
            let selectable = !folder.name_attributes.contains(&NameAttribute::NoSelect);
            if selectable && folder.name_attributes.contains(&NameAttribute::Drafts) {
                result.special.push(folder.name.to_string());
            }
            result.folders.push(RemoteFolder {
                display_name: accounts::folder_label(&folder.name),
                name: folder.name.into_owned(),
                delimiter: folder.delimiter.map(|s| s.into_owned()),
                selectable,
            });
        }
        Ok(())
    })?;
    result.special.sort();
    result.special.dedup();
    logout(&mut stream);
    Ok(result)
}
pub fn index(
    server: &ServerSettings,
    password: &str,
    folder: &str,
) -> Result<DraftIndex, CheckError> {
    index_using(server, password, folder, &connector()?)
}
fn index_using(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    tls: &native_tls::TlsConnector,
) -> Result<DraftIndex, CheckError> {
    if !valid_folder(folder) {
        return Err(CheckError::Invalid);
    }
    let mut stream = authenticated(server, password, tls)?;
    let mut bytes = b"QM3 EXAMINE ".to_vec();
    quoted(&mut bytes, folder);
    bytes.extend_from_slice(b"\r\n");
    command(&mut stream, &bytes)?;
    let mut epoch = None;
    until_done(&mut stream, "QM3", |response| {
        if let Response::Data { outcome, .. } = response
            && let Some(ResponseCode::UidValidity(value)) = outcome.code
            && (value == 0 || epoch.replace(value).is_some_and(|old| old != value))
        {
            return Err(CheckError::Protocol);
        }
        Ok(())
    })?;
    let epoch = epoch.ok_or(CheckError::Protocol)?;
    command(&mut stream, b"QM4 UID SEARCH ALL\r\n")?;
    let mut found = None;
    until_done(&mut stream, "QM4", |response| {
        crate::sync::ensure_epoch(&response, epoch)?;
        if let Response::MailboxData(MailboxDatum::Search(values)) = response {
            if found.is_some() || values.len() > 10000 || values.contains(&0) {
                return Err(CheckError::Limit);
            }
            found = Some(values.into_iter().collect::<BTreeSet<_>>());
        }
        Ok(())
    })?;
    let found = found.ok_or(CheckError::Protocol)?;
    let result = DraftIndex {
        uidvalidity: epoch,
        has_more: found.len() > 200,
        uids: found.into_iter().rev().take(200).collect(),
    };
    logout(&mut stream);
    Ok(result)
}
pub fn valid_folder(folder: &str) -> bool {
    !folder.is_empty() && folder.len() <= 4096 && !folder.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
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
    fn special_use_keeps_ambiguity_and_never_guesses_names() {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listen = listener();
        let settings = server(&listen, ConnectionSecurity::Tls);
        let worker = thread::spawn(move || {
            let mut r = BufReader::new(accept.accept(socket(listen)).unwrap());
            r.get_mut().write_all(b"* OK fixture\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM2 LOGIN"));
            r.get_mut().write_all(b"QM2 OK login\r\n").unwrap();
            assert_eq!(line(&mut r), "QM3 LIST \"\" \"*\"\r\n");
            r.get_mut().write_all(b"* LIST () \"/\" Drafts\r\n* LIST (\\Drafts) \"/\" One\r\n* LIST (\\Drafts) \"/\" Two\r\n* LIST (\\Drafts \\Noselect) \"/\" Tree\r\nQM3 OK listed\r\n").unwrap();
            assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
        });
        let result = folders_using(&settings, "test", &tls(&cert)).unwrap();
        worker.join().unwrap();
        assert_eq!(result.special, vec!["One", "Two"]);
        assert_eq!(result.folders.len(), 4);
    }
    #[test]
    fn scan_searches_all_without_age_cutoff_and_keeps_epoch() {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listen = listener();
        let settings = server(&listen, ConnectionSecurity::Tls);
        let worker = thread::spawn(move || {
            let mut r = BufReader::new(accept.accept(socket(listen)).unwrap());
            r.get_mut().write_all(b"* OK fixture\r\n").unwrap();
            let _ = line(&mut r);
            r.get_mut().write_all(b"QM2 OK login\r\n").unwrap();
            assert_eq!(line(&mut r), "QM3 EXAMINE \"One\"\r\n");
            r.get_mut()
                .write_all(b"* OK [UIDVALIDITY 42] epoch\r\nQM3 OK examined\r\n")
                .unwrap();
            assert_eq!(line(&mut r), "QM4 UID SEARCH ALL\r\n");
            r.get_mut()
                .write_all(b"* SEARCH 3 9 7\r\nQM4 OK searched\r\n")
                .unwrap();
            assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
        });
        let result = index_using(&settings, "test", "One", &tls(&cert)).unwrap();
        worker.join().unwrap();
        assert_eq!(result.uidvalidity, 42);
        assert_eq!(result.uids, vec![9, 7, 3]);
        assert!(!result.has_more);
        assert!(index_using(&settings, "test", "a\r\nSELECT INBOX", &tls(&cert)).is_err());
    }
}

/// prepared仅首次执行可APPEND；恢复不确定请求只能查询原标识并逐字节核验。
#[derive(Debug, PartialEq, Eq)]
pub enum UploadResult {
    EpochChanged,
    Verified { uidvalidity: u32, uid: u32 },
    Uncertain,
}
pub fn upload(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    expected_epoch: Option<u32>,
    message_id: &str,
    raw: &[u8],
    probe_only: bool,
) -> Result<UploadResult, crate::sent::CopyError> {
    upload_using(
        server,
        password,
        folder,
        expected_epoch,
        message_id,
        raw,
        probe_only,
        &connector().map_err(crate::sent::map_error)?,
    )
}
#[allow(clippy::too_many_arguments)]
fn upload_using(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    expected_epoch: Option<u32>,
    message_id: &str,
    raw: &[u8],
    probe_only: bool,
    tls: &native_tls::TlsConnector,
) -> Result<UploadResult, crate::sent::CopyError> {
    use crate::sent::{self, CopyError};
    use std::time::{Duration, Instant};
    let Some(local) = message_id
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix("@qingmail.invalid>"))
    else {
        return Err(CopyError::Invalid);
    };
    if !valid_folder(folder)
        || local.len() != 32
        || !local.bytes().all(|b| b.is_ascii_hexdigit())
        || raw.is_empty()
        || raw.len() > 8 * 1024 * 1024
    {
        return Err(CopyError::Invalid);
    }
    let mut stream = authenticated(server, password, tls).map_err(sent::map_error)?;
    let epoch = sent::examine(&mut stream, folder)?;
    if expected_epoch.is_some_and(|old| old != epoch) {
        logout(&mut stream);
        return Ok(UploadResult::EpochChanged);
    }
    let result = (|| {
        if let Some(uid) = sent::search_existing(&mut stream, epoch, message_id)? {
            return Ok(Some(uid));
        }
        if probe_only {
            return Ok(None);
        }
        let appended = sent::append_flags(
            &mut stream,
            folder,
            epoch,
            raw,
            Instant::now() + Duration::from_secs(60),
            "\\Draft \\Seen",
        )?;
        if appended.uidvalidity != epoch {
            return Err(CopyError::ResultUnknown);
        }
        // 即使有APPENDUID也按Message-ID重新查证，再核验内容；不把 tagged OK 当成完整验证。
        sent::search_existing(&mut stream, epoch, message_id).map_err(|_| CopyError::ResultUnknown)
    })();
    logout(&mut stream);
    match result {
        Ok(Some(uid)) => {
            let received = crate::sync::message_using(server, password, folder, epoch, uid, tls)
                .map_err(|_| CopyError::ResultUnknown)?;
            if received != raw {
                return Err(CopyError::ResultUnknown);
            }
            Ok(UploadResult::Verified {
                uidvalidity: epoch,
                uid,
            })
        }
        Ok(None) => Ok(UploadResult::Uncertain),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    use crate::accounts::tests::{acceptor, certificate, line, listener, server, socket};
    use mail_core::ConnectionSecurity;
    use std::{
        io::{BufReader, Read, Write},
        thread,
    };
    fn fixture(mode: &str) -> Result<UploadResult, crate::sent::CopyError> {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listen = listener();
        let settings = server(&listen, ConnectionSecurity::Tls);
        let tls = native_tls::TlsConnector::builder()
            .add_root_certificate(native_tls::Certificate::from_pem(&cert).unwrap())
            .build()
            .unwrap();
        let raw=b"Message-ID: <0123456789abcdef0123456789abcdef@qingmail.invalid>\r\nSubject: draft\r\n\r\nfrozen\r\n";
        let mode = mode.to_owned();
        let probe = mode.starts_with("probe");
        let server_mode = mode.clone();
        let worker = thread::spawn(move || {
            let mut r = BufReader::new(accept.accept(socket(listen.try_clone().unwrap())).unwrap());
            r.get_mut().write_all(b"* OK fixture\r\n").unwrap();
            let _ = line(&mut r);
            r.get_mut().write_all(b"QM2 OK login\r\n").unwrap();
            assert_eq!(line(&mut r), "QM4 EXAMINE \"Drafts\"\r\n");
            r.get_mut()
                .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM4 OK examined\r\n")
                .unwrap();
            assert!(line(&mut r).starts_with("QM5 UID SEARCH HEADER Message-ID "));
            if server_mode == "probe_found" || server_mode == "probe_mismatch" {
                r.get_mut()
                    .write_all(b"* SEARCH 7\r\nQM5 OK searched\r\n")
                    .unwrap();
            } else {
                r.get_mut()
                    .write_all(b"* SEARCH\r\nQM5 OK searched\r\n")
                    .unwrap();
            }
            if server_mode == "probe_missing" {
                assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
                return;
            }
            if !probe {
                assert_eq!(
                    line(&mut r),
                    format!(
                        "QM6 APPEND \"Drafts\" (\\Draft \\Seen) {{{}}}\r\n",
                        raw.len()
                    )
                );
                r.get_mut().write_all(b"+ literal\r\n").unwrap();
                let mut bytes = vec![0; raw.len()];
                r.read_exact(&mut bytes).unwrap();
                assert_eq!(bytes, raw);
                assert_eq!(line(&mut r), "\r\n");
                if server_mode == "lost_response" {
                    return;
                }
                r.get_mut()
                    .write_all(b"QM6 OK [APPENDUID 42 7] appended\r\n")
                    .unwrap();
                assert!(line(&mut r).starts_with("QM5 UID SEARCH HEADER Message-ID "));
                r.get_mut()
                    .write_all(b"* SEARCH 7\r\nQM5 OK searched\r\n")
                    .unwrap();
            }
            assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
            drop(r);
            let mut r = BufReader::new(accept.accept(socket(listen)).unwrap());
            r.get_mut().write_all(b"* OK fixture\r\n").unwrap();
            let _ = line(&mut r);
            r.get_mut().write_all(b"QM2 OK login\r\n").unwrap();
            assert_eq!(line(&mut r), "QM3 EXAMINE \"Drafts\"\r\n");
            r.get_mut()
                .write_all(b"* OK [UIDVALIDITY 42] stable\r\nQM3 OK examined\r\n")
                .unwrap();
            assert_eq!(line(&mut r), "QM4 UID FETCH 7 (UID BODY.PEEK[])\r\n");
            let received = if server_mode == "probe_mismatch" {
                b"Subject: different\r\n\r\nother\r\n".as_slice()
            } else {
                raw.as_slice()
            };
            r.get_mut()
                .write_all(format!("* 1 FETCH (UID 7 BODY[] {{{}}}\r\n", received.len()).as_bytes())
                .unwrap();
            r.get_mut().write_all(received).unwrap();
            r.get_mut().write_all(b")\r\nQM4 OK fetched\r\n").unwrap();
            assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
        });
        let result = upload_using(
            &settings,
            "test",
            "Drafts",
            Some(42),
            "<0123456789abcdef0123456789abcdef@qingmail.invalid>",
            raw,
            probe,
            &tls,
        );
        worker.join().unwrap();
        result
    }
    #[test]
    fn append_draft_flag_and_verify_raw_even_with_appenduid() {
        assert_eq!(
            fixture("append"),
            Ok(UploadResult::Verified {
                uidvalidity: 42,
                uid: 7
            })
        );
    }
    #[test]
    fn lost_response_recovery_is_query_only_and_wrong_content_is_not_success() {
        assert_eq!(
            fixture("lost_response"),
            Err(crate::sent::CopyError::ResultUnknown)
        );
        assert_eq!(fixture("probe_missing"), Ok(UploadResult::Uncertain));
        assert_eq!(
            fixture("probe_found"),
            Ok(UploadResult::Verified {
                uidvalidity: 42,
                uid: 7
            })
        );
        assert_eq!(
            fixture("probe_mismatch"),
            Err(crate::sent::CopyError::ResultUnknown)
        );
    }
}
