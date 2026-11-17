//! 独立后台 IPC 基础。尚未接入的业务命令通过精确能力集与 Unsupported 表达。
mod endpoint;
mod resources;
mod send;
mod session;
use crate::{
    SyncEngine,
    account_gateway::AccountGateway,
    storage::{RPC_COMMANDS, StorageClient, StorageWorker},
};
pub use endpoint::Endpoint;
use mail_protocol::{transport::DeadlineStream, *};
use std::{
    io::{self, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
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
    accounts: AccountGateway,
    epoch: String,
    revision: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    sessions: Mutex<session::Sessions>,
    resources: Mutex<resources::Resources>,
    sending: send::Sending,
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
        let revision = Arc::new(AtomicU64::new(0));
        let accounts =
            AccountGateway::start(config.network_enabled, storage.client(), revision.clone())?;
        let sync = SyncEngine::start_enabled(storage.client(), config.network_enabled);
        let service = Arc::new(Service {
            storage: storage.client(),
            sync,
            accounts,
            epoch: transport::request_id()?,
            revision: revision.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            sessions: Mutex::new(session::Sessions::default()),
            resources: Mutex::new(resources::Resources::default()),
            sending: send::Sending::new(storage.client(), config.network_enabled, revision.clone()),
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
        service.accounts.stop();
        service.sending.stop();
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
    fn sessions(&self) -> std::sync::MutexGuard<'_, session::Sessions> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.observe(self.revision.load(Ordering::Acquire), self.sync.revision());
        sessions
    }
    fn status(&self, revision: u64) -> DaemonStatus {
        DaemonStatus {
            pid: std::process::id(),
            revision,
            sync_paused: self.sync.paused(),
            sync_status: self.sync.status(),
            sending: self.sending.active(),
            tray_status: "独立后台开发入口；前台与托盘迁移尚未启用".into(),
        }
    }
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
        let mut binary = Vec::new();
        let result = if raw.get("version").and_then(|v| v.as_u64())
            != Some(u64::from(PROTOCOL_VERSION))
        {
            Err(rpc_error(
                ErrorCode::VersionMismatch,
                "前后台协议版本不兼容",
            ))
        } else {
            match serde_json::from_value::<Request>(raw) {
                Ok(request) => {
                    if request.validate().is_err() {
                        Err(rpc_error(ErrorCode::InvalidRequest, "IPC 参数无效"))
                    } else {
                        match &request.command {
                            Command::ReadResource {
                                resource,
                                offset,
                                length,
                            } => self
                                .resources
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .read(resource, *offset, *length)
                                .map(|(chunk, bytes)| {
                                    binary = bytes;
                                    Reply::Resource(chunk)
                                }),
                            Command::WriteResource {
                                resource,
                                offset,
                                length,
                            } => {
                                let mut bytes = vec![0; *length as usize];
                                reader
                                    .read_exact(&mut bytes)
                                    .map_err(|_| {
                                        rpc_error(ErrorCode::InvalidRequest, "资源分块未完整接收")
                                    })
                                    .and_then(|()| {
                                        self.resources
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .write(resource, *offset, &bytes)
                                    })
                            }
                            _ => self.dispatch(request),
                        }
                    }
                }
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
            other => {
                other.map_err(io::Error::other)?;
                reader.get_mut().write_all(&binary)?;
                reader.get_mut().flush()
            }
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
                    CommandName::AttachUi,
                    CommandName::UiHeartbeat,
                    CommandName::DetachUi,
                    CommandName::Activate,
                    CommandName::FinishShutdown,
                    CommandName::Snapshot,
                    CommandName::Events,
                    CommandName::SyncNow,
                    CommandName::PauseSync,
                    CommandName::ReadResource,
                    CommandName::WriteResource,
                    CommandName::CreateUpload,
                    CommandName::FinishUpload,
                    CommandName::CancelResource,
                    CommandName::SelectedDraftBody,
                    CommandName::ReviewSend,
                ];
                commands.extend_from_slice(RPC_COMMANDS);
                if self.accounts.enabled() {
                    commands.extend([
                        CommandName::CheckAccount,
                        CommandName::AccountCheckStatus,
                        CommandName::CommitAccount,
                        CommandName::ConfirmSend,
                        CommandName::Resend,
                        CommandName::RetryCopy,
                    ]);
                }
                Ok(Reply::Hello {
                    commands,
                    epoch: self.epoch.clone(),
                })
            }
            Command::Status => Ok(Reply::Status(self.status(self.sessions().revision()))),
            Command::ReviewSend {
                account,
                draft,
                revision,
            } => self
                .sending
                .review(account.clone(), draft.clone(), *revision),
            Command::ConfirmSend { .. } | Command::Resend { .. } | Command::RetryCopy { .. } => {
                self.sending.submit(request)
            }
            Command::CreateUpload {
                account,
                name,
                media_type,
                size,
            } => self
                .resources
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .upload(account.clone(), name.clone(), media_type.clone(), *size),
            Command::CancelResource { resource } => {
                self.resources
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .cancel(resource);
                Ok(Reply::Acknowledged)
            }
            Command::SelectedDraftBody { account } => {
                let body =
                    crate::storage::receive(self.storage.selected_draft_body(account.clone()))
                        .map_err(|e| rpc_error(ErrorCode::Storage, e))?;
                let resource = body
                    .map(|bytes| {
                        self.resources
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(bytes.into(), "application/vnd.qingmail.draft+json".into())
                    })
                    .transpose()?;
                Ok(Reply::OptionalResource(resource))
            }
            Command::FinishUpload { resource } => {
                let claim = crate::storage::receive(self.storage.claim_mutation(request.clone()))
                    .map_err(|e| rpc_error(ErrorCode::Storage, e))??;
                match claim {
                    mail_storage::MutationClaim::Existing(MutationStatus::Finished(result)) => {
                        return *result;
                    }
                    mail_storage::MutationClaim::Existing(_) => {
                        return Err(rpc_error(
                            ErrorCode::Uncertain,
                            "上传导入结果未确认，请查询原请求，不能重放",
                        ));
                    }
                    mail_storage::MutationClaim::Acquired => (),
                }
                let result = (|| {
                    let upload = self
                        .resources
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .complete(resource)?;
                    crate::storage::receive(self.storage.import_bytes(
                        upload.account,
                        upload.name,
                        upload.media_type,
                        upload.bytes,
                    ))
                    .map(Reply::Attachment)
                    .map_err(|e| rpc_error(ErrorCode::Storage, e))
                })();
                crate::storage::receive(
                    self.storage
                        .finish_mutation(request.clone(), result.clone()),
                )
                .map_err(|e| rpc_error(ErrorCode::Storage, e))??;
                result
            }
            Command::AttachUi { session } => self.sessions().attach(session, Instant::now()),
            Command::UiHeartbeat {
                session,
                acknowledged,
            } => self
                .sessions()
                .heartbeat(session, *acknowledged, Instant::now()),
            Command::DetachUi { session } => self.sessions().detach(session, Instant::now()),
            Command::Activate { target } => self.sessions().activate(*target, Instant::now()),
            Command::Events { after } => Ok(Reply::Events(self.sessions().events(*after))),
            Command::SyncNow {
                account: None,
                folder: None,
                before: None,
            } => {
                if !self.sync.request_sync() {
                    return Err(rpc_error(ErrorCode::Offline, "隔离后台禁止联网"));
                }
                Ok(Reply::Acknowledged)
            }
            Command::PauseSync {
                account: None,
                paused,
            } => {
                self.sync.set_paused(*paused);
                Ok(Reply::Acknowledged)
            }
            Command::Snapshot => {
                // 版本在读取期间改变就重取；不能给旧数据贴新版本，导致漏掉失效事件。
                for _ in 0..4 {
                    let before = self.sessions().revision();
                    let accounts = self
                        .storage
                        .accounts()
                        .map_err(|e| rpc_error(ErrorCode::Busy, e))?
                        .recv_timeout(STORAGE_TIMEOUT)
                        .map_err(|_| rpc_error(ErrorCode::Busy, "快照读取超时"))?
                        .map_err(|e| rpc_error(ErrorCode::Storage, e))?;
                    let after = self.sessions().revision();
                    if before == after {
                        return Ok(Reply::Snapshot(AppSnapshot {
                            epoch: self.epoch.clone(),
                            revision: after,
                            status: self.status(after),
                            default_account: accounts.first().map(|(id, _)| id.clone()),
                            accounts,
                            pending_operations: 0,
                            draft_conflicts: 0,
                        }));
                    }
                }
                Err(rpc_error(
                    ErrorCode::Busy,
                    "后台状态正在变化，请重新获取快照",
                ))
            }
            Command::PrepareShutdown => {
                if self.accounts.saving() || self.sending.active() {
                    return Err(rpc_error(ErrorCode::Busy, "账号正在保存，请稍后退出"));
                }
                if self.sessions().prepare(Instant::now()) {
                    self.stopped.store(true, Ordering::Release);
                }
                Ok(Reply::Acknowledged)
            }
            Command::FinishShutdown { session, saved } => {
                if *saved && (self.accounts.saving() || self.sending.active()) {
                    return Err(rpc_error(ErrorCode::Busy, "账号正在保存，请稍后退出"));
                }
                if self.sessions().finish(session, *saved, Instant::now())? {
                    self.stopped.store(true, Ordering::Release);
                }
                Ok(Reply::Acknowledged)
            }
            Command::MutationResult { mutation } if self.sending.owns(mutation) => {
                Ok(Reply::Mutation(MutationStatus::InProgress))
            }
            Command::CheckAccount { .. }
            | Command::AccountCheckStatus { .. }
            | Command::CommitAccount { .. }
            | Command::MutationResult { .. } => self.accounts.call(request),
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
