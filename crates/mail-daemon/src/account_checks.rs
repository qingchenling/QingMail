//! 连接检查独立于控制线程和 SQLite；令牌绑定原配置、目标账号与已验证凭据。
use mail_core::{AccountId, AccountSettings};
use mail_protocol::{AccountCheckResult, ErrorCode, Reply, RpcError, transport::request_id};
use mail_service::{
    accounts::{self, AccountCheck, CheckError},
    credentials::{Credentials, SystemVault, VaultError, staging::CredentialStaging},
};
use std::{
    collections::HashMap,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_CHECKS: usize = 16;
const QUEUED_CHECKS: usize = 2;

pub trait CheckBackend: Send + Sync + 'static {
    fn load(&self, reference: &str) -> Result<Credentials, VaultError>;
    fn check(&self, settings: &AccountSettings, credentials: &Credentials) -> AccountCheck;
}
struct SystemBackend;
impl CheckBackend for SystemBackend {
    fn load(&self, reference: &str) -> Result<Credentials, VaultError> {
        SystemVault.load_staged(reference)
    }
    fn check(&self, settings: &AccountSettings, credentials: &Credentials) -> AccountCheck {
        accounts::check(settings, credentials)
    }
}

/// 不实现 Debug/Clone；账号提交直接消费已检查的这份凭据，不重新信任临时项。
pub struct CheckedAccount {
    pub account: Option<AccountId>,
    pub settings: AccountSettings,
    pub credential_ref: String,
    pub credentials: Credentials,
    pub baseline: Option<mail_storage::AccountVersion>,
}
struct Entry {
    request_id: String,
    account: Option<AccountId>,
    settings: AccountSettings,
    credential_ref: String,
    baseline: Option<mail_storage::AccountVersion>,
    expires: Instant,
    state: State,
}
enum State {
    Pending,
    Finished {
        result: AccountCheckResult,
        credentials: Option<Credentials>,
    },
    Failed(RpcError),
    Consumed,
}
type Entries = Arc<Mutex<HashMap<String, Entry>>>;
pub struct CheckManager {
    enabled: bool,
    entries: Entries,
    sender: Option<mpsc::SyncSender<String>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
fn error(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}
fn check_error(value: CheckError) -> RpcError {
    error(
        match value {
            CheckError::Authentication => ErrorCode::Authentication,
            CheckError::Invalid => ErrorCode::InvalidRequest,
            _ => ErrorCode::Offline,
        },
        value.to_string(),
    )
}
impl CheckManager {
    pub fn start(enabled: bool) -> io::Result<Self> {
        Self::with_backend(enabled, Arc::new(SystemBackend))
    }
    pub fn with_backend(enabled: bool, backend: Arc<dyn CheckBackend>) -> io::Result<Self> {
        let entries: Entries = Default::default();
        let stopped = Arc::new(AtomicBool::new(false));
        if !enabled {
            return Ok(Self {
                enabled,
                entries,
                sender: None,
                stopped,
                worker: None,
            });
        }
        let (sender, receiver) = mpsc::sync_channel::<String>(QUEUED_CHECKS);
        let state = entries.clone();
        let stop = stopped.clone();
        let worker = thread::Builder::new()
            .name("qingmail-account-check".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let task = match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(task) => task,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .retain(|_, entry| entry.expires > Instant::now());
                            continue;
                        }
                        Err(_) => break,
                    };
                    let Some((settings, reference)) = ({
                        let entries = state.lock().unwrap_or_else(|e| e.into_inner());
                        entries
                            .get(&task)
                            .filter(|e| Instant::now() < e.expires)
                            .map(|e| (e.settings.clone(), e.credential_ref.clone()))
                    }) else {
                        continue;
                    };
                    let outcome = match backend.load(&reference) {
                        Ok(credentials) => {
                            let checked = backend.check(&settings, &credentials);
                            let passed = checked.passed();
                            State::Finished {
                                result: AccountCheckResult {
                                    check: passed.then(|| task.clone()),
                                    imap: checked.imap.map_err(check_error),
                                    smtp: checked.smtp.map_err(check_error),
                                },
                                credentials: passed.then_some(credentials),
                            }
                        }
                        Err(e) => State::Failed(error(ErrorCode::Authentication, e.to_string())),
                    };
                    let mut entries = state.lock().unwrap_or_else(|e| e.into_inner());
                    entries.retain(|_, entry| entry.expires > Instant::now());
                    if let Some(entry) = entries.get_mut(&task) {
                        entry.state = outcome;
                    }
                }
            })?;
        Ok(Self {
            enabled,
            entries,
            sender: Some(sender),
            stopped,
            worker: Some(worker),
        })
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn begin(
        &self,
        request: &str,
        account: Option<AccountId>,
        settings: AccountSettings,
        credential_ref: String,
        baseline: Option<mail_storage::AccountVersion>,
    ) -> Result<Reply, RpcError> {
        if !self.enabled {
            return Err(error(
                ErrorCode::Unsupported,
                "隔离后台未启用邮件网络或密钥库访问",
            ));
        }
        if !mail_protocol::valid_id(request)
            || !mail_protocol::valid_id(&credential_ref)
            || account
                .as_ref()
                .is_some_and(|id| !mail_protocol::valid_id(&id.0))
        {
            return Err(error(ErrorCode::InvalidRequest, "检查标识无效"));
        }
        accounts::validate(&settings).map_err(check_error)?;
        if baseline.as_ref().map(|v| &v.account) != account.as_ref() {
            return Err(error(ErrorCode::Conflict, "账号检查基线与目标不一致"));
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| entry.expires > Instant::now());
        if let Some((task, entry)) = entries.iter().find(|(_, e)| e.request_id == request) {
            if entry.account != account
                || entry.settings != settings
                || entry.credential_ref != credential_ref
                || entry.baseline != baseline
            {
                return Err(error(ErrorCode::Conflict, "原检查请求参数不一致"));
            }
            return Self::reply(task, entry);
        }
        if entries.len() >= MAX_CHECKS {
            return Err(error(ErrorCode::Busy, "连接检查已达上限，请稍后重试"));
        }
        let task = request_id().map_err(|_| error(ErrorCode::Internal, "无法创建检查标识"))?;
        entries.insert(
            task.clone(),
            Entry {
                request_id: request.into(),
                account,
                settings,
                credential_ref,
                baseline,
                expires: Instant::now() + TOKEN_TTL,
                state: State::Pending,
            },
        );
        if self
            .sender
            .as_ref()
            .ok_or_else(|| error(ErrorCode::Busy, "检查服务已停止"))?
            .try_send(task.clone())
            .is_err()
        {
            entries.remove(&task);
            return Err(error(ErrorCode::Busy, "连接检查队列已满，请稍后重试"));
        }
        Ok(Reply::AccountCheckPending { task })
    }
    pub fn poll(&self, task: &str) -> Result<Reply, RpcError> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| entry.expires > Instant::now());
        let entry = entries
            .get(task)
            .ok_or_else(|| error(ErrorCode::NotFound, "连接检查不存在或已过期，请重新检查"))?;
        Self::reply(task, entry)
    }
    fn reply(task: &str, entry: &Entry) -> Result<Reply, RpcError> {
        match &entry.state {
            State::Pending => Ok(Reply::AccountCheckPending { task: task.into() }),
            State::Finished { result, .. } => Ok(Reply::AccountCheck(result.clone())),
            State::Failed(error) => Err(error.clone()),
            State::Consumed => Err(error(
                ErrorCode::Conflict,
                "检查令牌已消费，请查询账号保存结果",
            )),
        }
    }
    /// 调用账号提交前消费；令牌失效/目标不匹配绝不读取或更新正式凭据。
    /// 持久提交及幂等记录由调用者管理，不能用新请求重新消费或自动重试。
    pub fn take_checked(
        &self,
        task: &str,
        account: &Option<AccountId>,
    ) -> Result<CheckedAccount, RpcError> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| entry.expires > Instant::now());
        let entry = entries
            .get_mut(task)
            .ok_or_else(|| error(ErrorCode::NotFound, "连接检查不存在或已过期"))?;
        if &entry.account != account {
            return Err(error(ErrorCode::Conflict, "检查令牌不属于该目标账号"));
        }
        match &mut entry.state {
            State::Finished { credentials, .. } => {
                let credentials = credentials
                    .take()
                    .ok_or_else(|| error(ErrorCode::Authentication, "连接检查未通过"))?;
                entry.state = State::Consumed;
                Ok(CheckedAccount {
                    account: entry.account.clone(),
                    settings: entry.settings.clone(),
                    credential_ref: entry.credential_ref.clone(),
                    credentials,
                    baseline: entry.baseline.clone(),
                })
            }
            State::Pending => Err(error(ErrorCode::Busy, "连接检查尚未完成")),
            _ => Err(error(
                ErrorCode::Conflict,
                "检查令牌不可用，请检查原请求状态",
            )),
        }
    }
    pub fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}
impl Drop for CheckManager {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct FakeBackend {
        loads: AtomicUsize,
        passed: bool,
        missing: bool,
    }
    impl CheckBackend for FakeBackend {
        fn load(&self, _: &str) -> Result<Credentials, VaultError> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            if self.missing {
                return Err(VaultError::Expired);
            }
            Credentials::new("test-imap".into(), "test-smtp".into())
        }
        fn check(&self, _: &AccountSettings, _: &Credentials) -> AccountCheck {
            AccountCheck {
                imap: Ok(vec![]),
                smtp: if self.passed {
                    Ok(())
                } else {
                    Err(CheckError::Authentication)
                },
            }
        }
    }
    fn settings() -> AccountSettings {
        use mail_core::{ConnectionSecurity, ServerSettings};
        let server = ServerSettings {
            host: "mail.example.test".into(),
            port: 993,
            username: "test".into(),
            security: ConnectionSecurity::Tls,
        };
        AccountSettings {
            display_name: "测试".into(),
            address: "test@example.test".into(),
            imap: server.clone(),
            smtp: server,
        }
    }
    fn task(reply: Reply) -> String {
        match reply {
            Reply::AccountCheckPending { task } => task,
            _ => panic!(),
        }
    }
    fn finished(manager: &CheckManager, task: &str) -> Result<AccountCheckResult, RpcError> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match manager.poll(task)? {
                Reply::AccountCheck(result) => return Ok(result),
                Reply::AccountCheckPending { .. } => (),
                _ => panic!(),
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn check_is_async_deduplicated_bound_to_target_and_consumed_once() {
        let backend = Arc::new(FakeBackend {
            loads: AtomicUsize::new(0),
            passed: true,
            missing: false,
        });
        let manager = CheckManager::with_backend(true, backend.clone()).unwrap();
        let id = "a".repeat(32);
        let target = Some(AccountId("b".repeat(32)));
        let baseline = Some(mail_storage::AccountVersion {
            account: target.clone().unwrap(),
            settings: settings(),
            credential_key: target.clone().unwrap(),
        });
        let task = task(
            manager
                .begin(
                    &id,
                    target.clone(),
                    settings(),
                    "c".repeat(32),
                    baseline.clone(),
                )
                .unwrap(),
        );
        assert_eq!(finished(&manager, &task).unwrap().check, Some(task.clone()));
        assert!(matches!(
            manager
                .begin(
                    &id,
                    target.clone(),
                    settings(),
                    "c".repeat(32),
                    baseline.clone()
                )
                .unwrap(),
            Reply::AccountCheck(_)
        ));
        assert_eq!(backend.loads.load(Ordering::Relaxed), 1);
        assert_eq!(
            manager
                .begin(&id, None, settings(), "c".repeat(32), None)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert!(manager.take_checked(&task, &None).is_err());
        let checked = manager.take_checked(&task, &target).unwrap();
        assert_eq!(checked.account, target);
        assert_eq!(checked.settings, settings());
        assert_eq!(*checked.credentials.imap, "test-imap");
        assert!(manager.take_checked(&task, &target).is_err());
    }
    #[test]
    fn bounded_queue_rejects_more_checks_without_blocking_status_or_shutdown() {
        struct BlockingBackend {
            started: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl CheckBackend for BlockingBackend {
            fn load(&self, _: &str) -> Result<Credentials, VaultError> {
                Credentials::new("imap".into(), "smtp".into())
            }
            fn check(&self, _: &AccountSettings, _: &Credentials) -> AccountCheck {
                self.started.try_send(()).unwrap();
                let _ = self
                    .release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2));
                AccountCheck {
                    imap: Ok(vec![]),
                    smtp: Ok(()),
                }
            }
        }
        let (started, started_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let mut manager = CheckManager::with_backend(
            true,
            Arc::new(BlockingBackend {
                started,
                release: Mutex::new(release_rx),
            }),
        )
        .unwrap();
        let first = task(
            manager
                .begin(&"a".repeat(32), None, settings(), "f".repeat(32), None)
                .unwrap(),
        );
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for id in ['b', 'c'] {
            manager
                .begin(
                    &id.to_string().repeat(32),
                    None,
                    settings(),
                    "f".repeat(32),
                    None,
                )
                .unwrap();
        }
        assert_eq!(
            manager
                .begin(&"d".repeat(32), None, settings(), "f".repeat(32), None)
                .unwrap_err()
                .code,
            ErrorCode::Busy
        );
        assert!(matches!(
            manager.poll(&first).unwrap(),
            Reply::AccountCheckPending { .. }
        ));
        manager.stopped.store(true, Ordering::Release);
        release.send(()).unwrap();
        manager.stop();
        assert!(manager.entries.lock().unwrap().is_empty());
    }
    #[test]
    fn failed_expired_and_disabled_checks_never_offer_commit_token() {
        for missing in [false, true] {
            let backend = Arc::new(FakeBackend {
                loads: AtomicUsize::new(0),
                passed: false,
                missing,
            });
            let manager = CheckManager::with_backend(true, backend.clone()).unwrap();
            let task = task(
                manager
                    .begin(&"a".repeat(32), None, settings(), "c".repeat(32), None)
                    .unwrap(),
            );
            if missing {
                assert_eq!(
                    finished(&manager, &task).unwrap_err().code,
                    ErrorCode::Authentication
                );
            } else {
                let result = finished(&manager, &task).unwrap();
                assert!(result.check.is_none());
                assert!(result.smtp.is_err());
            }
            assert!(manager.take_checked(&task, &None).is_err());
            manager
                .entries
                .lock()
                .unwrap()
                .get_mut(&task)
                .unwrap()
                .expires = Instant::now();
            assert_eq!(manager.poll(&task).unwrap_err().code, ErrorCode::NotFound);
        }
        let backend = Arc::new(FakeBackend {
            loads: AtomicUsize::new(0),
            passed: true,
            missing: false,
        });
        let manager = CheckManager::with_backend(false, backend.clone()).unwrap();
        assert_eq!(
            manager
                .begin(&"a".repeat(32), None, settings(), "c".repeat(32), None)
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
        assert_eq!(backend.loads.load(Ordering::Relaxed), 0);
    }
}
