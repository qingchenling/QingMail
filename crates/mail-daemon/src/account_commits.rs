//! 账号提交先留持久请求，再写独立凭据项，最后原子切换数据库引用及请求结果。
use crate::{
    account_checks::CheckManager,
    storage::{StorageClient, receive},
};
use mail_core::AccountId;
use mail_protocol::{
    Command, ErrorCode, MutationStatus, Reply, Request, RpcError, transport::request_id,
};
use mail_service::credentials::{
    CredentialVault, SystemVault, VaultError, staging::CredentialStaging,
};
use mail_storage::MutationClaim;
use std::{
    collections::HashMap,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

type RpcResult = Result<Reply, RpcError>;
type Active = Arc<Mutex<HashMap<String, usize>>>;
struct Activity {
    id: String,
    active: Active,
}
impl Drop for Activity {
    fn drop(&mut self) {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = active.get_mut(&self.id) {
            *count -= 1;
            if *count == 0 {
                active.remove(&self.id);
            }
        }
    }
}
struct Job {
    request: Request,
    reply: mpsc::SyncSender<RpcResult>,
    _activity: Activity,
}
pub(crate) struct AccountCommitter {
    sender: Option<mpsc::SyncSender<Job>>,
    worker: Option<thread::JoinHandle<()>>,
    active: Active,
}
fn error(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}
impl AccountCommitter {
    pub(crate) fn start(
        enabled: bool,
        storage: StorageClient,
        checks: Arc<CheckManager>,
        revision: Arc<AtomicU64>,
    ) -> io::Result<Self> {
        Self::with_vault(enabled, storage, checks, revision, Arc::new(SystemVault))
    }
    pub(crate) fn with_vault<V: CredentialVault + CredentialStaging + Send + Sync + 'static>(
        enabled: bool,
        storage: StorageClient,
        checks: Arc<CheckManager>,
        revision: Arc<AtomicU64>,
        vault: Arc<V>,
    ) -> io::Result<Self> {
        let active = Active::default();
        if !enabled {
            return Ok(Self {
                sender: None,
                worker: None,
                active,
            });
        }
        let (sender, receiver) = mpsc::sync_channel::<Job>(2);
        let worker = thread::Builder::new()
            .name("qingmail-account-commit".into())
            .spawn(move || {
                // 有序退出时完成已接受的提交；前台断连不取消后台已取得的执行权。
                for job in receiver {
                    process(job, &storage, &checks, vault.as_ref(), &revision);
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            active,
        })
    }
    pub(crate) fn submit(&self, request: Request) -> Result<mpsc::Receiver<RpcResult>, RpcError> {
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| error(ErrorCode::Unsupported, "隔离后台未启用账号提交或密钥库访问"))?;
        request
            .validate()
            .map_err(|_| error(ErrorCode::InvalidRequest, "账号提交请求无效"))?;
        if !matches!(request.command, Command::CommitAccount { .. }) {
            return Err(error(ErrorCode::InvalidRequest, "不是账号提交请求"));
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        let activity = Activity {
            id: request.id.clone(),
            active: self.active.clone(),
        };
        *self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(request.id.clone())
            .or_default() += 1;
        sender
            .try_send(Job {
                request,
                reply,
                _activity: activity,
            })
            .map_err(|_| error(ErrorCode::Busy, "账号保存队列繁忙，请稍后重试"))?;
        Ok(receiver)
    }
    pub(crate) fn active(&self, id: &str) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }
    pub(crate) fn has_active(&self) -> bool {
        !self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
    pub(crate) fn stop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for AccountCommitter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn process<V: CredentialVault + CredentialStaging>(
    job: Job,
    storage: &StorageClient,
    checks: &CheckManager,
    vault: &V,
    revision: &AtomicU64,
) {
    let claim = receive(storage.claim_mutation(job.request.clone()))
        .map_err(|e| error(ErrorCode::Storage, e))
        .and_then(|r| r);
    match claim {
        Ok(MutationClaim::Acquired) => (),
        Ok(MutationClaim::Existing(MutationStatus::Finished(result))) => {
            let _ = job.reply.send(*result);
            return;
        }
        Ok(MutationClaim::Existing(_)) => {
            let _ = job
                .reply
                .send(Ok(Reply::Mutation(MutationStatus::Uncertain)));
            return;
        }
        Err(e) => {
            let _ = job.reply.send(Err(e));
            return;
        }
    }
    // 只有日志可靠写盘之后才确认已接受；客户端不需要保持连接等待密钥库。
    let _ = job
        .reply
        .send(Ok(Reply::Mutation(MutationStatus::InProgress)));
    let result = perform(&job.request, storage, checks, vault);
    if result.is_ok() {
        revision.fetch_add(1, Ordering::Release);
        // 成功结果已与业务事务一起提交，不能再以第二个结果写入覆盖它。
    } else {
        let _ = receive(storage.finish_mutation(job.request.clone(), result));
        // 结果不能落盘时保留执行记录；服务在无活动任务时显示 Uncertain，重启也不重放。
    }
}
fn perform<V: CredentialVault + CredentialStaging>(
    request: &Request,
    storage: &StorageClient,
    checks: &CheckManager,
    vault: &V,
) -> RpcResult {
    let Command::CommitAccount { check, account } = &request.command else {
        return Err(error(ErrorCode::InvalidRequest, "不是账号提交请求"));
    };
    let checked = checks.take_checked(check, account)?;
    receive(storage.validate_account_commit(checked.baseline.clone()))
        .map_err(|e| error(ErrorCode::Storage, e))??;
    let account = checked
        .account
        .clone()
        .map(Ok)
        .unwrap_or_else(|| request_id().map(AccountId))
        .map_err(|_| error(ErrorCode::Internal, "无法生成账号标识"))?;
    let key = loop {
        let key =
            AccountId(request_id().map_err(|_| error(ErrorCode::Internal, "无法生成凭据版本"))?);
        if key != account
            && !receive(storage.credential_referenced(key.clone()))
                .map_err(|e| error(ErrorCode::Storage, e))?
        {
            break key;
        }
    };
    // 每次独立的新键，无论密钥库失败或超时都不会覆盖旧账号键。
    vault.save(&key, &checked.credentials).map_err(|e| {
        error(
            if matches!(e, VaultError::Timeout | VaultError::OperationFailed) {
                ErrorCode::Uncertain
            } else {
                ErrorCode::Authentication
            },
            format!("新凭据保存未确认：{e}；旧账号配置与凭据未更改"),
        )
    })?;
    let committed = receive(storage.commit_account_credentials(
        request.clone(),
        account.clone(),
        checked.settings,
        key.clone(),
        checked.baseline,
    ))
    .map_err(|e| error(ErrorCode::Uncertain, e))
    .and_then(|r| r);
    if let Err(e) = committed {
        // 只有数据库证明此键没有任何引用才清理；未知提交结果保留数据。
        if matches!(
            receive(storage.credential_referenced(key.clone())),
            Ok(false)
        ) {
            let _ = vault.delete(&key);
        }
        return Err(e);
    }
    // 清理失败只留下可过期临时项，不把已提交账号降为失败，更不撤销配置。
    let _ = vault.discard_staged(&checked.credential_ref);
    Ok(Reply::AccountSaved(account))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_checks::CheckBackend;
    use crate::storage::StorageWorker;
    use mail_core::{AccountSettings, ConnectionSecurity, ServerSettings};
    use mail_service::{accounts::AccountCheck, credentials::Credentials};
    use std::{
        sync::atomic::AtomicUsize,
        time::{Duration, Instant},
    };

    #[derive(Default)]
    struct FakeVault {
        saved: Mutex<HashMap<String, (String, String)>>,
        calls: AtomicUsize,
        failure: Option<VaultError>,
    }
    impl CredentialVault for FakeVault {
        fn status(&self) -> Result<(), VaultError> {
            Ok(())
        }
        fn save(&self, key: &AccountId, c: &Credentials) -> Result<(), VaultError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.failure == Some(VaultError::Locked) {
                return Err(VaultError::Locked);
            }
            self.saved
                .lock()
                .unwrap()
                .insert(key.0.clone(), (c.imap.to_string(), c.smtp.to_string()));
            self.failure.map_or(Ok(()), Err)
        }
        fn load(&self, key: &AccountId) -> Result<Credentials, VaultError> {
            let saved = self.saved.lock().unwrap();
            let (imap, smtp) = saved.get(&key.0).ok_or(VaultError::Missing)?;
            Credentials::new(imap.clone(), smtp.clone())
        }
        fn delete(&self, key: &AccountId) -> Result<(), VaultError> {
            self.saved.lock().unwrap().remove(&key.0);
            Ok(())
        }
    }
    impl CredentialStaging for FakeVault {
        fn stage(&self, _: &Credentials) -> Result<String, VaultError> {
            Ok("f".repeat(32))
        }
        fn load_staged(&self, _: &str) -> Result<Credentials, VaultError> {
            Credentials::new("new-imap".into(), "new-smtp".into())
        }
        // 即使临时项清理失败，也不回滚已提交的账号。
        fn discard_staged(&self, _: &str) -> Result<(), VaultError> {
            Err(VaultError::Locked)
        }
        fn cleanup_staged(&self) -> Result<u64, VaultError> {
            Ok(0)
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
    fn settings() -> AccountSettings {
        let server = ServerSettings {
            host: "mail.example.test".into(),
            username: "test".into(),
            port: 993,
            security: ConnectionSecurity::Tls,
        };
        AccountSettings {
            display_name: "测试".into(),
            address: "test@example.test".into(),
            imap: server.clone(),
            smtp: server,
        }
    }
    fn checked(checks: &CheckManager, baseline: Option<mail_storage::AccountVersion>) -> String {
        let target = baseline.as_ref().map(|v| v.account.clone());
        let mut changed = settings();
        changed.imap.host = "changed.example.test".into();
        let reply = checks
            .begin(
                &request_id().unwrap(),
                target,
                changed,
                "f".repeat(32),
                baseline,
            )
            .unwrap();
        let Reply::AccountCheckPending { task } = reply else {
            panic!()
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Reply::AccountCheck(result) = checks.poll(&task).unwrap() {
                return result.check.unwrap();
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn result(storage: &StorageClient, request: &Request) -> RpcResult {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let response = receive(storage.rpc(Request::new(
                request_id().unwrap(),
                Command::MutationResult {
                    mutation: request.id.clone(),
                },
            )))
            .unwrap();
            if let Reply::Mutation(MutationStatus::Finished(result)) = response.result.unwrap() {
                return *result;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn lost_response_and_restart_replay_return_one_saved_account_without_new_vault_write() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut storage = StorageWorker::start(Some(data.clone())).unwrap();
        let vault = Arc::new(FakeVault::default());
        let checks = Arc::new(CheckManager::with_backend(true, vault.clone()).unwrap());
        let revision = Arc::new(AtomicU64::new(0));
        let mut commits = AccountCommitter::with_vault(
            true,
            storage.client(),
            checks.clone(),
            revision.clone(),
            vault.clone(),
        )
        .unwrap();
        let request = Request::new(
            "a".repeat(32),
            Command::CommitAccount {
                check: checked(&checks, None),
                account: None,
            },
        );
        drop(commits.submit(request.clone()).unwrap());
        let Reply::AccountSaved(account) = result(&storage.client(), &request).unwrap() else {
            panic!()
        };
        let version = receive(storage.client().account_version(account.clone())).unwrap();
        assert_eq!(version.settings.imap.host, "changed.example.test");
        assert_ne!(version.credential_key, account);
        assert_eq!(
            *CredentialVault::load(vault.as_ref(), &version.credential_key)
                .unwrap()
                .imap,
            "new-imap"
        );
        assert!(
            matches!(commits.submit(request.clone()).unwrap().recv_timeout(Duration::from_secs(2)).unwrap().unwrap(),Reply::AccountSaved(id) if id==account)
        );
        let mut conflict = request.clone();
        conflict.command = Command::CommitAccount {
            check: "b".repeat(32),
            account: None,
        };
        assert_eq!(
            commits
                .submit(conflict)
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        commits.stop();
        assert_eq!(revision.load(Ordering::Acquire), 1);
        assert_eq!(vault.calls.load(Ordering::Relaxed), 1);
        storage.stop();
        let mut storage = StorageWorker::start(Some(data)).unwrap();
        // 新检查管理器没有旧令牌；持久结果仍须可查询，而非重新认证或保存。
        let checks = Arc::new(CheckManager::with_backend(true, vault.clone()).unwrap());
        let mut commits = AccountCommitter::with_vault(
            true,
            storage.client(),
            checks,
            Arc::new(AtomicU64::new(0)),
            vault.clone(),
        )
        .unwrap();
        assert!(
            matches!(commits.submit(request).unwrap().recv_timeout(Duration::from_secs(2)).unwrap().unwrap(),Reply::AccountSaved(id) if id==account)
        );
        assert_eq!(receive(storage.client().accounts()).unwrap().len(), 1);
        assert_eq!(vault.calls.load(Ordering::Relaxed), 1);
        commits.stop();
        storage.stop();
    }
    #[test]
    fn vault_failure_and_database_failure_preserve_old_credentials_and_configuration() {
        for (failure, database_failure) in [
            (Some(VaultError::Locked), false),
            (Some(VaultError::Timeout), false),
            (None, true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            let mut store = mail_storage::Store::open(&data).unwrap();
            let account = store.create_account(&settings()).unwrap();
            let baseline = store.account_version(&account).unwrap();
            drop(store);
            if database_failure {
                rusqlite::Connection::open(data.join("state.sqlite3")).unwrap().execute_batch("CREATE TRIGGER fail_switch BEFORE INSERT ON account_credentials WHEN NEW.credential_key!=NEW.account_id BEGIN SELECT RAISE(ABORT,'full disk'); END;").unwrap();
            }
            let vault = Arc::new(FakeVault {
                failure,
                ..Default::default()
            });
            vault
                .saved
                .lock()
                .unwrap()
                .insert(account.0.clone(), ("old-imap".into(), "old-smtp".into()));
            let mut storage = StorageWorker::start(Some(data)).unwrap();
            let checks = Arc::new(CheckManager::with_backend(true, vault.clone()).unwrap());
            let mut commits = AccountCommitter::with_vault(
                true,
                storage.client(),
                checks.clone(),
                Arc::new(AtomicU64::new(0)),
                vault.clone(),
            )
            .unwrap();
            let request = Request::new(
                "a".repeat(32),
                Command::CommitAccount {
                    check: checked(&checks, Some(baseline.clone())),
                    account: Some(account.clone()),
                },
            );
            let accepted = commits
                .submit(request.clone())
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            assert!(matches!(
                accepted,
                Reply::Mutation(MutationStatus::InProgress)
            ));
            let error = result(&storage.client(), &request).unwrap_err();
            assert_eq!(
                error.code,
                if database_failure {
                    ErrorCode::Storage
                } else if failure == Some(VaultError::Timeout) {
                    ErrorCode::Uncertain
                } else {
                    ErrorCode::Authentication
                }
            );
            assert_eq!(
                receive(storage.client().account_version(account.clone())).unwrap(),
                baseline
            );
            assert_eq!(
                *CredentialVault::load(vault.as_ref(), &account)
                    .unwrap()
                    .imap,
                "old-imap"
            );
            assert_eq!(
                commits
                    .submit(request)
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap_err()
                    .code,
                error.code
            );
            assert_eq!(vault.calls.load(Ordering::Relaxed), 1);
            // 结果不明的密钥库写入保留候选项；DB明确回滚时只清理无引用新键。
            assert_eq!(
                vault.saved.lock().unwrap().len(),
                if failure == Some(VaultError::Timeout) {
                    2
                } else {
                    1
                }
            );
            commits.stop();
            storage.stop();
        }
    }
    #[test]
    fn update_keeps_old_key_and_rejects_another_check_of_the_previous_credential_version() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut store = mail_storage::Store::open(&data).unwrap();
        let account = store.create_account(&settings()).unwrap();
        let baseline = store.account_version(&account).unwrap();
        let draft = store
            .create_draft(
                &account,
                &mail_protocol::DraftDocument {
                    headers: Default::default(),
                    body: mail_content::draft::Draft::plain("旧草稿保留").unwrap(),
                    attachments: vec![],
                },
            )
            .unwrap();
        drop(store);
        let vault = Arc::new(FakeVault::default());
        vault
            .saved
            .lock()
            .unwrap()
            .insert(account.0.clone(), ("old-imap".into(), "old-smtp".into()));
        let mut storage = StorageWorker::start(Some(data.clone())).unwrap();
        let checks = Arc::new(CheckManager::with_backend(true, vault.clone()).unwrap());
        let check = checked(&checks, Some(baseline.clone()));
        let stale = checked(&checks, Some(baseline));
        let mut commits = AccountCommitter::with_vault(
            true,
            storage.client(),
            checks,
            Arc::new(AtomicU64::new(0)),
            vault.clone(),
        )
        .unwrap();
        let request = Request::new(
            "a".repeat(32),
            Command::CommitAccount {
                check,
                account: Some(account.clone()),
            },
        );
        commits
            .submit(request.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(
            matches!(result(&storage.client(),&request).unwrap(),Reply::AccountSaved(id) if id==account)
        );
        let current = receive(storage.client().account_version(account.clone())).unwrap();
        assert_eq!(current.settings.imap.host, "changed.example.test");
        assert_ne!(current.credential_key, account);
        assert_eq!(
            *CredentialVault::load(vault.as_ref(), &account)
                .unwrap()
                .imap,
            "old-imap"
        );
        assert_eq!(
            *CredentialVault::load(vault.as_ref(), &current.credential_key)
                .unwrap()
                .imap,
            "new-imap"
        );
        let stale_request = Request::new(
            "b".repeat(32),
            Command::CommitAccount {
                check: stale,
                account: Some(account.clone()),
            },
        );
        commits
            .submit(stale_request.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(
            result(&storage.client(), &stale_request).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(vault.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            receive(storage.client().account_version(account.clone())).unwrap(),
            current
        );
        commits.stop();
        storage.stop();
        let store = mail_storage::Store::open(&data).unwrap();
        assert_eq!(store.draft(&account, &draft.id).unwrap(), draft);
        assert!(store.credential_keys(&account).unwrap().contains(&account));
    }
    #[test]
    fn interrupted_claim_is_uncertain_after_restart_and_never_consumes_token_or_saves() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let request = Request::new(
            "a".repeat(32),
            Command::CommitAccount {
                check: "b".repeat(32),
                account: None,
            },
        );
        let mut store = mail_storage::Store::open(&data).unwrap();
        store.claim_mutation(&request).unwrap();
        drop(store);
        let mut storage = StorageWorker::start(Some(data)).unwrap();
        let vault = Arc::new(FakeVault::default());
        let checks = Arc::new(CheckManager::with_backend(true, vault.clone()).unwrap());
        let mut commits = AccountCommitter::with_vault(
            true,
            storage.client(),
            checks,
            Arc::new(AtomicU64::new(0)),
            vault.clone(),
        )
        .unwrap();
        assert!(matches!(
            commits
                .submit(request)
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            Reply::Mutation(MutationStatus::Uncertain)
        ));
        assert_eq!(vault.calls.load(Ordering::Relaxed), 0);
        assert!(receive(storage.client().accounts()).unwrap().is_empty());
        commits.stop();
        storage.stop();
    }
}
