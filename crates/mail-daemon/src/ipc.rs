//! 独立后台 IPC 基础。尚未接入的业务命令通过精确能力集与 Unsupported 表达。
mod endpoint;
use crate::{
    SyncEngine,
    account_checks::CheckManager,
    storage::{RPC_COMMANDS, StorageClient, StorageWorker},
};
pub use endpoint::Endpoint;
use mail_protocol::{transport::DeadlineStream, *};
use std::{
    io::{self, BufReader},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
const WORKERS: usize = 4;
const WAITING_CONNECTIONS: usize = 16;
const STORAGE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Config {
    pub runtime_root: PathBuf,
    pub data: PathBuf,
    pub network_enabled: bool,
}
impl Config {
    /// 开发验证用：不读取正式 XDG 数据、不取真实凭据、不触发邮件网络。
    pub fn isolated(root: PathBuf) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "隔离根须为绝对路径",
            ));
        }
        for path in [&root, &root.join("runtime")] {
            match std::fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e),
            }
            endpoint::check_directory(path)?;
        }
        Ok(Self {
            runtime_root: root.join("runtime"),
            data: root.join("data"),
            network_enabled: false,
        })
    }
}
pub struct Server {
    endpoint: Endpoint,
    storage: StorageWorker,
    service: Arc<Service>,
}
struct Service {
    storage: StorageClient,
    sync: SyncEngine,
    checks: CheckManager,
    epoch: String,
    revision: AtomicU64,
    stopped: Arc<AtomicBool>,
}
impl Server {
    pub fn start(config: Config) -> io::Result<Self> {
        let endpoint = Endpoint::bind(&config.runtime_root)?;
        let storage = StorageWorker::start(Some(config.data))?;
        // 在宣告 ready 前真正打开/迁移数据库；持锁或损坏不能静默启动“假后台”。
        storage
            .client()
            .accounts()
            .map_err(io::Error::other)?
            .recv_timeout(STORAGE_TIMEOUT)
            .map_err(|_| io::Error::other("后台存储就绪超时"))?
            .map_err(io::Error::other)?;
        let checks = CheckManager::start(config.network_enabled)?;
        let sync = SyncEngine::start_enabled(storage.client(), config.network_enabled);
        let service = Arc::new(Service {
            storage: storage.client(),
            sync,
            checks,
            epoch: transport::request_id()?,
            revision: AtomicU64::new(0),
            stopped: Arc::new(AtomicBool::new(false)),
        });
        Ok(Self {
            endpoint,
            storage,
            service,
        })
    }
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        self.service.stopped.clone()
    }
    pub fn socket_path(&self) -> &std::path::Path {
        self.endpoint.socket_path()
    }
    /// 固定工作线程和有限等待队列；客户端断开不停止后台，不自动重放业务请求。
    pub fn run(mut self) -> io::Result<()> {
        let (sender, receiver) = mpsc::sync_channel::<UnixStream>(WAITING_CONNECTIONS);
        let receiver = Arc::new(Mutex::new(receiver));
        let result = thread::scope(|scope| {
            for _ in 0..WORKERS {
                let receiver = receiver.clone();
                let service = self.service.clone();
                scope.spawn(move || {
                    loop {
                        let stream = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        match stream {
                            Ok(stream) => {
                                let _ = service.serve(stream);
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
            let result = (|| {
                while !self.service.stopped.load(Ordering::Acquire) {
                    match self.endpoint.accept()? {
                        Some(stream) => {
                            let _ = sender.try_send(stream);
                        }
                        None => thread::park_timeout(Duration::from_millis(50)),
                    }
                }
                Ok(())
            })();
            drop(sender);
            result
        });
        let mut service = Arc::try_unwrap(self.service)
            .map_err(|_| io::Error::other("后台工作线程尚未释放状态"))?;
        service.checks.stop();
        service.sync.stop();
        self.storage.stop();
        result
    }
}
fn rpc_error(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}
impl Service {
    fn serve(&self, stream: UnixStream) -> io::Result<()> {
        let stream = DeadlineStream::new(stream)?;
        let mut reader = BufReader::new(stream);
        let raw: serde_json::Value = read_json(&mut reader).map_err(io::Error::other)?;
        let id = raw
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|id| valid_id(id))
            .unwrap_or("")
            .to_owned();
        let result =
            if raw.get("version").and_then(|v| v.as_u64()) != Some(u64::from(PROTOCOL_VERSION)) {
                Err(rpc_error(
                    ErrorCode::VersionMismatch,
                    "前后台协议版本不兼容",
                ))
            } else {
                match serde_json::from_value::<Request>(raw) {
                    Ok(request) => self.dispatch(request),
                    Err(_) => Err(rpc_error(
                        ErrorCode::InvalidRequest,
                        "IPC 请求格式无效或命令未知",
                    )),
                }
            };
        let response = Response {
            version: PROTOCOL_VERSION,
            id,
            result,
        };
        reader.get_mut().reset_write_deadline();
        match write_json(reader.get_mut(), &response) {
            Err(ProtocolError::TooLarge) => {
                let response = Response {
                    version: PROTOCOL_VERSION,
                    id: response.id,
                    result: Err(rpc_error(
                        ErrorCode::InvalidRequest,
                        "结果超过控制帧限制，请缩小查询范围",
                    )),
                };
                write_json(reader.get_mut(), &response).map_err(io::Error::other)
            }
            other => other.map_err(io::Error::other),
        }
    }
    fn dispatch(&self, request: Request) -> Result<Reply, RpcError> {
        request
            .validate()
            .map_err(|_| rpc_error(ErrorCode::InvalidRequest, "IPC 参数无效"))?;
        if self.stopped.load(Ordering::Acquire) {
            return Err(rpc_error(ErrorCode::Busy, "后台正在退出"));
        }
        match &request.command {
            Command::Hello => {
                let mut commands = vec![
                    CommandName::Hello,
                    CommandName::Status,
                    CommandName::PrepareShutdown,
                ];
                commands.extend_from_slice(RPC_COMMANDS);
                if self.checks.enabled() {
                    commands.extend([CommandName::CheckAccount, CommandName::AccountCheckStatus]);
                }
                Ok(Reply::Hello {
                    commands,
                    epoch: self.epoch.clone(),
                })
            }
            Command::Status => Ok(Reply::Status(DaemonStatus {
                pid: std::process::id(),
                revision: self.revision.load(Ordering::Acquire) + self.sync.revision(),
                sync_paused: self.sync.paused(),
                sync_status: self.sync.status(),
                sending: false,
                tray_status: "独立后台开发入口；前台与托盘迁移尚未启用".into(),
            })),
            Command::PrepareShutdown => {
                // 本阶段不支持 AttachUi/发送命令，没有编辑会话需要协调。接入时必须替换为两阶段关闭。
                self.stopped.store(true, Ordering::Release);
                Ok(Reply::Acknowledged)
            }
            Command::CheckAccount {
                account,
                settings,
                credential_ref,
            } => {
                if !self.checks.enabled() {
                    return Err(rpc_error(
                        ErrorCode::Unsupported,
                        "隔离后台未启用邮件网络或密钥库访问",
                    ));
                }
                if let Some(account) = account {
                    let accounts = self
                        .storage
                        .accounts()
                        .map_err(|e| rpc_error(ErrorCode::Storage, e))?
                        .recv_timeout(STORAGE_TIMEOUT)
                        .map_err(|_| rpc_error(ErrorCode::Busy, "账号查询超时"))?
                        .map_err(|e| rpc_error(ErrorCode::Storage, e))?;
                    if !accounts.iter().any(|(id, _)| id == account) {
                        return Err(rpc_error(ErrorCode::NotFound, "目标账号不存在"));
                    }
                }
                self.checks.begin(
                    &request.id,
                    account.clone(),
                    settings.clone(),
                    credential_ref.clone(),
                )
            }
            Command::AccountCheckStatus { task } if self.checks.enabled() => self.checks.poll(task),
            command if RPC_COMMANDS.contains(&command.name()) => {
                let changes = matches!(
                    command,
                    Command::OpenDraft { .. }
                        | Command::NewDraft { .. }
                        | Command::SelectDraft { .. }
                        | Command::SaveDraft { .. }
                        | Command::DiscardSelectedDraft { .. }
                        | Command::StopUncertain { .. }
                );
                let response = self
                    .storage
                    .rpc(request)
                    .map_err(|e| rpc_error(ErrorCode::Busy, e))?
                    .recv_timeout(STORAGE_TIMEOUT)
                    .map_err(|_| {
                        rpc_error(ErrorCode::Uncertain, "后台请求未确认，请查询原请求结果")
                    })?
                    .map_err(|e| rpc_error(ErrorCode::Storage, e))?;
                if changes && response.result.is_ok() {
                    self.revision.fetch_add(1, Ordering::Release);
                }
                response.result
            }
            _ => Err(rpc_error(
                ErrorCode::Unsupported,
                "此命令尚未接入独立后台，请查询 Hello 能力列表",
            )),
        }
    }
}
