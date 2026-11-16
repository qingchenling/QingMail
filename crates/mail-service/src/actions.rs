//! 单账号在线邮件操作：标记已读/星标、移动归档/删除。P3 才做持久离线队列。
use crate::accounts::{CheckError, ImapStream, authenticated, quoted, response};
use crate::sync::{command, logout, until_done};
use imap_proto::{Response, ResponseCode, Status};
use mail_core::ServerSettings;

const MAX_UIDS: usize = 500;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlagChange {
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveOutcome {
    /// 服务器完成 MOVE。
    Moved,
    /// 服务器不支持 MOVE；已 COPY 并标记删除，但后续 EXPUNGE 未确认。
    CopiedOnly,
}

fn validate(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    uids: &[u32],
) -> Result<(), CheckError> {
    if folder.is_empty()
        || folder.len() > 4096
        || folder.chars().any(char::is_control)
        || uids.is_empty()
        || uids.len() > MAX_UIDS
        || uids.contains(&0)
        || server.host.is_empty()
        || server.port == 0
        || password.is_empty()
    {
        return Err(CheckError::Invalid);
    }
    Ok(())
}
fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}
fn select(stream: &mut ImapStream, folder: &str, expected_epoch: u32) -> Result<(), CheckError> {
    let mut command_bytes = b"QM3 SELECT ".to_vec();
    quoted(&mut command_bytes, folder);
    command_bytes.extend_from_slice(b"\r\n");
    command(stream, &command_bytes)?;
    let mut epoch = None;
    let mut tagged_ok = false;
    for _ in 0..64 {
        match response(stream, true)? {
            Response::Done { tag, status, .. } => {
                if tag.as_bytes() != b"QM3" {
                    return Err(CheckError::Protocol);
                }
                tagged_ok = status == Status::Ok;
                break;
            }
            Response::Data {
                status: Status::Bye,
                ..
            }
            | Response::Continue(_) => {
                return Err(CheckError::Protocol);
            }
            Response::Data { outcome, .. } => {
                if let Some(ResponseCode::UidValidity(value)) = outcome.code
                    && (value == 0 || epoch.replace(value).is_some_and(|old| old != value))
                {
                    return Err(CheckError::Protocol);
                }
            }
            _ => {}
        }
    }
    if !tagged_ok {
        return Err(CheckError::Protocol);
    }
    if epoch != Some(expected_epoch) {
        return Err(CheckError::EpochChanged);
    }
    Ok(())
}

/// STORE 只接受已更新的本地缓存之后调用；失败时调用方不得改缓存。
pub fn store_flags(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    expected_epoch: u32,
    uids: &[u32],
    change: FlagChange,
) -> Result<(), CheckError> {
    store_flags_using(
        server,
        password,
        folder,
        expected_epoch,
        uids,
        change,
        &tls()?,
    )
}
pub(crate) fn store_flags_using(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    expected_epoch: u32,
    uids: &[u32],
    change: FlagChange,
    connector: &native_tls::TlsConnector,
) -> Result<(), CheckError> {
    validate(server, password, folder, uids)?;
    if change.seen.is_none() && change.flagged.is_none() {
        return Err(CheckError::Invalid);
    }
    let mut stream = authenticated(server, password, connector)?;
    let result = (|| {
        select(&mut stream, folder, expected_epoch)?;
        let set = uid_set(uids);
        for (add, names) in [
            (
                true,
                [
                    ("\\Seen", change.seen == Some(true)),
                    ("\\Flagged", change.flagged == Some(true)),
                ],
            ),
            (
                false,
                [
                    ("\\Seen", change.seen == Some(false)),
                    ("\\Flagged", change.flagged == Some(false)),
                ],
            ),
        ] {
            let flags = names
                .iter()
                .filter(|(_, enabled)| *enabled)
                .map(|(name, _)| *name)
                .collect::<Vec<_>>();
            if flags.is_empty() {
                continue;
            }
            command(
                &mut stream,
                format!(
                    "QM4 UID STORE {set} {}FLAGS.SILENT ({})\r\n",
                    if add { "+" } else { "-" },
                    flags.join(" ")
                )
                .as_bytes(),
            )?;
            until_done(&mut stream, "QM4", |_| Ok(()))?;
        }
        Ok(())
    })();
    logout(&mut stream);
    result
}

/// 优先 UID MOVE；服务器不支持时退化为 COPY + \Deleted + UID EXPUNGE。
pub fn move_messages(
    server: &ServerSettings,
    password: &str,
    source: &str,
    expected_epoch: u32,
    uids: &[u32],
    destination: &str,
) -> Result<MoveOutcome, CheckError> {
    move_messages_using(
        server,
        password,
        source,
        expected_epoch,
        uids,
        destination,
        &tls()?,
    )
}
pub(crate) fn move_messages_using(
    server: &ServerSettings,
    password: &str,
    source: &str,
    expected_epoch: u32,
    uids: &[u32],
    destination: &str,
    connector: &native_tls::TlsConnector,
) -> Result<MoveOutcome, CheckError> {
    validate(server, password, source, uids)?;
    if destination.is_empty()
        || destination.len() > 4096
        || destination.chars().any(char::is_control)
    {
        return Err(CheckError::Invalid);
    }
    let mut stream = authenticated(server, password, connector)?;
    let result = (|| {
        select(&mut stream, source, expected_epoch)?;
        let set = uid_set(uids);
        let mut move_command = b"QM4 UID MOVE ".to_vec();
        move_command.extend_from_slice(set.as_bytes());
        move_command.push(b' ');
        quoted(&mut move_command, destination);
        move_command.extend_from_slice(b"\r\n");
        command(&mut stream, &move_command)?;
        match tagged(&mut stream, "QM4")? {
            Status::Ok => return Ok(MoveOutcome::Moved),
            Status::No | Status::Bad => {}
            _ => return Err(CheckError::Protocol),
        }
        // 退化路径：复制成功后标记删除，再尝试 UID EXPUNGE。
        let mut copy = b"QM4 UID COPY ".to_vec();
        copy.extend_from_slice(set.as_bytes());
        copy.push(b' ');
        quoted(&mut copy, destination);
        copy.extend_from_slice(b"\r\n");
        command(&mut stream, &copy)?;
        if tagged(&mut stream, "QM4")? != Status::Ok {
            return Err(CheckError::Protocol);
        }
        command(
            &mut stream,
            format!("QM5 UID STORE {set} +FLAGS.SILENT (\\Deleted)\r\n").as_bytes(),
        )?;
        until_done(&mut stream, "QM5", |_| Ok(()))?;
        command(&mut stream, format!("QM6 UID EXPUNGE {set}\r\n").as_bytes())?;
        match tagged(&mut stream, "QM6")? {
            Status::Ok => Ok(MoveOutcome::Moved),
            // 副本已存在且源已标记删除，但没有确认真正移除。
            _ => Ok(MoveOutcome::CopiedOnly),
        }
    })();
    logout(&mut stream);
    result
}

fn tagged(stream: &mut ImapStream, tag: &str) -> Result<Status, CheckError> {
    for _ in 0..64 {
        match response(stream, true)? {
            Response::Done {
                tag: received,
                status,
                ..
            } => {
                return if received.as_bytes() == tag.as_bytes() {
                    Ok(status)
                } else {
                    Err(CheckError::Protocol)
                };
            }
            Response::Data {
                status: Status::Bye,
                ..
            }
            | Response::Continue(_) => {
                return Err(CheckError::Protocol);
            }
            _ => {}
        }
    }
    Err(CheckError::Limit)
}
fn tls() -> Result<native_tls::TlsConnector, CheckError> {
    native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()
        .map_err(|_| CheckError::Tls)
}
#[cfg(test)]
#[path = "actions_tests.rs"]
mod tests;
