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
