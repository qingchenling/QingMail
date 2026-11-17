//! SMTP 接受后把同一份冻结 MIME 追加到 IMAP 已发送文件夹。
//!
//! 只处理副本：APPEND 前先按冻结的 Message-ID 查重，因此“结果不确定”后重试
//! 不会产生第二份副本；任何分支都不重发邮件（ADR-0005）。
use crate::accounts::{self, CheckError, ImapStream, authenticated, io_error, quoted, response};
use imap_proto::{MailboxDatum, Response, ResponseCode, Status, rfc4315::UidSetMember};
use mail_core::{RemoteFolder, ServerSettings};
use std::{
    collections::BTreeSet,
    io::Write,
    time::{Duration, Instant},
};

/// 与发送构建层的最终 MIME 上限一致；超过时不尝试 APPEND。
pub const MAX_COPY_BYTES: usize = 40 * 1024 * 1024;
/// 连接与定位/查重阶段的总期限；正文字节发出后另给有界响应窗口。
pub const COPY_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyError {
    Invalid,
    NoSentFolder,
    Connection,
    Timeout,
    Tls,
    Authentication,
    Protocol,
    Limit,
    /// 查重失败：无法排除已存在副本时不追加，避免重复。
    SearchFailed,
    /// 服务器明确拒绝 EXAMINE 或 APPEND（含过早的 NO/BAD）。
    Rejected,
    /// 正文字节可能已被服务器接收；只能由用户确认后重试，重试前会先查重。
    ResultUnknown,
}
impl CopyError {
    /// 存入发送任务的固定脱敏诊断代码，不含密码或服务器原始响应。
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "copy_invalid",
            Self::NoSentFolder => "copy_no_sent_folder",
            Self::Connection => "copy_connection_failed",
            Self::Timeout => "copy_timeout",
            Self::Tls => "copy_tls_failed",
            Self::Authentication => "copy_auth_failed",
            Self::Protocol => "copy_protocol_failed",
            Self::Limit => "copy_limit",
            Self::SearchFailed => "copy_search_failed",
            Self::Rejected => "copy_append_rejected",
            Self::ResultUnknown => "copy_result_unknown",
        }
    }
    pub fn is_uncertain(self) -> bool {
        self == Self::ResultUnknown
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopiedMessage {
    pub folder: String,
    pub uidvalidity: u32,
    /// 服务器未提供 APPENDUID 时为 None；不伪造 UIDPLUS 能力。
    pub uid: Option<u32>,
    /// 查重发现同一 Message-ID 已存在，没有再次 APPEND。
    pub existed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyResult {
    Saved(CopiedMessage),
    Failed(CopyError),
}

pub(crate) fn map_error(error: CheckError) -> CopyError {
    match error {
        CheckError::Invalid => CopyError::Invalid,
        CheckError::Dns | CheckError::Connect => CopyError::Connection,
        CheckError::Timeout => CopyError::Timeout,
        CheckError::Tls => CopyError::Tls,
        CheckError::Authentication => CopyError::Authentication,
        CheckError::Protocol
        | CheckError::EpochChanged
        | CheckError::Cancelled
        | CheckError::Storage => CopyError::Protocol,
        CheckError::Limit => CopyError::Limit,
    }
}

/// 单个 ASCII 邮箱形态的消息标识；与发送构建层一致，避免把用户输入拼进命令。
fn valid_message_id(value: &str) -> bool {
    let Some(inner) = value
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
    else {
        return false;
    };
    inner.len() <= 200
        && inner.matches('@').count() == 1
        && !inner.starts_with('@')
        && !inner.ends_with('@')
        && inner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@.-_".contains(&b))
}
fn valid_request(server: &ServerSettings, password: &str, message_id: &str, raw: &[u8]) -> bool {
    !server.host.is_empty()
        && server.host.len() <= 253
        && !server.host.contains(['/', '@', '?', '#'])
        && !server
            .host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        && server.port != 0
        && !server.username.is_empty()
        && server.username.len() <= 1024
        && !server.username.chars().any(char::is_control)
        && !password.is_empty()
        && password.len() <= 16384
        && !password.chars().any(char::is_control)
        && !raw.is_empty()
        && raw.len() <= MAX_COPY_BYTES
        && valid_message_id(message_id)
}

/// 不发送邮件；只把调用者提供的冻结字节追加到已发送文件夹。
pub fn save_copy(
    server: &ServerSettings,
    password: &str,
    message_id: &str,
    raw: &[u8],
    preferred_folder: Option<&str>,
) -> CopyResult {
    let connector = match native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()
    {
        Ok(connector) => connector,
        Err(_) => return CopyResult::Failed(CopyError::Tls),
    };
    save_copy_using(
        server,
        password,
        message_id,
        raw,
        preferred_folder,
        &connector,
        Instant::now() + COPY_TOTAL_TIMEOUT,
    )
}
fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
}
/// 为下一阶段设置读取预算；阶段之间检查总期限。
fn phase(stream: &mut ImapStream, deadline: Instant) -> Result<(), CopyError> {
    let left = remaining(deadline).ok_or(CopyError::Timeout)?;
    stream
        .get_mut()
        .reset_budget(1024 * 1024, left.as_secs().max(1));
    Ok(())
}
pub(crate) fn save_copy_using(
    server: &ServerSettings,
    password: &str,
    message_id: &str,
    raw: &[u8],
    preferred_folder: Option<&str>,
    tls: &native_tls::TlsConnector,
    deadline: Instant,
) -> CopyResult {
    if !valid_request(server, password, message_id, raw) {
        return CopyResult::Failed(CopyError::Invalid);
    }
    if remaining(deadline).is_none() {
        return CopyResult::Failed(CopyError::Timeout);
    }
    let mut stream = match authenticated(server, password, tls) {
        Ok(stream) => stream,
        Err(error) => return CopyResult::Failed(map_error(error)),
    };
    // 先完成全部网络判断，再无条件尝试 LOGOUT；失败路径不产生半边新状态。
    let result = copy_over(&mut stream, message_id, raw, preferred_folder, deadline);
    logout(&mut stream);
    match result {
        Ok(result) => result,
        Err(error) => CopyResult::Failed(error),
    }
}
fn copy_over(
    stream: &mut ImapStream,
    message_id: &str,
    raw: &[u8],
    preferred_folder: Option<&str>,
    deadline: Instant,
) -> Result<CopyResult, CopyError> {
    phase(stream, deadline)?;
    command(stream, b"QM3 LIST \"\" \"*\"\r\n")?;
    let folders = accounts::imap_done(stream, "QM3", false, true).map_err(map_error)?;
    let Some(folder) = choose_sent_folder(&folders, preferred_folder) else {
        return Ok(CopyResult::Failed(CopyError::NoSentFolder));
    };
    phase(stream, deadline)?;
    let epoch = examine(stream, &folder)?;
    // 查重在任何 APPEND 之前；查重失败时不追加，避免产生第二份副本。
    phase(stream, deadline)?;
    if let Some(uid) = search_existing(stream, epoch, message_id)? {
        return Ok(CopyResult::Saved(CopiedMessage {
            folder,
            uidvalidity: epoch,
            uid: Some(uid),
            existed: true,
        }));
    }
    phase(stream, deadline)?;
    Ok(CopyResult::Saved(append(
        stream, &folder, epoch, raw, deadline,
    )?))
}

/// 优先沿用上次记录的文件夹；否则按已知“已发送”名称与叶子名做确定性选择。
pub fn choose_sent_folder(folders: &[RemoteFolder], preferred: Option<&str>) -> Option<String> {
    crate::folders::choose(
        folders,
        preferred,
        SENT_NAMES,
        &["sent ", "sent-", "已发送"],
    )
}

const SENT_NAMES: &[&str] = &[
    "sent",
    "sent items",
    "sent messages",
    "sent mail",
    "sent-mail",
    "sent_items",
    "gesendet",
    "envoyés",
    "enviados",
    "inviata",
    "отправленные",
    "送信済み",
    "보낸편지함",
    "已发送邮件",
    "已寄出",
    "寄件备份",
    "寄件備份",
];

fn command(stream: &mut ImapStream, bytes: &[u8]) -> Result<(), CopyError> {
    stream
        .get_mut()
        .write_all(bytes)
        .and_then(|()| stream.get_mut().flush())
        .map_err(|error| map_error(io_error(error)))
}
fn logout(stream: &mut ImapStream) {
    let _ = command(stream, b"QM9 LOGOUT\r\n");
}

pub(crate) fn examine(stream: &mut ImapStream, folder: &str) -> Result<u32, CopyError> {
    let mut command_bytes = b"QM4 EXAMINE ".to_vec();
    quoted(&mut command_bytes, folder);
    command_bytes.extend_from_slice(b"\r\n");
    command(stream, &command_bytes)?;
    let mut epoch = None;
    for _ in 0..64 {
        match response(stream, true).map_err(map_error)? {
            Response::Done { tag, status, .. } => {
                if tag.as_bytes() != b"QM4" {
                    return Err(CopyError::Protocol);
                }
                if status != Status::Ok {
                    return Err(CopyError::Rejected);
                }
                return epoch.ok_or(CopyError::Protocol);
            }
            Response::Data {
                status: Status::Bye,
                ..
            }
            | Response::Continue(_) => return Err(CopyError::Connection),
            Response::Data { outcome, .. } => {
                if let Some(ResponseCode::UidValidity(value)) = outcome.code
                    && (value == 0 || epoch.replace(value).is_some_and(|old| old != value))
                {
                    return Err(CopyError::Protocol);
                }
            }
            _ => {}
        }
    }
    Err(CopyError::Limit)
}

/// 只读查重：返回同一 Message-ID 的最小 UID。
pub(crate) fn search_existing(
    stream: &mut ImapStream,
    epoch: u32,
    message_id: &str,
) -> Result<Option<u32>, CopyError> {
    let mut command_bytes = b"QM5 UID SEARCH HEADER Message-ID ".to_vec();
    quoted(&mut command_bytes, message_id);
    command_bytes.extend_from_slice(b"\r\n");
    command(stream, &command_bytes)?;
    let mut found = BTreeSet::new();
    let mut saw_search = false;
    for _ in 0..64 {
        match response(stream, true).map_err(map_error)? {
            Response::Done { tag, status, .. } => {
                if tag.as_bytes() != b"QM5" {
                    return Err(CopyError::Protocol);
                }
                if status != Status::Ok {
                    return Err(CopyError::SearchFailed);
                }
                if !saw_search {
                    return Err(CopyError::SearchFailed);
                }
                return Ok(found.first().copied());
            }
            Response::Data {
                status: Status::Bye,
                ..
            }
            | Response::Continue(_) => return Err(CopyError::Protocol),
            Response::Data { outcome, .. } => {
                if let Some(ResponseCode::UidValidity(value)) = outcome.code
                    && value != epoch
                {
                    return Err(CopyError::Protocol);
                }
            }
            Response::MailboxData(MailboxDatum::Search(values)) => {
                if saw_search || values.len() > 10000 || values.contains(&0) {
                    return Err(CopyError::Limit);
                }
                saw_search = true;
                found.extend(values.iter().copied());
                if found.len() > 10000 {
                    return Err(CopyError::Limit);
                }
            }
            _ => {}
        }
    }
    Err(CopyError::Limit)
}

fn append(
    stream: &mut ImapStream,
    folder: &str,
    epoch: u32,
    raw: &[u8],
    deadline: Instant,
) -> Result<CopiedMessage, CopyError> {
    append_flags(stream, folder, epoch, raw, deadline, "\\Seen")
}
pub(crate) fn append_flags(
    stream: &mut ImapStream,
    folder: &str,
    epoch: u32,
    raw: &[u8],
    deadline: Instant,
    flags: &str,
) -> Result<CopiedMessage, CopyError> {
    let mut command_bytes = b"QM6 APPEND ".to_vec();
    quoted(&mut command_bytes, folder);
    command_bytes.extend_from_slice(format!(" ({flags}) {{{}}}\r\n", raw.len()).as_bytes());
    command(stream, &command_bytes)?;
    // 同步字面量：服务器确认前一个正文字节都没有发送，失败是明确未保存。
    let mut ready = false;
    for _ in 0..64 {
        match response(stream, true).map_err(map_error)? {
            Response::Continue(_) => {
                ready = true;
                break;
            }
            Response::Done { tag, .. } => {
                if tag.as_bytes() != b"QM6" {
                    return Err(CopyError::Protocol);
                }
                // 服务器在正文前就拒绝：明确没有副本，不能重试成“不确定”。
                return Err(CopyError::Rejected);
            }
            Response::Data {
                status: Status::Bye,
                ..
            } => return Err(CopyError::Connection),
            _ => {}
        }
    }
    if !ready {
        return Err(CopyError::Limit);
    }
    // 从这里开始服务器可能已接收部分正文；任何中断都只能标为不确定。
    if stream
        .get_mut()
        .write_all(raw)
        .and_then(|()| stream.get_mut().write_all(b"\r\n"))
        .and_then(|()| stream.get_mut().flush())
        .is_err()
    {
        return Err(CopyError::ResultUnknown);
    }
    // 正文字节已发出；给服务器一个有界响应窗口。超时不知道是否已保存，只能标为不确定。
    let left = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_default();
    stream
        .get_mut()
        .reset_budget(1024 * 1024, left.as_secs().clamp(30, 60));
    for _ in 0..64 {
        match response(stream, true) {
            Ok(Response::Done {
                tag,
                status,
                outcome,
            }) => {
                if tag.as_bytes() != b"QM6" {
                    return Err(CopyError::ResultUnknown);
                }
                if status != Status::Ok {
                    // 服务器在完整正文后明确 NO/BAD：按协议没有保存副本，属明确失败。
                    return Err(CopyError::Rejected);
                }
                return Ok(match outcome.code {
                    Some(ResponseCode::AppendUid(uid_epoch, set)) if uid_epoch != 0 => {
                        CopiedMessage {
                            folder: folder.into(),
                            uidvalidity: uid_epoch,
                            uid: first_uid(&set),
                            existed: false,
                        }
                    }
                    // 没有 APPENDUID 仍然成功；不伪造 UID。
                    _ => CopiedMessage {
                        folder: folder.into(),
                        uidvalidity: epoch,
                        uid: None,
                        existed: false,
                    },
                });
            }
            Ok(Response::Data {
                status: Status::Bye,
                ..
            }) => return Err(CopyError::ResultUnknown),
            Ok(_) => {}
            Err(_) => return Err(CopyError::ResultUnknown),
        }
    }
    Err(CopyError::ResultUnknown)
}
fn first_uid(set: &[UidSetMember]) -> Option<u32> {
    match set.first()? {
        UidSetMember::Uid(uid) => Some(*uid).filter(|uid| *uid != 0),
        UidSetMember::UidRange(range) => Some(*range.start()).filter(|uid| *uid != 0),
    }
}

#[cfg(test)]
#[path = "sent_tests.rs"]
mod tests;
