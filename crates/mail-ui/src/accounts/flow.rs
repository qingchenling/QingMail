//! 账号页后台工作流：前台仅写临时凭据；提交结果丢失只查询，不重放。
use mail_core::{AccountId, AccountSettings};
use mail_protocol::{
    AccountCheckResult, Command, ErrorCode, MutationStatus, Reply, Request, RpcError,
    transport::{Client, ClientError, request_id},
};
use mail_service::credentials::{Credentials, staging::CredentialStaging};
use std::time::{Duration, Instant};

pub(super) enum Failure {
    Remote(RpcError),
    Transport(String),
}
pub(super) trait Endpoint {
    fn call(&self, request: Request) -> Result<Reply, Failure>;
}
impl Endpoint for mail_daemon::account_gateway::AccountGateway {
    fn call(&self, request: Request) -> Result<Reply, Failure> {
        self.call(request).map_err(Failure::Remote)
    }
}
// 切换正式独立后台时沿用相同工作流，只替换端点。
impl Endpoint for Client {
    fn call(&self, request: Request) -> Result<Reply, Failure> {
        self.call(&request).map_err(|e| match e {
            ClientError::Remote(e) => Failure::Remote(e),
            other => Failure::Transport(other.to_string()),
        })
    }
}
impl Endpoint for crate::lifecycle::AccountBackend {
    fn call(&self, request: Request) -> Result<Reply, Failure> {
        match self {
            crate::lifecycle::AccountBackend::Local(backend) => {
                backend.call(request).map_err(Failure::Remote)
            }
            crate::lifecycle::AccountBackend::Remote(client, _) => Endpoint::call(client, request),
        }
    }
}
pub(super) fn message(error: Failure) -> String {
    match error {
        Failure::Remote(e) => e.message,
        Failure::Transport(e) => e,
    }
}
fn request(command: Command) -> Result<Request, String> {
    Ok(Request::new(
        request_id().map_err(|e| e.to_string())?,
        command,
    ))
}
pub(super) fn check(
    endpoint: &impl Endpoint,
    vault: &impl CredentialStaging,
    account: Option<AccountId>,
    settings: AccountSettings,
    credentials: Credentials,
) -> Result<AccountCheckResult, String> {
    let reference = vault.stage(&credentials).map_err(|e| e.to_string())?;
    drop(credentials);
    check_reference(endpoint, vault, account, settings, &reference)
}
pub(super) fn check_reference(
    endpoint: &impl Endpoint,
    vault: &impl CredentialStaging,
    account: Option<AccountId>,
    settings: AccountSettings,
    reference: &str,
) -> Result<AccountCheckResult, String> {
    let result = (|| {
        let initial = request(Command::CheckAccount {
            account,
            settings,
            credential_ref: reference.to_owned(),
        })?;
        let mut reply = endpoint.call(initial).map_err(message)?;
        let until = Instant::now() + Duration::from_secs(120);
        loop {
            match reply {
                Reply::AccountCheck(result) => return Ok(result),
                Reply::AccountCheckPending { task } if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(250));
                    reply = endpoint
                        .call(request(Command::AccountCheckStatus { task })?)
                        .map_err(message)?;
                }
                Reply::AccountCheckPending { .. } => return Err("连接检查超时，未提交账号".into()),
                _ => return Err("后台返回了非预期检查结果，未提交账号".into()),
            }
        }
    })();
    // 后台检查完成后持有实际验证过的凭据，提交不重新信任临时项。
    // 窗口关闭/检查失败也尽力删除；无法清理的项由 TTL 限制。
    let _ = vault.discard_staged(reference);
    result
}
/// Polling and account checks run outside GPUI. Cancel never commits an account.
pub(super) fn microsoft(
    endpoint: &impl Endpoint,
    vault: &impl CredentialStaging,
    account: Option<AccountId>,
    settings: AccountSettings,
    session: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<AccountCheckResult, String> {
    let result = (|| {
        let until = Instant::now() + Duration::from_secs(305);
        while !cancel.load(std::sync::atomic::Ordering::Acquire) && Instant::now() < until {
            match endpoint
                .call(request(Command::MicrosoftLoginStatus {
                    session: session.into(),
                })?)
                .map_err(message)?
            {
                Reply::MicrosoftLoginReady { credential_ref } => {
                    let result =
                        check_reference(endpoint, vault, account, settings, &credential_ref);
                    return if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        Err("已取消登录，未保存账号".into())
                    } else {
                        result
                    };
                }
                Reply::MicrosoftLoginPending => std::thread::sleep(Duration::from_millis(250)),
                _ => return Err("微软登录返回了非预期结果".into()),
            }
        }
        Err("微软登录已取消或超时，未保存账号".into())
    })();
    if let Ok(request) = request(Command::CancelMicrosoftLogin {
        session: session.into(),
    }) {
        let _ = endpoint.call(request);
    }
    result
}
pub(super) enum SaveResult {
    Finished(Result<AccountId, RpcError>),
    Unconfirmed(String),
}
fn decode(reply: Result<Reply, Failure>) -> SaveResult {
    match reply {
        Ok(Reply::AccountSaved(account)) => SaveResult::Finished(Ok(account)),
        Ok(Reply::Mutation(MutationStatus::Finished(result))) => match *result {
            Ok(Reply::AccountSaved(account)) => SaveResult::Finished(Ok(account)),
            Err(e) => SaveResult::Finished(Err(e)),
            _ => SaveResult::Unconfirmed("后台保存结果类型不匹配".into()),
        },
        Ok(Reply::Mutation(MutationStatus::InProgress)) => {
            SaveResult::Unconfirmed("后台仍在保存".into())
        }
        Ok(Reply::Mutation(MutationStatus::Uncertain)) => {
            SaveResult::Unconfirmed("原请求执行结果不确定，请核对账号列表；禁止重复提交".into())
        }
        Ok(Reply::Mutation(MutationStatus::NotFound)) => {
            SaveResult::Unconfirmed("暂未找到原请求记录；不能据此重新提交".into())
        }
        Err(Failure::Remote(e)) if e.code != ErrorCode::Uncertain => SaveResult::Finished(Err(e)),
        Err(e) => SaveResult::Unconfirmed(message(e)),
        _ => SaveResult::Unconfirmed("后台返回了非预期保存结果".into()),
    }
}
pub(super) fn save(endpoint: &impl Endpoint, id: &str, initial: Option<Request>) -> SaveResult {
    // 唯一写调用；随后所有请求都是查询原 ID。
    if let Some(initial) = initial {
        let result = decode(endpoint.call(initial));
        if matches!(result, SaveResult::Finished(_)) {
            return result;
        }
    }
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let query = match request(Command::MutationResult {
            mutation: id.into(),
        }) {
            Ok(query) => query,
            Err(e) => return SaveResult::Unconfirmed(e),
        };
        let reply = endpoint.call(query);
        // 查询失败不能当作原提交失败（例如网络中断或存储暂时不可读）。
        if let Err(e) = reply {
            return SaveResult::Unconfirmed(message(e));
        }
        let pending = matches!(reply, Ok(Reply::Mutation(MutationStatus::InProgress)));
        let result = decode(reply);
        if !pending || Instant::now() >= until {
            return result;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_core::{ConnectionSecurity, ServerSettings};
    use mail_service::credentials::VaultError;
    use std::{cell::RefCell, collections::VecDeque, sync::Mutex};

    struct FakeEndpoint {
        replies: RefCell<VecDeque<Result<Reply, Failure>>>,
        requests: RefCell<Vec<Request>>,
    }
    impl FakeEndpoint {
        fn new(replies: Vec<Result<Reply, Failure>>) -> Self {
            Self {
                replies: RefCell::new(replies.into()),
                requests: RefCell::new(vec![]),
            }
        }
    }
    impl Endpoint for FakeEndpoint {
        fn call(&self, request: Request) -> Result<Reply, Failure> {
            self.requests.borrow_mut().push(request);
            self.replies
                .borrow_mut()
                .pop_front()
                .expect("unexpected extra request")
        }
    }
    #[derive(Default)]
    struct Staging {
        discarded: Mutex<Vec<String>>,
        locked: bool,
    }
    impl CredentialStaging for Staging {
        fn stage(&self, credentials: &Credentials) -> Result<String, VaultError> {
            assert_eq!(&*credentials.imap, "input-secret");
            if self.locked {
                Err(VaultError::Locked)
            } else {
                Ok("a".repeat(32))
            }
        }
        fn load_staged(&self, _: &str) -> Result<Credentials, VaultError> {
            panic!("frontend must not read credentials back")
        }
        fn discard_staged(&self, reference: &str) -> Result<(), VaultError> {
            self.discarded.lock().unwrap().push(reference.into());
            Ok(())
        }
        fn cleanup_staged(&self) -> Result<u64, VaultError> {
            panic!("not part of form submission")
        }
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
    fn credentials() -> Credentials {
        Credentials::new("input-secret".into(), "other-secret".into()).unwrap()
    }
    fn commit() -> Request {
        Request::new(
            "b".repeat(32),
            Command::CommitAccount {
                check: "c".repeat(32),
                account: None,
            },
        )
    }
    #[test]
    fn check_transmits_reference_only_and_cleans_staging_on_success_or_failure() {
        let endpoint = FakeEndpoint::new(vec![
            Ok(Reply::AccountCheckPending {
                task: "d".repeat(32),
            }),
            Ok(Reply::AccountCheck(AccountCheckResult {
                check: Some("d".repeat(32)),
                imap: Ok(vec![]),
                smtp: Ok(()),
            })),
        ]);
        let vault = Staging::default();
        assert!(
            check(&endpoint, &vault, None, settings(), credentials())
                .unwrap()
                .check
                .is_some()
        );
        let requests = endpoint.requests.borrow();
        assert!(
            matches!(&requests[0].command, Command::CheckAccount { credential_ref, .. } if credential_ref == &"a".repeat(32))
        );
        assert!(matches!(
            &requests[1].command,
            Command::AccountCheckStatus { .. }
        ));
        assert_eq!(*vault.discarded.lock().unwrap(), vec!["a".repeat(32)]);
        let endpoint = FakeEndpoint::new(vec![Err(Failure::Transport("lost".into()))]);
        assert!(check(&endpoint, &vault, None, settings(), credentials()).is_err());
        assert_eq!(vault.discarded.lock().unwrap().len(), 2);
        let endpoint = FakeEndpoint::new(vec![]);
        let vault = Staging {
            locked: true,
            ..Default::default()
        };
        assert!(check(&endpoint, &vault, None, settings(), credentials()).is_err());
        assert!(endpoint.requests.borrow().is_empty());
    }
    #[test]
    fn lost_commit_response_queries_original_id_and_manual_query_never_resubmits() {
        let account = AccountId("e".repeat(32));
        let endpoint = FakeEndpoint::new(vec![
            Err(Failure::Transport("response lost".into())),
            Ok(Reply::Mutation(MutationStatus::Finished(Box::new(Ok(
                Reply::AccountSaved(account.clone()),
            ))))),
            Ok(Reply::Mutation(MutationStatus::Finished(Box::new(Ok(
                Reply::AccountSaved(account.clone()),
            ))))),
        ]);
        let initial = commit();
        assert!(
            matches!(save(&endpoint, &initial.id, Some(initial.clone())), SaveResult::Finished(Ok(id)) if id == account)
        );
        assert!(
            matches!(save(&endpoint, &initial.id, None), SaveResult::Finished(Ok(id)) if id == account)
        );
        let requests = endpoint.requests.borrow();
        assert_eq!(
            requests
                .iter()
                .filter(|r| matches!(r.command, Command::CommitAccount { .. }))
                .count(),
            1
        );
        assert!(requests[1..].iter().all(|r| matches!(&r.command, Command::MutationResult { mutation } if mutation == &initial.id)));
    }
    #[test]
    fn unknown_or_failed_queries_do_not_clear_pending_save_but_terminal_results_do() {
        for reply in [
            Ok(Reply::Mutation(MutationStatus::Uncertain)),
            Ok(Reply::Mutation(MutationStatus::NotFound)),
            Ok(Reply::Acknowledged),
            Err(Failure::Remote(RpcError {
                code: ErrorCode::Storage,
                message: "temporarily unavailable".into(),
            })),
        ] {
            let endpoint = FakeEndpoint::new(vec![reply]);
            assert!(matches!(
                save(&endpoint, &commit().id, None),
                SaveResult::Unconfirmed(_)
            ));
        }
        let endpoint = FakeEndpoint::new(vec![Ok(Reply::Mutation(MutationStatus::Finished(
            Box::new(Err(RpcError {
                code: ErrorCode::Conflict,
                message: "expired baseline".into(),
            })),
        )))]);
        assert!(
            matches!(save(&endpoint, &commit().id, None), SaveResult::Finished(Err(e)) if e.code == ErrorCode::Conflict)
        );
    }

    use std::sync::atomic::AtomicBool;
    #[test]
    fn microsoft_reference_reuses_checks_and_cleans_up_session_without_committing() {
        let endpoint = FakeEndpoint::new(vec![
            Ok(Reply::MicrosoftLoginReady {
                credential_ref: "d".repeat(32),
            }),
            Ok(Reply::AccountCheck(AccountCheckResult {
                check: Some("e".repeat(32)),
                imap: Ok(vec![]),
                smtp: Ok(()),
            })),
            Ok(Reply::Acknowledged),
        ]);
        let vault = Staging::default();
        let settings = mail_service::microsoft::settings(
            "Test",
            "test@outlook.com",
            mail_core::Authentication::MicrosoftPersonal,
        )
        .unwrap();
        let result = microsoft(
            &endpoint,
            &vault,
            None,
            settings,
            &"f".repeat(32),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(result.check.is_some());
        let requests = endpoint.requests.borrow();
        assert!(matches!(
            &requests[0].command,
            Command::MicrosoftLoginStatus { .. }
        ));
        assert!(
            matches!(&requests[1].command, Command::CheckAccount { credential_ref, .. } if credential_ref == &"d".repeat(32))
        );
        assert!(matches!(
            &requests[2].command,
            Command::CancelMicrosoftLogin { .. }
        ));
        assert_eq!(requests.len(), 3);
        assert_eq!(*vault.discarded.lock().unwrap(), vec!["d".repeat(32)]);
    }
    #[test]
    fn microsoft_cancel_never_checks_or_commits_account() {
        let endpoint = FakeEndpoint::new(vec![Ok(Reply::Acknowledged)]);
        let vault = Staging::default();
        let settings = mail_service::microsoft::settings(
            "Test",
            "test@outlook.com",
            mail_core::Authentication::MicrosoftPersonal,
        )
        .unwrap();
        assert!(
            microsoft(
                &endpoint,
                &vault,
                None,
                settings,
                &"f".repeat(32),
                &AtomicBool::new(true)
            )
            .is_err()
        );
        let requests = endpoint.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0].command,
            Command::CancelMicrosoftLogin { .. }
        ));
    }
}
