//! 独立后台 IPC 基础。尚未接入的业务命令通过精确能力集与 Unsupported 表达。
mod business;
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
    pub cache: PathBuf,
    pub tray: bool,
}
impl Config {
    pub fn production() -> io::Result<Self> {
        let paths = mail_storage::AppPaths::discover().map_err(io::Error::other)?;
        let runtime_root = paths
            .runtime
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .ok_or_else(|| io::Error::other("缺少XDG_RUNTIME_DIR"))?;
        Ok(Self {
            runtime_root,
            data: paths.data,
            cache: paths.cache,
            network_enabled: true,
            tray: true,
        })
    }

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
            cache: root.join("cache"),
            network_enabled: false,
            tray: std::env::var_os("QINGMAIL_IPC_TEST_TRAY").is_some(),
        })
    }
}
pub struct Server {
    endpoint: Endpoint,
    storage: StorageWorker,
    service: Arc<Service>,
    tray_enabled: bool,
}
struct Service {
    storage: StorageClient,
    sync: SyncEngine,
    operations: crate::operations::Operations,
    accounts: AccountGateway,
    epoch: String,
    revision: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    sessions: Mutex<session::Sessions>,
    resources: Arc<Mutex<resources::Resources>>,
    business: business::Business,
    sending: send::Sending,
    tray_status: Mutex<String>,
    ownership: Mutex<()>,
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
        let operations = crate::operations::Operations::start(
            storage.client(),
            config.network_enabled,
            sync.pause_signal(),
            revision.clone(),
        )?;
        let resources = Arc::new(Mutex::new(resources::Resources::default()));
        let business = business::Business::start(
            storage.client(),
            config.network_enabled,
            config.cache,
            resources.clone(),
            revision.clone(),
        )?;
        let service = Arc::new(Service {
            storage: storage.client(),
            ownership: Mutex::new(()),
            sync,
            operations,
            accounts,
            epoch: transport::request_id()?,
            revision: revision.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
            sessions: Mutex::new(session::Sessions::default()),
            resources,
            business,
            tray_status: Mutex::new(if config.tray {
                "正在检查托盘宿主".into()
            } else {
                "未启用托盘；再次启动可打开前台，--quit 可停止后台".into()
            }),
            sending: send::Sending::new(storage.client(), config.network_enabled, revision.clone()),
        });
        Ok(Self {
            endpoint,
            storage,
            service,
            tray_enabled: config.tray,
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
        let (tray_queue, tray_commands) = mpsc::sync_channel(16);
        let (tray_result, tray_receive) = mpsc::sync_channel(1);
        if self.tray_enabled {
            thread::spawn(move || {
                let _ = tray_result.send(crate::Tray::start(tray_queue));
            });
        }
        let mut tray = None;
        let mut exit_intent = false;
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
                    if let Ok(result) = tray_receive.try_recv() {
                        *self
                            .service
                            .tray_status
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = match result {
                            Ok(item) => {
                                tray = Some(item);
                                "后台托盘已注册".into()
                            }
                            Err(e) => {
                                format!("托盘不可用：{e}；再次启动打开前台，--quit 可停止后台")
                            }
                        };
                    }
                    for command in tray_commands.try_iter() {
                        match command {
                            crate::Command::Open => {
                                if self
                                    .service
                                    .sessions()
                                    .activate(WindowTarget::Mailbox, Instant::now())
                                    .is_err()
                                    && let Ok(exe) = std::env::current_exe()
                                {
                                    let _ =
                                        std::process::Command::new(exe.with_file_name("mail-ui"))
                                            .env_remove("QINGMAIL_IPC_TEST_EDITOR")
                                            .env_remove("QINGMAIL_IPC_TEST_MAILBOX")
                                            .stdin(std::process::Stdio::null())
                                            .stdout(std::process::Stdio::null())
                                            .stderr(std::process::Stdio::null())
                                            .spawn()
                                            .map(|mut child| {
                                                thread::spawn(move || {
                                                    let _ = child.wait();
                                                })
                                            });
                                }
                            }
                            crate::Command::Quit => {
                                exit_intent = true;
                            }
                            _ => (),
                        }
                    }

                    if exit_intent {
                        let result = self.service.dispatch(Request::new(
                            transport::request_id()?,
                            Command::PrepareShutdown,
                        ));
                        exit_intent = matches!(
                            result,
                            Err(RpcError {
                                code: ErrorCode::Busy,
                                ..
                            })
                        );
                    }
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
        service.operations.stop();
        service.business.stop();
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
            account_sync: self.sync.account_states(),
            pid: std::process::id(),
            revision,
            sync_paused: self.sync.paused(),
            sync_status: self.sync.status(),
            sending: self.sending.active(),
            account_saving: self.accounts.saving(),
            tray_status: self
                .tray_status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
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
        let result = result.and_then(|reply| {
            let bytes = serde_json::to_vec(&reply)
                .map_err(|e| rpc_error(ErrorCode::Internal, e.to_string()))?;
            if bytes.len() > MAX_CONTROL_BYTES / 2 {
                if bytes.len() as u64 > MAX_STORAGE_PAYLOAD {
                    return Err(rpc_error(
                        ErrorCode::InvalidRequest,
                        "结果超限，请使用分页查询",
                    ));
                }
                self.resources
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(bytes.into(), "application/vnd.qingmail.reply+json".into())
                    .map(Reply::Encoded)
            } else {
                Ok(reply)
            }
        });
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
                    CommandName::Business,
                    CommandName::JobResult,
                ];
                commands.extend_from_slice(RPC_COMMANDS);
                commands.extend([
                    CommandName::CreateStorageRequest,
                    CommandName::StorageRequest,
                ]);
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
            Command::CreateStorageRequest { size } => self
                .resources
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .upload(
                    mail_core::AccountId("ipc-control".into()),
                    "request".into(),
                    "application/vnd.qingmail.request+json".into(),
                    *size,
                ),
            Command::StorageRequest { resource } => {
                let upload = self
                    .resources
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .complete(resource)?;
                if upload.media_type != "application/vnd.qingmail.request+json"
                    || upload.bytes.len() as u64 > MAX_STORAGE_PAYLOAD
                {
                    return Err(rpc_error(ErrorCode::InvalidRequest, "存储请求资源无效"));
                }
                let nested: Request = serde_json::from_slice(&upload.bytes)
                    .map_err(|_| rpc_error(ErrorCode::InvalidRequest, "存储请求无效"))?;
                if nested.id != request.id || !RPC_COMMANDS.contains(&nested.command.name()) {
                    return Err(rpc_error(ErrorCode::Unsupported, "资源仅允许有界存储命令"));
                }
                self.dispatch(nested)
            }
            Command::Business { operation } => {
                let _guard = self.ownership.lock().unwrap_or_else(|e| e.into_inner());
                if self.sessions().closing() {
                    return Err(rpc_error(ErrorCode::Busy, "后台正在协调保存并退出"));
                }
                if matches!(operation, BusinessOperation::RemoveAccount { .. })
                    && (self.accounts.saving() || self.sending.active() || self.business.active())
                {
                    return Err(rpc_error(
                        ErrorCode::Busy,
                        "账号或发送任务仍在处理，不能移除账号",
                    ));
                }
                self.business.submit(request)
            }
            Command::JobResult { task } => self.business.result(task),
            Command::Status => Ok(Reply::Status(self.status(self.sessions().revision()))),
            Command::ReviewSend {
                account,
                draft,
                revision,
            } => self
                .sending
                .review(account.clone(), draft.clone(), *revision),
            Command::ConfirmSend { .. } | Command::Resend { .. } | Command::RetryCopy { .. } => {
                let _guard = self.ownership.lock().unwrap_or_else(|e| e.into_inner());
                if self.sessions().closing() {
                    return Err(rpc_error(ErrorCode::Busy, "后台正在协调保存并退出"));
                }
                if self.business.active() || self.accounts.saving() {
                    return Err(rpc_error(ErrorCode::Busy, "后台业务仍在处理，请稍后发送"));
                }
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
                account,
                folder: None,
                before: None,
            } => {
                if !self.sync.request_account(account.clone()) {
                    return Err(rpc_error(ErrorCode::Offline, "隔离后台禁止联网"));
                }
                Ok(Reply::Acknowledged)
            }
            Command::PauseSync {
                account: Some(_), ..
            } => {
                let response = crate::storage::receive(self.storage.rpc(request))
                    .map_err(|e| rpc_error(ErrorCode::Storage, e))?;
                if response.result.is_ok() {
                    self.revision.fetch_add(1, Ordering::Release);
                }
                response.result
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
                    let default_account = match self
                        .storage
                        .control(Command::AccountOptions)
                        .map_err(|e| rpc_error(ErrorCode::Storage, e))?
                    {
                        Reply::AccountOptions { default, .. } => default,
                        _ => return Err(rpc_error(ErrorCode::Storage, "账号设置读取失败")),
                    };
                    let after = self.sessions().revision();
                    if before == after {
                        return Ok(Reply::Snapshot(AppSnapshot {
                            epoch: self.epoch.clone(),
                            revision: after,
                            status: self.status(after),
                            default_account,
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
                let _guard = self.ownership.lock().unwrap_or_else(|e| e.into_inner());
                if self.accounts.saving() || self.sending.active() || self.business.active() {
                    return Err(rpc_error(ErrorCode::Busy, "账号正在保存，请稍后退出"));
                }
                if self.sessions().prepare(Instant::now()) {
                    self.stopped.store(true, Ordering::Release);
                }
                Ok(Reply::Acknowledged)
            }
            Command::FinishShutdown { session, saved } => {
                let _guard = self.ownership.lock().unwrap_or_else(|e| e.into_inner());
                if *saved
                    && (self.accounts.saving() || self.sending.active() || self.business.active())
                {
                    return Err(rpc_error(ErrorCode::Busy, "账号正在保存，请稍后退出"));
                }
                if self.sessions().finish(session, *saved, Instant::now())? {
                    self.stopped.store(true, Ordering::Release);
                }
                Ok(Reply::Acknowledged)
            }
            Command::MutationResult { mutation }
                if self.sending.owns(mutation) || self.business.owns(mutation) =>
            {
                Ok(Reply::Mutation(MutationStatus::InProgress))
            }
            Command::CommitAccount { .. } => {
                let _guard = self.ownership.lock().unwrap_or_else(|e| e.into_inner());
                if self.sessions().closing() {
                    return Err(rpc_error(ErrorCode::Busy, "后台正在协调保存并退出"));
                }
                if self.business.active() || self.sending.active() {
                    return Err(rpc_error(
                        ErrorCode::Busy,
                        "后台业务仍在处理，请稍后保存账号",
                    ));
                }
                self.accounts.call(request)
            }
            Command::CheckAccount { .. }
            | Command::AccountCheckStatus { .. }
            | Command::MutationResult { .. } => self.accounts.call(request),
            command if RPC_COMMANDS.contains(&command.name()) => {
                if matches!(
                    command,
                    Command::EnqueueOperation { .. }
                        | Command::RetryOperation { .. }
                        | Command::ResolveOperation { .. }
                ) && self.sessions().closing()
                {
                    return Err(rpc_error(ErrorCode::Busy, "后台正在退出，未接受新邮件操作"));
                }
                let changes = matches!(
                    command,
                    Command::EnqueueOperation { .. }
                        | Command::RetryOperation { .. }
                        | Command::ResolveOperation { .. }
                        | Command::SetDefaultAccount { .. }
                        | Command::OpenDraft { .. }
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
