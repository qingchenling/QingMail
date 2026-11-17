//! 同 UID Unix socket 客户端；每次请求单独连接，绝不自动重试写操作。
use crate::{
    Command, PROTOCOL_VERSION, ProtocolError, Reply, Request, Response, read_json, write_json,
};
use std::{
    fmt, fs,
    io::{self, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const IO_TIMEOUT: Duration = Duration::from_secs(5);
pub const SOCKET_RELATIVE: &str = "qingmail/daemon.sock";

pub fn request_id() -> io::Result<String> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| io::Error::other("无法生成 IPC 请求标识"))?;
    Ok(format!("{:032x}", u128::from_be_bytes(random)))
}
/// 绝对期限覆盖整个控制帧；慢速逐字节输入不能无限占据工作线程。
pub struct DeadlineStream {
    stream: UnixStream,
    read_deadline: Instant,
    write_deadline: Instant,
}
impl DeadlineStream {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        if rustix::net::sockopt::socket_peercred(&stream)?.uid != rustix::process::geteuid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "IPC 对端不属于当前用户",
            ));
        }
        let deadline = Instant::now() + IO_TIMEOUT;
        Ok(Self {
            stream,
            read_deadline: deadline,
            write_deadline: deadline,
        })
    }
    pub fn reset_write_deadline(&mut self) {
        self.write_deadline = Instant::now() + IO_TIMEOUT;
    }
}
fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|t| !t.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "IPC 控制帧超时"))
}
impl Read for DeadlineStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.stream
            .set_read_timeout(Some(remaining(self.read_deadline)?))?;
        self.stream.read(output)
    }
}
impl Write for DeadlineStream {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.stream
            .set_write_timeout(Some(remaining(self.write_deadline)?))?;
        self.stream.write(input)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
#[derive(Debug)]
pub enum ClientError {
    Protocol(ProtocolError),
    Remote(crate::RpcError),
    MismatchedResponse,
}
impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(e) => e.fmt(f),
            Self::Remote(e) => f.write_str(&e.message),
            Self::MismatchedResponse => f.write_str("IPC 响应标识或版本不匹配"),
        }
    }
}
impl std::error::Error for ClientError {}
impl From<ProtocolError> for ClientError {
    fn from(e: ProtocolError) -> Self {
        Self::Protocol(e)
    }
}
impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        Self::Protocol(ProtocolError::Io(e))
    }
}
#[derive(Clone)]
pub struct Client {
    runtime_root: PathBuf,
}
impl Client {
    pub fn new(runtime_root: PathBuf) -> Self {
        Self { runtime_root }
    }
    pub fn request(&self, command: Command) -> Result<Reply, ClientError> {
        self.call(&Request::new(request_id()?, command))
    }
    /// 调用者保留 request.id；结果丢失先 MutationResult，不能生成新 ID 重放写操作。
    pub fn call(&self, request: &Request) -> Result<Reply, ClientError> {
        request.validate()?;
        let path = checked_socket_path(&self.runtime_root)?;
        let stream = UnixStream::connect(path)?;
        let mut stream = DeadlineStream::new(stream)?;
        write_json(&mut stream, request)?;
        let response: Response = read_json(&mut BufReader::new(stream))?;
        if response.version != PROTOCOL_VERSION || response.id != request.id {
            return Err(ClientError::MismatchedResponse);
        }
        response.result.map_err(ClientError::Remote)
    }
}
pub fn checked_socket_path(root: &Path) -> io::Result<PathBuf> {
    if !root.is_absolute()
        || root
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC 运行根须为无相对组件的绝对路径",
        ));
    }
    for path in [root.to_owned(), root.join("qingmail")] {
        let m = fs::symlink_metadata(path)?;
        if !m.is_dir()
            || m.mode() & 0o777 != 0o700
            || m.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "IPC 运行目录类型、所有者或权限无效",
            ));
        }
    }
    let path = root.join(SOCKET_RELATIVE);
    let m = fs::symlink_metadata(&path)?;
    if !m.file_type().is_socket()
        || m.mode() & 0o777 != 0o600
        || m.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC socket 类型、所有者或权限无效",
        ));
    }
    Ok(path)
}
