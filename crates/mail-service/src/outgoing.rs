//! 发送前检查与单次SMTP提交。调用者必须先持久化快照和Sending许可；不自动重试。
use crate::accounts::resolve_bounded;
use lettre::{
    Address, Message,
    address::Envelope,
    message::{
        Attachment, Mailbox, MultiPart, SinglePart,
        header::{ContentTransferEncoding, ContentType},
    },
    transport::smtp::{
        authentication::{Credentials, Mechanism},
        client::{SmtpConnection, TlsParameters},
        commands::{Data, Mail, Rcpt},
        extension::ClientId,
    },
};
use mail_core::{ConnectionSecurity, OutgoingSnapshot, ServerSettings, SmtpOutcome};
use std::{
    collections::HashSet,
    time::{Duration, Instant, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    Sender,
    Recipient { field: &'static str, index: usize },
    NoRecipients,
    Subject,
    EmptySubjectNeedsConfirmation,
    Metadata,
    BodyLimit,
    Attachment { index: usize },
    MessageLimit,
}
impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sender => f.write_str("发件地址无效或暂不支持"),
            Self::Recipient { field, index } => {
                write!(f, "请修正{field}中的第{}个邮箱地址", index + 1)
            }
            Self::NoRecipients => f.write_str("至少填写一个收件人、抄送或密送地址"),
            Self::Subject => f.write_str("主题含控制字符或超过上限"),
            Self::EmptySubjectNeedsConfirmation => f.write_str("无主题发送需要明确确认"),
            Self::Metadata => f.write_str("发送快照的日期或邮件标识无效"),
            Self::BodyLimit => f.write_str("正文超过发送上限"),
            Self::Attachment { index } => write!(f, "第{}个附件信息或大小无效", index + 1),
            Self::MessageLimit => f.write_str("邮件或收件人数量超过发送上限"),
        }
    }
}
impl std::error::Error for PrepareError {}

/// 无Clone；一次构建供一次提交。网络信封包含密送，但MIME头中不包含Bcc。
pub struct PreparedMessage {
    raw: Vec<u8>,
    envelope: Envelope,
}
impl PreparedMessage {
    /// 同一份字节可供后续IMAP副本保存；不含SMTP dot-stuffing。
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
}
fn mailbox(value: &str) -> Option<Address> {
    // 首轮发送仅支持独立ASCII邮箱；避免隐式依赖SMTPUTF8能力。
    if !value.is_ascii()
        || value.len() > 320
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
        || value.contains(['<', '>', ',', ';'])
    {
        return None;
    }
    value.parse().ok()
}
/// allow_empty_subject仅在操作者已明确确认无主题发送时为true。
pub fn prepare(
    snapshot: &OutgoingSnapshot,
    allow_empty_subject: bool,
) -> Result<PreparedMessage, PrepareError> {
    let sender = mailbox(&snapshot.sender).ok_or(PrepareError::Sender)?;
    let count = snapshot.headers.to.len() + snapshot.headers.cc.len() + snapshot.headers.bcc.len();
    if count == 0 {
        return Err(PrepareError::NoRecipients);
    }
    if count > 100 {
        return Err(PrepareError::MessageLimit);
    }
    let subject = &snapshot.headers.subject;
    if subject.len() > 4096 || subject.chars().any(char::is_control) {
        return Err(PrepareError::Subject);
    }
    if subject.trim().is_empty() && !allow_empty_subject {
        return Err(PrepareError::EmptySubjectNeedsConfirmation);
    }
    let id = &snapshot.message_id;
    let inner = id
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .ok_or(PrepareError::Metadata)?;
    if inner.len() > 200
        || !inner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@.-_".contains(&b))
        || mailbox(inner).is_none()
    {
        return Err(PrepareError::Metadata);
    }
    let seconds = u64::try_from(snapshot.date_unix).map_err(|_| PrepareError::Metadata)?;
    // 限制到公历9999年以内，避免日期格式化对异常时间溢出。
    if seconds > 253402300799 {
        return Err(PrepareError::Metadata);
    }
    let date = UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .ok_or(PrepareError::Metadata)?;
    if snapshot.plain.len() > 2 * 1024 * 1024 || snapshot.html.len() > 4 * 1024 * 1024 {
        return Err(PrepareError::BodyLimit);
    }
    let mut builder = Message::builder()
        .from(Mailbox::new(None, sender.clone()))
        .date(date)
        .message_id(Some(id.clone()))
        .subject(subject);
    let mut recipients = vec![];
    let mut seen = HashSet::new();
    for (field, values) in [
        ("收件人", &snapshot.headers.to),
        ("抄送", &snapshot.headers.cc),
        ("密送", &snapshot.headers.bcc),
    ] {
        for (index, value) in values.iter().enumerate() {
            let address = mailbox(value).ok_or(PrepareError::Recipient { field, index })?;
            if seen.insert(address.to_string()) {
                recipients.push(address.clone());
            }
            // 密送只进信封；不通过Bcc头再删除，避免副本泄露。
            if field == "收件人" {
                builder = builder.to(Mailbox::new(None, address));
            } else if field == "抄送" {
                builder = builder.cc(Mailbox::new(None, address));
            }
        }
    }
    let envelope =
        Envelope::new(Some(sender), recipients).map_err(|_| PrepareError::NoRecipients)?;
    let boundary = inner.split_once('@').ok_or(PrepareError::Metadata)?.0;
    // 持久任务ID当前32位；为其他调用者也限制RFC 2046的70字符boundary上限。
    if boundary.len() > 40 {
        return Err(PrepareError::Metadata);
    }
    let alternative = MultiPart::alternative()
        .boundary(format!("qingmail-{boundary}-alternative"))
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .header(ContentTransferEncoding::Base64)
                .body(snapshot.plain.clone()),
        )
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .header(ContentTransferEncoding::Base64)
                .body(snapshot.html.clone()),
        );
    let mut mixed = MultiPart::mixed()
        .boundary(format!("qingmail-{boundary}-mixed"))
        .multipart(alternative);
    if snapshot.attachments.len() > 32 {
        return Err(PrepareError::MessageLimit);
    }
    let mut total = 0usize;
    let mut ids = HashSet::new();
    for (index, attachment) in snapshot.attachments.iter().enumerate() {
        let info = &attachment.info;
        total = total.saturating_add(attachment.bytes.len());
        if total > 25 * 1024 * 1024
            || info.size as usize != attachment.bytes.len()
            || !ids.insert(&info.id)
            || info.name.is_empty()
            || info.name.len() > 1024
            || info.name.chars().any(char::is_control)
            || info.name.contains(['/', '\\'])
            || matches!(info.name.as_str(), "." | "..")
            || info.media_type.len() > 128
            || !info.media_type.contains('/')
            || !info
                .media_type
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/!#$&^_.+-".contains(&b))
        {
            return Err(PrepareError::Attachment { index });
        }
        let kind =
            ContentType::parse(&info.media_type).map_err(|_| PrepareError::Attachment { index })?;
        mixed = mixed
            .singlepart(Attachment::new(info.name.clone()).body(attachment.bytes.clone(), kind));
    }
    let message = builder
        .envelope(envelope.clone())
        .multipart(mixed)
        .map_err(|_| PrepareError::Metadata)?;
    let raw = message.formatted();
    if raw.len() > 40 * 1024 * 1024 || raw.split(|b| *b == b'\n').any(|line| line.len() > 999) {
        return Err(PrepareError::MessageLimit);
    }
    // 本轮不协商8BITMIME；库负责Unicode标题/文件名编码和正文传输编码。
    if !raw.is_ascii() {
        return Err(PrepareError::Metadata);
    }
    Ok(PreparedMessage { raw, envelope })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryDetail {
    Accepted,
    Configuration,
    Connection,
    Tls,
    Authentication,
    EnvelopeRejected,
    DataRejected,
    ResultUnknown,
}
pub struct Delivery {
    pub outcome: SmtpOutcome,
    pub detail: DeliveryDetail,
}
fn failed(detail: DeliveryDetail) -> Delivery {
    Delivery {
        outcome: SmtpOutcome::DefinitelyRejected,
        detail,
    }
}

/// 整次 SMTP 提交的独立期限；DNS 解析另有 10 秒上限。超时在 DATA 前是明确失败，
/// DATA 中或之后只能是“结果不确定”，不能误报为未发送。
pub const SMTP_TOTAL_TIMEOUT: Duration = Duration::from_secs(180);

/// 只提交一次；仅能由明确发送操作触发，调用前必须已获得持久化Sending许可。
/// 不接收“跳过证书验证”选项，不保存密码或返回服务器原始响应。
pub fn deliver(server: &ServerSettings, password: &str, message: PreparedMessage) -> Delivery {
    let Ok(tls) = TlsParameters::new(server.host.clone()) else {
        return failed(DeliveryDetail::Tls);
    };
    deliver_using(
        server,
        password,
        message,
        &tls,
        Instant::now() + SMTP_TOTAL_TIMEOUT,
    )
}
fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
}
fn deliver_using(
    server: &ServerSettings,
    password: &str,
    message: PreparedMessage,
    tls: &TlsParameters,
    deadline: Instant,
) -> Delivery {
    if server.host.is_empty()
        || server.host.len() > 253
        || server
            .host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || server.host.contains(['/', '@', '?', '#'])
        || server.port == 0
        || server.username.is_empty()
        || server.username.len() > 1024
        || server.username.chars().any(char::is_control)
        || password.is_empty()
        || password.len() > 16384
    {
        return failed(DeliveryDetail::Configuration);
    }
    let hello = ClientId::default();
    // 先限时解析，再把 IP 交给 lettre；TLS 域名仍来自 TlsParameters，证书校验不变。
    let addresses = match resolve_bounded(&server.host, server.port, Duration::from_secs(10)) {
        Ok(addresses) => addresses,
        Err(_) => return failed(DeliveryDetail::Connection),
    };
    let Some(left) = remaining(deadline) else {
        return failed(DeliveryDetail::Connection);
    };
    // 逐个尝试解析结果：localhost 可能先返回不可达的 IPv6 地址。
    let mut connected = None;
    for address in addresses.into_iter().take(8) {
        match SmtpConnection::connect(
            address,
            Some(left.min(Duration::from_secs(10))),
            &hello,
            if server.security == ConnectionSecurity::Tls {
                Some(tls)
            } else {
                None
            },
            None,
        ) {
            Ok(conn) => {
                connected = Some(conn);
                break;
            }
            Err(_) => continue,
        }
    }
    let Some(mut conn) = connected else {
        return failed(DeliveryDetail::Connection);
    };
    let Some(left) = remaining(deadline) else {
        return failed(DeliveryDetail::Connection);
    };
    if conn.set_timeout(Some(left)).is_err() {
        return failed(DeliveryDetail::Connection);
    }
    if server.security == ConnectionSecurity::StartTls && conn.starttls(tls, &hello).is_err() {
        return failed(DeliveryDetail::Tls);
    }
    if !conn.is_encrypted() {
        return failed(DeliveryDetail::Tls);
    }
    match conn.auth(
        &[Mechanism::Plain, Mechanism::Login],
        &Credentials::new(server.username.clone(), password.to_owned()),
    ) {
        Ok(response) if response.has_code(235) => {}
        _ => return failed(DeliveryDetail::Authentication),
    }
    // 在所有收件人均获接受前不发DATA，部分拒绝不会导致部分投递。
    if remaining(deadline).is_none() {
        return failed(DeliveryDetail::Connection);
    }
    match conn.command(Mail::new(message.envelope.from().cloned(), vec![])) {
        Ok(r) if r.has_code(250) => {}
        _ => return failed(DeliveryDetail::EnvelopeRejected),
    }
    for recipient in message.envelope.to() {
        if remaining(deadline).is_none() {
            return failed(DeliveryDetail::Connection);
        }
        match conn.command(Rcpt::new(recipient.clone(), vec![])) {
            Ok(r) if r.has_code(250) || r.has_code(251) => {}
            _ => return failed(DeliveryDetail::EnvelopeRejected),
        }
    }
    if remaining(deadline).is_none() {
        return failed(DeliveryDetail::Connection);
    }
    match conn.command(Data) {
        Ok(r) if r.has_code(354) => {}
        _ => return failed(DeliveryDetail::DataRejected),
    }
    // 354 之后一个正文字节都还没发送；期限已到仍是明确未投递。
    let Some(left) = remaining(deadline) else {
        return failed(DeliveryDetail::Connection);
    };
    let _ = conn.set_timeout(Some(left));
    // 从正文提交开始，任何非明确拒绝/接受的结果均保守标记Uncertain。
    // lettre的DATA终止器自带CRLF；去掉MIME末尾的一组，避免投递正文多出空行。
    let payload = message.raw.strip_suffix(b"\r\n").unwrap_or(&message.raw);
    let result = match conn.message(payload) {
        Ok(r) if r.has_code(250) => Delivery {
            outcome: SmtpOutcome::Accepted,
            detail: DeliveryDetail::Accepted,
        },
        Err(e) if e.is_permanent() || e.is_transient() => failed(DeliveryDetail::DataRejected),
        _ => Delivery {
            outcome: SmtpOutcome::Uncertain,
            detail: DeliveryDetail::ResultUnknown,
        },
    };
    // QUIT失败不能推翻已确认接受，不进行任何重连或重发。
    let _ = conn.quit();
    result
}

#[cfg(test)]
#[path = "outgoing_tests.rs"]
mod tests;
