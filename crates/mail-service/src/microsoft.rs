//! Microsoft delegated OAuth for native clients: browser + PKCE, bounded loopback callback.
//! Tokens never enter IPC, SQLite, ordinary files or diagnostic strings.
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use mail_core::{AccountSettings, Authentication, ConnectionSecurity, ServerSettings};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::{Zeroize, Zeroizing};

pub const CLIENT_ID: &str = "fa0a5ba2-d5bb-49af-9683-58bc3cb517e7";
pub const SCOPES: &str = "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send";
pub const CALLBACK_PATH: &str = "/oauth2/callback";
pub const MAX_SECRET_BYTES: usize = 128 * 1024;
const LOGIN_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Network,
    Cancelled,
    Expired,
    InvalidCallback,
    Reauthenticate,
    Response,
    Vault,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Configuration => "微软应用配置无效，请检查注册的账号类型、回调地址和邮件协议权限",
            Self::Network => "无法连接微软登录服务，请检查网络后重试",
            Self::Cancelled => "已取消微软登录，原账号与凭据保留",
            Self::Expired => "微软登录已超时，请重新登录",
            Self::InvalidCallback => "登录回调校验失败，请重新登录",
            Self::Reauthenticate => "微软授权已失效或被撤销，请在账号页重新登录",
            Self::Response => "微软未返回有效的邮件授权，请检查应用权限与账号策略",
            Self::Vault => "授权未能保存到系统密钥库，未保存账号",
        })
    }
}
impl std::error::Error for Error {}
pub fn valid_client_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn tenant(kind: Authentication) -> Result<&'static str, Error> {
    match kind {
        Authentication::MicrosoftPersonal => Ok("consumers"),
        Authentication::Microsoft365 => Ok("organizations"),
        _ => Err(Error::Configuration),
    }
}
pub fn settings(name: &str, address: &str, kind: Authentication) -> Result<AccountSettings, Error> {
    tenant(kind)?;
    let value = AccountSettings {
        display_name: if name.trim().is_empty() {
            address.to_owned()
        } else {
            name.trim().to_owned()
        },
        address: address.to_owned(),
        imap: ServerSettings {
            authentication: kind,
            host: "outlook.office365.com".into(),
            port: 993,
            username: address.into(),
            security: ConnectionSecurity::Tls,
        },
        smtp: ServerSettings {
            authentication: kind,
            host: if kind == Authentication::MicrosoftPersonal {
                "smtp-mail.outlook.com"
            } else {
                "smtp.office365.com"
            }
            .into(),
            port: 587,
            username: address.into(),
            security: ConnectionSecurity::StartTls,
        },
    };
    crate::accounts::validate(&value).map_err(|_| Error::Configuration)?;
    Ok(value)
}
/// Prevent sending a Microsoft bearer token to an editable, unrelated mail server.
pub fn valid_servers(settings: &AccountSettings) -> bool {
    let kind = settings.imap.authentication;
    if kind == Authentication::Password {
        return settings.smtp.authentication == kind;
    }
    settings.smtp.authentication == kind
        && valid_imap_server(&settings.imap)
        && valid_smtp_server(&settings.smtp)
        && settings.imap.username == settings.address
        && settings.smtp.username == settings.address
}
pub(crate) fn valid_imap_server(server: &ServerSettings) -> bool {
    server.authentication == Authentication::Password
        || (server.host == "outlook.office365.com"
            && server.port == 993
            && server.security == ConnectionSecurity::Tls)
}
pub(crate) fn valid_smtp_server(server: &ServerSettings) -> bool {
    server.authentication == Authentication::Password
        || (server.host
            == if server.authentication == Authentication::MicrosoftPersonal {
                "smtp-mail.outlook.com"
            } else {
                "smtp.office365.com"
            }
            && server.port == 587
            && server.security == ConnectionSecurity::StartTls)
}
pub fn xoauth2(username: &str, token: &str) -> Result<Zeroizing<String>, Error> {
    if username.is_empty()
        || username.len() > 1024
        || username.chars().any(char::is_control)
        || !valid_token(token)
    {
        return Err(Error::Configuration);
    }
    let raw = Zeroizing::new(format!("user={username}\x01auth=Bearer {token}\x01\x01"));
    Ok(Zeroizing::new(STANDARD.encode(raw.as_bytes())))
}
fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 65536
        && !value.chars().any(|c| c.is_control() || c.is_whitespace())
}
fn now() -> Result<u64, Error> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .map_err(|_| Error::Response)
}
fn random() -> Result<Zeroizing<String>, Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::Configuration)?;
    let text = URL_SAFE_NO_PAD.encode(bytes);
    bytes.zeroize();
    Ok(Zeroizing::new(text))
}
fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}

pub struct Tokens {
    pub client_id: String,
    pub kind: Authentication,
    pub access_token: Zeroizing<String>,
    pub refresh_token: Zeroizing<String>,
    pub expires_at: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTokens {
    client_id: String,
    kind: Authentication,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}
impl Drop for StoredTokens {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}
impl Tokens {
    pub fn needs_refresh(&self) -> bool {
        now().map_or(true, |now| now.saturating_add(120) >= self.expires_at)
    }
    pub fn encode(&self) -> Result<Zeroizing<Vec<u8>>, Error> {
        #[derive(Serialize)]
        struct Saved<'a> {
            client_id: &'a str,
            kind: Authentication,
            access_token: &'a str,
            refresh_token: &'a str,
            expires_at: u64,
        }
        serde_json::to_vec(&Saved {
            client_id: &self.client_id,
            kind: self.kind,
            access_token: &self.access_token,
            refresh_token: &self.refresh_token,
            expires_at: self.expires_at,
        })
        .map(Zeroizing::new)
        .map_err(|_| Error::Response)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_SECRET_BYTES {
            return Err(Error::Response);
        }
        let stored: StoredTokens = serde_json::from_slice(bytes).map_err(|_| Error::Response)?;
        if !valid_client_id(&stored.client_id)
            || tenant(stored.kind).is_err()
            || !valid_token(&stored.access_token)
            || !valid_token(&stored.refresh_token)
            || stored.expires_at == 0
        {
            return Err(Error::Response);
        }
        Ok(Self {
            client_id: stored.client_id.clone(),
            kind: stored.kind,
            access_token: Zeroizing::new(stored.access_token.clone()),
            refresh_token: Zeroizing::new(stored.refresh_token.clone()),
            expires_at: stored.expires_at,
        })
    }
    pub fn refresh(&self) -> Result<Self, Error> {
        exchange(
            &self.client_id,
            self.kind,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &self.refresh_token),
                ("scope", SCOPES),
            ],
            Some(&self.refresh_token),
        )
    }
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    token_type: String,
    expires_in: u64,
}
impl Drop for TokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        if let Some(value) = &mut self.refresh_token {
            value.zeroize();
        }
    }
}
fn parse_tokens(
    bytes: &[u8],
    client_id: &str,
    kind: Authentication,
    previous: Option<&str>,
) -> Result<Tokens, Error> {
    let token: TokenResponse = serde_json::from_slice(bytes).map_err(|_| Error::Response)?;
    let refresh = token
        .refresh_token
        .as_deref()
        .or(previous)
        .ok_or(Error::Response)?;
    if !token.token_type.eq_ignore_ascii_case("Bearer")
        || !valid_token(&token.access_token)
        || !valid_token(refresh)
        || !(1..=86400).contains(&token.expires_in)
    {
        return Err(Error::Response);
    }
    Ok(Tokens {
        client_id: client_id.into(),
        kind,
        access_token: Zeroizing::new(token.access_token.clone()),
        refresh_token: Zeroizing::new(refresh.into()),
        expires_at: now()?.saturating_add(token.expires_in),
    })
}
fn exchange(
    client_id: &str,
    kind: Authentication,
    fields: &[(&str, &str)],
    previous: Option<&str>,
) -> Result<Tokens, Error> {
    let endpoint = format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
        tenant(kind)?
    );
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| Error::Network)?;
    let mut form = vec![("client_id", client_id)];
    form.extend_from_slice(fields);
    let response = client
        .post(endpoint)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(
            Zeroizing::new(
                url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(form)
                    .finish(),
            )
            .to_string(),
        )
        .send()
        .map_err(|_| Error::Network)?;
    let status = response.status();
    let mut bytes = Zeroizing::new(Vec::new());
    response
        .take((MAX_SECRET_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Network)?;
    if bytes.len() > MAX_SECRET_BYTES {
        return Err(Error::Response);
    }
    if !status.is_success() {
        #[derive(Deserialize)]
        struct Failure {
            error: String,
        }
        return Err(
            match serde_json::from_slice::<Failure>(&bytes)
                .ok()
                .map(|v| v.error)
                .as_deref()
            {
                Some("invalid_grant" | "interaction_required" | "consent_required") => {
                    Error::Reauthenticate
                }
                Some("invalid_client" | "unauthorized_client" | "invalid_scope") => {
                    Error::Configuration
                }
                _ => {
                    if status.is_server_error() || status.as_u16() == 429 {
                        Error::Network
                    } else {
                        Error::Response
                    }
                }
            },
        );
    }
    parse_tokens(&bytes, client_id, kind, previous)
}

pub struct Login {
    listener: TcpListener,
    client_id: String,
    kind: Authentication,
    verifier: Zeroizing<String>,
    state: Zeroizing<String>,
    redirect: String,
    pub authorization_url: String,
}
impl Login {
    pub fn begin(client_id: &str, address: &str, kind: Authentication) -> Result<Self, Error> {
        if !valid_client_id(client_id)
            || address.len() > 320
            || address.chars().any(|c| c.is_control() || c.is_whitespace())
            || !address.contains('@')
        {
            return Err(Error::Configuration);
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| Error::Configuration)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| Error::Configuration)?;
        let redirect = format!(
            "http://localhost:{}{CALLBACK_PATH}",
            listener
                .local_addr()
                .map_err(|_| Error::Configuration)?
                .port()
        );
        let verifier = random()?;
        let state = random()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = url::Url::parse(&format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize",
            tenant(kind)?
        ))
        .map_err(|_| Error::Configuration)?;
        url.query_pairs_mut().extend_pairs([
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("response_mode", "query"),
            ("scope", SCOPES),
            ("state", state.as_str()),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("login_hint", address),
            ("prompt", "select_account"),
        ]);
        Ok(Self {
            listener,
            client_id: client_id.into(),
            kind,
            verifier,
            state,
            redirect,
            authorization_url: url.into(),
        })
    }
    pub fn finish(self, cancel: &AtomicBool) -> Result<Tokens, Error> {
        let deadline = Instant::now() + LOGIN_TTL;
        while Instant::now() < deadline && !cancel.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((mut stream, peer)) if peer.ip().is_loopback() => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .map_err(|_| Error::InvalidCallback)?;
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .map_err(|_| Error::InvalidCallback)?;
                    let callback = self.read_callback(&mut stream);
                    let valid = callback.is_ok() || matches!(callback, Err(Error::Cancelled));
                    let text = if valid {
                        "QingMail login received. You can close this tab and return to QingMail."
                    } else {
                        "Invalid login callback."
                    };
                    let reply = format!(
                        "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{text}",
                        if valid { "200 OK" } else { "400 Bad Request" },
                        text.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                    match callback {
                        Ok(code) => {
                            return exchange(
                                &self.client_id,
                                self.kind,
                                &[
                                    ("grant_type", "authorization_code"),
                                    ("code", &code),
                                    ("redirect_uri", &self.redirect),
                                    ("code_verifier", &self.verifier),
                                ],
                                None,
                            );
                        }
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(_) => {}
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(_) => return Err(Error::InvalidCallback),
            }
        }
        Err(if cancel.load(Ordering::Acquire) {
            Error::Cancelled
        } else {
            Error::Expired
        })
    }
    fn read_callback(&self, stream: &mut TcpStream) -> Result<Zeroizing<String>, Error> {
        let mut bytes = Zeroizing::new(Vec::new());
        let mut byte = [0];
        let deadline = Instant::now() + Duration::from_secs(2);
        while bytes.len() < 16384 {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or(Error::InvalidCallback)?;
            stream
                .set_read_timeout(Some(remaining))
                .map_err(|_| Error::InvalidCallback)?;
            stream
                .read_exact(&mut byte)
                .map_err(|_| Error::InvalidCallback)?;
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        if !bytes.ends_with(b"\r\n\r\n") {
            return Err(Error::InvalidCallback);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::InvalidCallback)?;
        self.callback(text)
    }
    fn callback(&self, text: &str) -> Result<Zeroizing<String>, Error> {
        let mut lines = text.split("\r\n");
        let mut request = lines
            .next()
            .ok_or(Error::InvalidCallback)?
            .split_whitespace();
        if request.next() != Some("GET") {
            return Err(Error::InvalidCallback);
        }
        let target = request.next().ok_or(Error::InvalidCallback)?;
        if request.next() != Some("HTTP/1.1")
            || request.next().is_some()
            || !target.starts_with('/')
        {
            return Err(Error::InvalidCallback);
        }
        let expected = url::Url::parse(&self.redirect).map_err(|_| Error::InvalidCallback)?;
        let host = format!(
            "localhost:{}",
            expected.port().ok_or(Error::InvalidCallback)?
        );
        let hosts: Vec<_> = lines
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim())
            .collect();
        if hosts != [host.as_str()] {
            return Err(Error::InvalidCallback);
        }
        let url = url::Url::parse(&format!("http://{host}{target}"))
            .map_err(|_| Error::InvalidCallback)?;
        if url.path() != CALLBACK_PATH {
            return Err(Error::InvalidCallback);
        }
        let pairs: Vec<_> = url.query_pairs().collect();
        let one = |key: &str| {
            let mut values = pairs.iter().filter(|(k, _)| k == key);
            let value = values.next().map(|(_, v)| v.as_ref());
            if values.next().is_some() { None } else { value }
        };
        if !one("state").is_some_and(|s| constant_eq(s, &self.state)) {
            return Err(Error::InvalidCallback);
        }
        if one("error").is_some() {
            return Err(Error::Cancelled);
        }
        let code = one("code").ok_or(Error::InvalidCallback)?;
        if code.is_empty() || code.len() > 8192 || code.chars().any(char::is_control) {
            return Err(Error::InvalidCallback);
        }
        Ok(Zeroizing::new(code.into()))
    }
}

#[cfg(test)]
#[path = "microsoft_tests.rs"]
mod tests;
