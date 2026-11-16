use crate::credentials::Credentials;
use mail_core::{AccountSettings, ConnectionSecurity, ServerSettings};
use std::{
    fmt,
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckError {
    Invalid,
    Dns,
    Connect,
    Timeout,
    Tls,
    Authentication,
    Protocol,
    Limit,
}
impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "配置无效",
            Self::Dns => "无法解析服务器地址",
            Self::Connect => "无法连接服务器",
            Self::Timeout => "连接或响应超时",
            Self::Tls => "TLS 失败：请检查证书、主机名与加密方式",
            Self::Authentication => "认证失败，请检查用户名及应用专用密码",
            Self::Protocol => "服务器协议响应不符合预期",
            Self::Limit => "服务器响应超过检查上限",
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    /// 原始协议名称，后续命令必须使用此值。
    pub name: String,
    pub display_name: String,
    pub delimiter: Option<String>,
    pub selectable: bool,
}
#[derive(Debug)]
pub struct AccountCheck {
    pub imap: Result<Vec<RemoteFolder>, CheckError>,
    pub smtp: Result<(), CheckError>,
}
impl AccountCheck {
    pub fn passed(&self) -> bool {
        self.imap.is_ok() && self.smtp.is_ok()
    }
}
pub fn validate(settings: &AccountSettings) -> Result<(), CheckError> {
    if settings.address.len() > 320
        || settings.address.matches('@').count() != 1
        || settings.address.starts_with('@')
        || settings.address.ends_with('@')
        || settings
            .address
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || settings.display_name.is_empty()
        || settings.display_name.len() > 1024
    {
        return Err(CheckError::Invalid);
    }
    for server in [&settings.imap, &settings.smtp] {
        if server.host.is_empty()
            || server.host.len() > 253
            || server.host.contains(['/', '@', '?', '#'])
            || server
                .host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || server.port == 0
            || server.username.is_empty()
            || server.username.len() > 1024
            || server.username.chars().any(char::is_control)
        {
            return Err(CheckError::Invalid);
        }
    }
    Ok(())
}
/// 调用专用后台任务；无 MAIL FROM/RCPT TO/DATA 或 APPEND。
pub fn check(settings: &AccountSettings, credentials: &Credentials) -> AccountCheck {
    if let Err(e) = validate(settings) {
        return AccountCheck {
            imap: Err(e),
            smtp: Err(e),
        };
    }
    AccountCheck {
        imap: check_imap(&settings.imap, &credentials.imap),
        smtp: check_smtp(&settings.smtp, &credentials.smtp),
    }
}
fn io_error(e: io::Error) -> CheckError {
    if matches!(
        e.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        CheckError::Timeout
    } else if e.kind() == io::ErrorKind::FileTooLarge {
        CheckError::Limit
    } else {
        CheckError::Connect
    }
}
struct Bounded<T> {
    stream: T,
    remaining: usize,
    deadline: Instant,
}
impl<T> Bounded<T> {
    fn new(stream: T) -> Self {
        Self {
            stream,
            remaining: 1024 * 1024,
            deadline: Instant::now() + Duration::from_secs(20),
        }
    }
}
impl<T: Read> Read for Bounded<T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if Instant::now() > self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "check deadline"));
        }
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "check response limit",
            ));
        }
        let length = buffer.len().min(self.remaining);
        let n = self.stream.read(&mut buffer[..length])?;
        self.remaining -= n;
        Ok(n)
    }
}
impl<T: Write> Write for Bounded<T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
fn connect(server: &ServerSettings) -> Result<TcpStream, CheckError> {
    let addresses = (server.host.as_str(), server.port)
        .to_socket_addrs()
        .map_err(|_| CheckError::Dns)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = CheckError::Connect;
    for address in addresses.take(8) {
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            return Err(CheckError::Timeout);
        };
        match TcpStream::connect_timeout(&address, timeout.min(Duration::from_secs(3))) {
            Ok(socket) => {
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .map_err(io_error)?;
                socket
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .map_err(io_error)?;
                return Ok(socket);
            }
            Err(e) => last = io_error(e),
        }
    }
    Err(last)
}
fn check_imap(server: &ServerSettings, password: &str) -> Result<Vec<RemoteFolder>, CheckError> {
    let connector = native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()
        .map_err(|_| CheckError::Tls)?;
    check_imap_with_connector(server, password, &connector)
}
fn check_imap_with_connector(
    server: &ServerSettings,
    password: &str,
    connector: &native_tls::TlsConnector,
) -> Result<Vec<RemoteFolder>, CheckError> {
    let socket = connect(server)?;
    let mut stream = Bounded::new(socket);
    if server.security == ConnectionSecurity::StartTls {
        starttls(&mut stream)?;
    }
    let tls = connector
        .connect(&server.host, stream.stream)
        .map_err(|_| CheckError::Tls)?;
    let mut stream = BufReader::new(Bounded::new(tls));
    if server.security == ConnectionSecurity::Tls
        && !matches!(
            imap_response(&mut stream)?,
            imap_proto::Response::Data {
                status: imap_proto::Status::Ok,
                ..
            }
        )
    {
        return Err(CheckError::Protocol);
    }
    let mut login = zeroize::Zeroizing::new(b"QM2 LOGIN ".to_vec());
    quoted(&mut login, &server.username);
    login.push(b' ');
    quoted(&mut login, password);
    login.extend_from_slice(b"\r\n");
    stream.get_mut().write_all(&login).map_err(io_error)?;
    stream.get_mut().flush().map_err(io_error)?;
    drop(login);
    imap_done(&mut stream, "QM2", true, false)?;
    stream
        .get_mut()
        .write_all(b"QM3 LIST \"\" \"*\"\r\n")
        .map_err(io_error)?;
    stream.get_mut().flush().map_err(io_error)?;
    let folders = imap_done(&mut stream, "QM3", false, true)?;
    // 检查已完成。LOGOUT 不改变邮箱；服务端拒绝/断开不使已完成检查失效。
    let _ = stream.get_mut().write_all(b"QM4 LOGOUT\r\n");
    let _ = stream.get_mut().flush();
    Ok(folders)
}
fn quoted(output: &mut Vec<u8>, text: &str) {
    output.push(b'"');
    for byte in text.bytes() {
        if matches!(byte, b'"' | b'\\') {
            output.push(b'\\');
        }
        output.push(byte);
    }
    output.push(b'"');
}
fn imap_response(stream: &mut impl BufRead) -> Result<imap_proto::Response<'static>, CheckError> {
    let mut bytes = Vec::new();
    loop {
        if stream.read_until(b'\n', &mut bytes).map_err(io_error)? == 0 {
            return Err(CheckError::Protocol);
        }
        if bytes.len() > 1024 * 1024 {
            return Err(CheckError::Limit);
        }
        // 接入检查不接受 FETCH 等复杂消息结构；限制解析面及嵌套风险。
        let prefix = bytes[..bytes.len().min(16)].to_ascii_uppercase();
        if prefix.starts_with(b"* ")
            && ![
                &b"* OK"[..],
                b"* NO",
                b"* BAD",
                b"* BYE",
                b"* CAPABILITY",
                b"* LIST",
            ]
            .iter()
            .any(|p| prefix.starts_with(p))
        {
            return Err(CheckError::Protocol);
        }
        match imap_proto::Response::parse(&bytes) {
            Ok(([], response)) => return Ok(response.into_owned()),
            Err(nom::Err::Incomplete(_)) => continue,
            _ => return Err(CheckError::Protocol),
        }
    }
}
fn imap_done(
    stream: &mut impl BufRead,
    tag: &str,
    authentication: bool,
    list: bool,
) -> Result<Vec<RemoteFolder>, CheckError> {
    use imap_proto::{MailboxDatum, NameAttribute, Response, Status};
    let mut folders = Vec::new();
    for _ in 0..4096 {
        match imap_response(stream)? {
            Response::Done {
                tag: received,
                status,
                ..
            } => {
                if received.as_bytes() != tag.as_bytes() {
                    return Err(CheckError::Protocol);
                }
                return match status {
                    Status::Ok => Ok(folders),
                    Status::No if authentication => Err(CheckError::Authentication),
                    _ => Err(CheckError::Protocol),
                };
            }
            Response::Data {
                status: Status::Bye,
                ..
            }
            | Response::Continue(_) => return Err(CheckError::Protocol),
            Response::MailboxData(MailboxDatum::List(folder)) if list => {
                if folders.len() >= 2048 {
                    return Err(CheckError::Limit);
                }
                folders.push(RemoteFolder {
                    display_name: folder_label(&folder.name),
                    name: folder.name.into_owned(),
                    delimiter: folder.delimiter.map(|s| s.into_owned()),
                    selectable: !folder.name_attributes.contains(&NameAttribute::NoSelect),
                });
            }
            Response::Data { .. } | Response::Capabilities(_) => {}
            _ => return Err(CheckError::Protocol),
        }
    }
    Err(CheckError::Limit)
}

/// RFC 3501 modified UTF-7；显示解码不改变服务端文件夹标识。
fn folder_label(raw: &str) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let decode = || -> Option<String> {
        let mut result = String::new();
        let mut rest = raw;
        while let Some(index) = rest.find('&') {
            result.push_str(&rest[..index]);
            rest = &rest[index + 1..];
            let end = rest.find('-')?;
            if end == 0 {
                result.push('&');
            } else {
                let mut base = rest[..end].replace(',', "/");
                while !base.len().is_multiple_of(4) {
                    base.push('=');
                }
                let bytes = STANDARD.decode(base).ok()?;
                if !bytes.len().is_multiple_of(2) {
                    return None;
                }
                let units = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_be_bytes([b[0], b[1]]))
                    .collect::<Vec<_>>();
                result.push_str(&String::from_utf16(&units).ok()?);
            }
            rest = &rest[end + 1..];
        }
        result.push_str(rest);
        Some(result)
    };
    decode().unwrap_or_else(|| raw.into())
}
fn check_smtp(server: &ServerSettings, password: &str) -> Result<(), CheckError> {
    use lettre::transport::smtp::SmtpTransport;
    let builder = match server.security {
        ConnectionSecurity::Tls => SmtpTransport::relay(&server.host),
        ConnectionSecurity::StartTls => SmtpTransport::starttls_relay(&server.host),
    }
    .map_err(|_| CheckError::Tls)?;
    smtp_using_builder(server, password, builder)
}
fn smtp_using_builder(
    server: &ServerSettings,
    password: &str,
    builder: lettre::transport::smtp::SmtpTransportBuilder,
) -> Result<(), CheckError> {
    use lettre::transport::smtp::authentication::Credentials as SmtpCredentials;
    let transport = builder
        .port(server.port)
        .timeout(Some(Duration::from_secs(5)))
        .credentials(SmtpCredentials::new(
            server.username.clone(),
            password.to_owned(),
        ))
        .build();
    match transport.test_connection() {
        Ok(true) => Ok(()),
        Ok(false) => Err(CheckError::Protocol),
        Err(e) => Err(smtp_error(&e)),
    }
}

fn smtp_error(error: &lettre::transport::smtp::Error) -> CheckError {
    // lettre 可将 TLS 错误嵌在 Connection 错误中；只检查类型与代码，不读取响应文字。
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = source {
        if error.downcast_ref::<native_tls::Error>().is_some() {
            return CheckError::Tls;
        }
        if let Some(e) = error.downcast_ref::<lettre::transport::smtp::Error>() {
            if e.is_tls() {
                return CheckError::Tls;
            }
            if e.is_timeout() {
                return CheckError::Timeout;
            }
            if e.status()
                .is_some_and(|code| matches!(code.to_string().as_str(), "535" | "534"))
            {
                return CheckError::Authentication;
            }
        }
        if let Some(e) = error.downcast_ref::<io::Error>()
            && matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            )
        {
            return CheckError::Timeout;
        }
        source = error.source();
    }
    if error.is_response() || error.is_client() || error.status().is_some() {
        CheckError::Protocol
    } else {
        CheckError::Connect
    }
}

fn starttls(stream: &mut (impl Read + Write)) -> Result<(), CheckError> {
    let greeting = bounded_line(stream)?;
    if !greeting.to_ascii_uppercase().starts_with(b"* OK ") {
        return Err(CheckError::Protocol);
    }
    stream.write_all(b"QM1 STARTTLS\r\n").map_err(io_error)?;
    stream.flush().map_err(io_error)?;
    for _ in 0..64 {
        let line = bounded_line(stream)?;
        let mut tokens = line
            .split(|b| b.is_ascii_whitespace())
            .filter(|s| !s.is_empty());
        let tag = tokens.next().ok_or(CheckError::Protocol)?;
        let status = tokens.next().ok_or(CheckError::Protocol)?;
        if tag == b"QM1" {
            return if status.eq_ignore_ascii_case(b"OK") {
                Ok(())
            } else {
                Err(CheckError::Tls)
            };
        }
        if tag != b"*" || status.eq_ignore_ascii_case(b"BYE") {
            return Err(CheckError::Protocol);
        }
    }
    Err(CheckError::Limit)
}
fn bounded_line(stream: &mut impl Read) -> Result<Vec<u8>, CheckError> {
    let mut line = Vec::new();
    for _ in 0..8192 {
        let mut byte = [0];
        stream.read_exact(&mut byte).map_err(io_error)?;
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            return Ok(line);
        }
    }
    Err(CheckError::Limit)
}

#[cfg(test)]
#[path = "accounts_tests.rs"]
mod tests;
