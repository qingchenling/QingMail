//! IPC 与过渡期进程内前台共用账号命令；只接受引用，不接收密码。
use crate::{
    account_checks::CheckManager, account_commits::AccountCommitter, storage::StorageClient,
};
use mail_protocol::{Command, ErrorCode, MutationStatus, Reply, Request, RpcError};
use std::{
    io,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(5);

pub struct AccountGateway {
    // 先停止提交并释放其检查引用，再销毁检查服务。
    commits: AccountCommitter,
    checks: Arc<CheckManager>,
    storage: StorageClient,
    microsoft: crate::microsoft_login::MicrosoftLogin,
}
fn error(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}
impl AccountGateway {
    pub fn start(
        enabled: bool,
        storage: StorageClient,
        revision: Arc<AtomicU64>,
    ) -> io::Result<Self> {
        let checks = Arc::new(CheckManager::start(enabled)?);
        let commits = AccountCommitter::start(enabled, storage.clone(), checks.clone(), revision)?;
        Ok(Self {
            microsoft: Default::default(),
            commits,
            checks,
            storage,
        })
    }
    pub fn enabled(&self) -> bool {
        self.checks.enabled()
    }
    pub fn saving(&self) -> bool {
        self.commits.has_active()
    }
    /// 必须在后台线程调用。写请求结果未确认时查询原 ID，不自动重放。
    pub fn call(&self, request: Request) -> Result<Reply, RpcError> {
        request.validate().map_err(|e| {
            error(
                if request.version != mail_protocol::PROTOCOL_VERSION {
                    ErrorCode::VersionMismatch
                } else {
                    ErrorCode::InvalidRequest
                },
                e.to_string(),
            )
        })?;
        match &request.command {
            Command::BeginMicrosoftLogin {
                address,
                authentication,
            } if self.enabled() => self.microsoft.begin(&request.id, address, *authentication),
            Command::MicrosoftLoginStatus { session } if self.enabled() => {
                self.microsoft.poll(session)
            }
            Command::CancelMicrosoftLogin { session } if self.enabled() => {
                self.microsoft.cancel(session)
            }
            Command::CheckAccount {
                account,
                settings,
                credential_ref,
            } => {
                if !self.enabled() {
                    return Err(error(
                        ErrorCode::Unsupported,
                        "隔离后台未启用邮件网络或密钥库访问",
                    ));
                }
                let baseline = account
                    .as_ref()
                    .map(|account| {
                        self.storage
                            .account_version(account.clone())
                            .map_err(|e| error(ErrorCode::Storage, e))?
                            .recv_timeout(TIMEOUT)
                            .map_err(|_| error(ErrorCode::Busy, "账号查询超时"))?
                            .map_err(|e| error(ErrorCode::Storage, e))
                    })
                    .transpose()?;
                self.checks.begin(
                    &request.id,
                    account.clone(),
                    settings.clone(),
                    credential_ref.clone(),
                    baseline,
                )
            }
            Command::AccountCheckStatus { task } if self.enabled() => self.checks.poll(task),
            Command::CommitAccount { .. } => self
                .commits
                .submit(request)?
                .recv_timeout(TIMEOUT)
                .map_err(|_| {
                    error(
                        ErrorCode::Uncertain,
                        "账号提交确认超时，请查询原请求结果，不要重新提交",
                    )
                })?,
            Command::MutationResult { mutation } => {
                let mutation = mutation.clone();
                let response = self
                    .storage
                    .rpc(request)
                    .map_err(|e| error(ErrorCode::Storage, e))?
                    .recv_timeout(TIMEOUT)
                    .map_err(|_| error(ErrorCode::Busy, "保存结果查询超时，请稍后继续查询"))?
                    .map_err(|e| error(ErrorCode::Storage, e))?;
                match response.result {
                    Ok(Reply::Mutation(MutationStatus::InProgress))
                        if !self.commits.active(&mutation) =>
                    {
                        Ok(Reply::Mutation(MutationStatus::Uncertain))
                    }
                    result => result,
                }
            }
            _ => Err(error(ErrorCode::Unsupported, "账号命令未启用或不受支持")),
        }
    }
    pub fn stop(&mut self) {
        self.microsoft.stop();
        self.commits.stop();
        if let Some(checks) = Arc::get_mut(&mut self.checks) {
            checks.stop();
        }
    }
}
impl Drop for AccountGateway {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account_checks::CheckBackend,
        storage::{StorageWorker, receive},
    };
    use mail_core::{AccountId, AccountSettings, ConnectionSecurity, ServerSettings};
    use mail_service::{
        accounts::AccountCheck,
        credentials::{CredentialVault, Credentials, VaultError, staging::CredentialStaging},
    };
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Instant,
    };

    #[derive(Default)]
    struct FakeVault {
        saves: AtomicUsize,
        removed: Mutex<Vec<String>>,
    }
    impl CredentialVault for FakeVault {
        fn status(&self) -> Result<(), VaultError> {
            Ok(())
        }
        fn save(&self, _: &AccountId, _: &Credentials) -> Result<(), VaultError> {
            self.saves.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn load(&self, _: &AccountId) -> Result<Credentials, VaultError> {
            panic!("not an account read")
        }
        fn delete(&self, _: &AccountId) -> Result<(), VaultError> {
            panic!("successful commit retains old credentials")
        }
    }
    impl CredentialStaging for FakeVault {
        fn stage(&self, _: &Credentials) -> Result<String, VaultError> {
            panic!("frontend stages separately")
        }
        fn load_staged(&self, _: &str) -> Result<Credentials, VaultError> {
            Credentials::new("imap".into(), "smtp".into())
        }
        fn discard_staged(&self, reference: &str) -> Result<(), VaultError> {
            self.removed.lock().unwrap().push(reference.into());
            Ok(())
        }
        fn cleanup_staged(&self) -> Result<u64, VaultError> {
            panic!()
        }
    }
    impl CheckBackend for FakeVault {
        fn load(&self, reference: &str) -> Result<Credentials, VaultError> {
            self.load_staged(reference)
        }
        fn check(&self, _: &AccountSettings, _: &Credentials) -> AccountCheck {
            AccountCheck {
                imap: Ok(vec![]),
                smtp: Ok(()),
            }
        }
    }
    fn request(id: char, command: Command) -> Request {
        Request::new(id.to_string().repeat(32), command)
    }
    fn settings() -> AccountSettings {
        let server = ServerSettings {
            authentication: Default::default(),
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
    fn ready(gateway: &AccountGateway, task: &str) -> Reply {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let reply = gateway
                .call(request(
                    'e',
                    Command::AccountCheckStatus { task: task.into() },
                ))
                .unwrap();
            if matches!(reply, Reply::AccountCheck(_)) {
                return reply;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn same_dispatcher_checks_commits_and_queries_durable_result_without_rewriting_credentials() {
        let root = tempfile::tempdir().unwrap();
        let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let vault = Arc::new(FakeVault::default());
        let checks = Arc::new(CheckManager::with_backend(true, vault.clone()).unwrap());
        let commits = AccountCommitter::with_vault(
            true,
            worker.client(),
            checks.clone(),
            Default::default(),
            vault.clone(),
        )
        .unwrap();
        let mut gateway = AccountGateway {
            microsoft: Default::default(),
            commits,
            checks,
            storage: worker.client(),
        };
        let Reply::AccountCheckPending { task } = gateway
            .call(request(
                'a',
                Command::CheckAccount {
                    account: None,
                    settings: settings(),
                    credential_ref: "f".repeat(32),
                },
            ))
            .unwrap()
        else {
            panic!()
        };
        let Reply::AccountCheck(result) = ready(&gateway, &task) else {
            panic!()
        };
        let commit = request(
            'b',
            Command::CommitAccount {
                check: result.check.unwrap(),
                account: None,
            },
        );
        assert!(matches!(
            gateway.call(commit.clone()).unwrap(),
            Reply::Mutation(MutationStatus::InProgress)
        ));
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let reply = gateway
                .call(request(
                    'c',
                    Command::MutationResult {
                        mutation: commit.id.clone(),
                    },
                ))
                .unwrap();
            if matches!(reply, Reply::Mutation(MutationStatus::Finished(_))) {
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            gateway.call(commit).unwrap(),
            Reply::AccountSaved(_)
        ));
        gateway.stop();
        assert!(!gateway.saving());
        assert_eq!(receive(worker.client().accounts()).unwrap().len(), 1);
        assert_eq!(vault.saves.load(Ordering::Relaxed), 1);
        assert_eq!(*vault.removed.lock().unwrap(), vec!["f".repeat(32)]);
        worker.stop();
    }
    #[test]
    fn disabled_validation_and_orphaned_claim_never_authorize_work() {
        let root = tempfile::tempdir().unwrap();
        let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let mut gateway =
            AccountGateway::start(false, worker.client(), Default::default()).unwrap();
        let command = Command::CommitAccount {
            check: "d".repeat(32),
            account: None,
        };
        let mut invalid = request('a', command.clone());
        invalid.version = 0;
        assert_eq!(
            gateway.call(invalid).unwrap_err().code,
            ErrorCode::VersionMismatch
        );
        assert_eq!(
            gateway
                .call(request('a', command.clone()))
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
        let claim = request('b', command);
        receive(worker.client().claim_mutation(claim.clone()))
            .unwrap()
            .unwrap();
        assert!(matches!(
            gateway
                .call(request('c', Command::MutationResult { mutation: claim.id }))
                .unwrap(),
            Reply::Mutation(MutationStatus::Uncertain)
        ));
        assert!(receive(worker.client().accounts()).unwrap().is_empty());
        gateway.stop();
        worker.stop();
    }
}
