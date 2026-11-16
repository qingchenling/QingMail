//! 首轮手动只读同步：LIST 与最近90天、每次至多200封的头部索引。
use crate::accounts::{self, CheckError, ImapStream, authenticated, io_error, quoted, response};
use imap_proto::{AttributeValue, MailboxDatum, MessageSection, Response, ResponseCode, Status};
use mail_core::{
    BatchKind, FolderBatch, MessageEnvelope, MessageFlags, RemoteFolder, ServerSettings,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};

fn connector() -> Result<native_tls::TlsConnector, CheckError> {
    native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()
        .map_err(|_| CheckError::Tls)
}
pub fn folders(server: &ServerSettings, password: &str) -> Result<Vec<RemoteFolder>, CheckError> {
    let mut stream = authenticated(server, password, &connector()?)?;
    command(&mut stream, b"QM3 LIST \"\" \"*\"\r\n")?;
    let result = accounts::imap_done(&mut stream, "QM3", false, true)?;
    logout(&mut stream);
    Ok(result)
}
pub fn folder(
    server: &ServerSettings,
    password: &str,
    name: &str,
    before: Option<u32>,
    expected_epoch: Option<u32>,
) -> Result<FolderBatch, CheckError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CheckError::Invalid)?
        .as_secs() as i64;
    folder_using(
        server,
        password,
        name,
        before,
        expected_epoch,
        now,
        &connector()?,
    )
}
fn folder_using(
    server: &ServerSettings,
    password: &str,
    name: &str,
    before: Option<u32>,
    expected_epoch: Option<u32>,
    now: i64,
    tls: &native_tls::TlsConnector,
) -> Result<FolderBatch, CheckError> {
    fetch_folder(
        server,
        password,
        FolderQuery {
            name,
            before,
            expected_epoch,
            kind: BatchKind::Recent,
        },
        now,
        tls,
    )
}
/// 主动加载最近90天之前的历史；按 UID 游标向前取，不冒充最近窗口同步。
pub fn history(
    server: &ServerSettings,
    password: &str,
    name: &str,
    before: Option<u32>,
    expected_epoch: Option<u32>,
) -> Result<FolderBatch, CheckError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CheckError::Invalid)?
        .as_secs() as i64;
    fetch_folder(
        server,
        password,
        FolderQuery {
            name,
            before,
            expected_epoch,
            kind: BatchKind::History,
        },
        now,
        &connector()?,
    )
}
struct FolderQuery<'a> {
    name: &'a str,
    before: Option<u32>,
    expected_epoch: Option<u32>,
    kind: BatchKind,
}
fn fetch_folder(
    server: &ServerSettings,
    password: &str,
    query: FolderQuery<'_>,
    now: i64,
    tls: &native_tls::TlsConnector,
) -> Result<FolderBatch, CheckError> {
    let FolderQuery {
        name,
        before,
        expected_epoch,
        kind,
    } = query;
    if name.is_empty()
        || name.len() > 4096
        || name.chars().any(char::is_control)
        || before == Some(0)
    {
        return Err(CheckError::Invalid);
    }
    let mut stream = authenticated(server, password, tls)?;
    let mut examine = b"QM3 EXAMINE ".to_vec();
    quoted(&mut examine, name);
    examine.extend_from_slice(b"\r\n");
    command(&mut stream, &examine)?;
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
    if before.is_some() && expected_epoch.is_some_and(|old| old != epoch) {
        return Err(CheckError::EpochChanged);
    }
    let cutoff = now.saturating_sub(90 * 86400);
    let date = mail_parser::DateTime::from_timestamp(cutoff);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let search = format!(
        "QM4 UID SEARCH {}{}-{}-{}\r\n",
        if kind == BatchKind::Recent {
            "SINCE "
        } else {
            "BEFORE "
        },
        date.day,
        MONTHS[date.month as usize - 1],
        date.year
    );
    command(&mut stream, search.as_bytes())?;
    let mut uids = BTreeSet::new();
    let mut saw_search = false;
    until_done(&mut stream, "QM4", |response| {
        ensure_epoch(&response, epoch)?;
        if let Response::MailboxData(MailboxDatum::Search(values)) = response {
            if saw_search || values.len() > 10000 || values.contains(&0) {
                return Err(CheckError::Limit);
            }
            saw_search = true;
            uids.extend(values);
            if uids.len() > 10000 {
                return Err(CheckError::Limit);
            }
        }
        Ok(())
    })?;
    if !saw_search {
        return Err(CheckError::Protocol);
    }
    let eligible = uids
        .iter()
        .rev()
        .copied()
        .filter(|uid| before.is_none_or(|before| *uid < before))
        .collect::<Vec<_>>();
    let has_more = eligible.len() > 200;
    let wanted = eligible.into_iter().take(200).collect::<BTreeSet<_>>();
    let mut messages = BTreeMap::new();
    let mut flags = BTreeMap::new();
    if !wanted.is_empty() {
        let set = wanted
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        command(
            &mut stream,
            format!(
                "QM5 UID FETCH {set} (UID FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT FROM DATE)])\r\n"
            )
            .as_bytes(),
        )?;
        until_done(&mut stream, "QM5", |response| {
            ensure_epoch(&response, epoch)?;
            if let Response::Fetch(_, attrs) = response {
                let mut uid = None;
                let mut header = None;
                let mut seen = false;
                let mut flagged = false;
                for attr in attrs {
                    match attr {
                        AttributeValue::Uid(value) => {
                            if uid.replace(value).is_some() {
                                return Err(CheckError::Protocol);
                            }
                        }
                        AttributeValue::Flags(values) => {
                            for value in values {
                                if value.eq_ignore_ascii_case("\\Seen") {
                                    seen = true;
                                } else if value.eq_ignore_ascii_case("\\Flagged") {
                                    flagged = true;
                                }
                            }
                        }
                        AttributeValue::BodySection {
                            section: Some(imap_proto::SectionPath::Full(MessageSection::Header)),
                            index: None,
                            data: Some(data),
                        } => {
                            if header.replace(data).is_some() {
                                return Err(CheckError::Protocol);
                            }
                        }
                        _ => return Err(CheckError::Protocol),
                    }
                }
                if let Some(header) = header {
                    let uid = uid
                        .filter(|uid| wanted.contains(uid))
                        .ok_or(CheckError::Protocol)?;
                    if header.len() > 64 * 1024 {
                        return Err(CheckError::Limit);
                    }
                    let parsed = mail_parser::MessageParser::default()
                        .parse_headers(&header)
                        .ok_or(CheckError::Protocol)?;
                    let sender = parsed
                        .from()
                        .and_then(|list| list.first())
                        .map(|from| match (from.name(), from.address()) {
                            (Some(name), Some(address)) => format!("{name} <{address}>"),
                            (_, Some(address)) => address.to_owned(),
                            (Some(name), _) => name.to_owned(),
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    let envelope = MessageEnvelope {
                        subject: parsed.subject().unwrap_or("").to_owned(),
                        sender,
                        received_at: parsed.date().map(|date| date.to_timestamp()).unwrap_or(0),
                    };
                    if messages.insert(uid, envelope).is_some()
                        || flags.insert(uid, MessageFlags { seen, flagged }).is_some()
                    {
                        return Err(CheckError::Protocol);
                    }
                }
            }
            Ok(())
        })?;
    }
    if messages.len() != wanted.len() || flags.len() != wanted.len() {
        return Err(CheckError::Protocol);
    }
    logout(&mut stream);
    Ok(FolderBatch {
        remote_name: name.into(),
        uidvalidity: epoch,
        since: cutoff,
        has_more,
        complete: before.is_none(),
        kind,
        messages: messages.into_iter().collect(),
        flags,
    })
}
fn ensure_epoch(response: &Response<'_>, epoch: u32) -> Result<(), CheckError> {
    if let Response::Data { outcome, .. } = response
        && let Some(ResponseCode::UidValidity(value)) = outcome.code
        && value != epoch
    {
        return Err(CheckError::EpochChanged);
    }
    Ok(())
}
pub(crate) fn command(stream: &mut ImapStream, bytes: &[u8]) -> Result<(), CheckError> {
    stream.get_mut().write_all(bytes).map_err(io_error)?;
    stream.get_mut().flush().map_err(io_error)
}
pub(crate) fn logout(stream: &mut ImapStream) {
    let _ = command(stream, b"QM9 LOGOUT\r\n");
}
pub(crate) fn until_done(
    stream: &mut ImapStream,
    tag: &str,
    mut item: impl FnMut(Response<'static>) -> Result<(), CheckError>,
) -> Result<(), CheckError> {
    for _ in 0..4096 {
        match response(stream, true)? {
            Response::Done {
                tag: received,
                status,
                ..
            } => {
                return if received.as_bytes() == tag.as_bytes() && status == Status::Ok {
                    Ok(())
                } else {
                    Err(CheckError::Protocol)
                };
            }
            Response::Data {
                status: Status::Bye,
                ..
            }
            | Response::Continue(_) => return Err(CheckError::Protocol),
            Response::Data {
                outcome:
                    imap_proto::Outcome {
                        code: Some(ResponseCode::UidNotSticky),
                        ..
                    },
                ..
            } => return Err(CheckError::Protocol),
            response => item(response)?,
        }
    }
    Err(CheckError::Limit)
}
/// 在调用通用解析器前限制协议括号深度；引号与字面量不参与深度统计。
/// 不完整字面量留给解析器报告 Incomplete，不把正文里的括号当协议嵌套。
#[cfg(test)]
pub(crate) fn guard_response(bytes: &[u8]) -> Result<(), CheckError> {
    guard_response_limit(bytes, 64 * 1024)
}
pub(crate) fn guard_response_limit(bytes: &[u8], literal_max: usize) -> Result<(), CheckError> {
    let mut offset = 0;
    let mut depth = 0;
    let mut quoted = false;
    while offset < bytes.len() {
        match bytes[offset] {
            b'\\' if quoted => {
                offset += 2;
                continue;
            }
            b'"' => quoted = !quoted,
            b'(' if !quoted => {
                depth += 1;
                if depth > 16 {
                    return Err(CheckError::Limit);
                }
            }
            b')' if !quoted => {
                if depth == 0 {
                    return Err(CheckError::Protocol);
                }
                depth -= 1;
            }
            b'{' if !quoted => {
                let rest = &bytes[offset + 1..];
                let Some(end) = rest.iter().position(|b| *b == b'}') else {
                    return Ok(());
                };
                if !rest[..end].iter().all(u8::is_ascii_digit) || end > 9 {
                    return Err(CheckError::Protocol);
                }
                let count = std::str::from_utf8(&rest[..end])
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    .ok_or(CheckError::Protocol)?;
                if count > literal_max {
                    return Err(CheckError::Limit);
                }
                let start = offset + 1 + end + 3;
                if bytes.get(offset + 1 + end + 1..start) != Some(b"\r\n".as_slice()) {
                    return if bytes.len() < start {
                        Ok(())
                    } else {
                        Err(CheckError::Protocol)
                    };
                }
                offset = start.saturating_add(count);
                continue;
            }
            _ => {}
        }
        offset += 1;
    }
    Ok(())
}
#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;

pub fn display_time(timestamp: i64) -> String {
    if timestamp == 0 {
        "未知日期".into()
    } else {
        mail_parser::DateTime::from_timestamp(timestamp).to_rfc3339()
    }
}

/// 明确打开正文时下载完整MIME（包括其中附件）；不设置已读标记。
pub fn message(
    server: &ServerSettings,
    password: &str,
    name: &str,
    epoch: u32,
    uid: u32,
) -> Result<Vec<u8>, CheckError> {
    message_using(server, password, name, epoch, uid, &connector()?)
}
fn message_using(
    server: &ServerSettings,
    password: &str,
    name: &str,
    epoch: u32,
    uid: u32,
    tls: &native_tls::TlsConnector,
) -> Result<Vec<u8>, CheckError> {
    if name.is_empty()
        || name.len() > 4096
        || name.chars().any(char::is_control)
        || uid == 0
        || epoch == 0
    {
        return Err(CheckError::Invalid);
    }
    let mut stream = authenticated(server, password, tls)?;
    let mut examine = b"QM3 EXAMINE ".to_vec();
    quoted(&mut examine, name);
    examine.extend_from_slice(b"\r\n");
    command(&mut stream, &examine)?;
    let mut seen_epoch = false;
    until_done(&mut stream, "QM3", |response| {
        ensure_epoch(&response, epoch)?;
        if let Response::Data { outcome, .. } = response
            && matches!(outcome.code, Some(ResponseCode::UidValidity(_)))
        {
            seen_epoch = true;
        }
        Ok(())
    })?;
    if !seen_epoch {
        return Err(CheckError::Protocol);
    }
    stream.get_mut().reset_budget(9 * 1024 * 1024, 60);
    command(
        &mut stream,
        format!("QM4 UID FETCH {uid} (UID BODY.PEEK[])\r\n").as_bytes(),
    )?;
    let mut raw = None;
    let mut complete = false;
    for _ in 0..64 {
        let item = accounts::response_limited(&mut stream, true, 9 * 1024 * 1024, 8 * 1024 * 1024)?;
        ensure_epoch(&item, epoch)?;
        match item {
            Response::Done { tag, status, .. } => {
                if tag.as_bytes() != b"QM4" || status != Status::Ok {
                    return Err(CheckError::Protocol);
                }
                complete = true;
                break;
            }
            Response::Fetch(_, attrs) => {
                let mut received_uid = None;
                let mut data = None;
                for attr in attrs {
                    match attr {
                        AttributeValue::Uid(value) if received_uid.is_none() => {
                            received_uid = Some(value)
                        }
                        AttributeValue::BodySection {
                            section: None,
                            index: None,
                            data: Some(value),
                        } if data.is_none() => data = Some(value),
                        _ => return Err(CheckError::Protocol),
                    }
                }
                if let Some(data) = data {
                    if received_uid != Some(uid)
                        || raw.is_some()
                        || data.len() > 8 * 1024 * 1024
                        || data.is_empty()
                    {
                        return Err(CheckError::Protocol);
                    }
                    raw = Some(data.into_owned());
                }
            }
            Response::Data {
                status: Status::Ok, ..
            } => {}
            _ => return Err(CheckError::Protocol),
        }
    }
    if !complete {
        return Err(CheckError::Limit);
    }
    let raw = raw.ok_or(CheckError::Protocol)?;
    logout(&mut stream);
    Ok(raw)
}
