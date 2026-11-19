//! IMAP 离线队列独立于 SMTP；只恢复幂等标记，移动越过副作用边界后绝不自动重放。
use crate::storage::{StorageClient, receive};
use mail_protocol::{
    Command, ErrorCode, MailOperation, OperationInfo, OperationState as State, RpcError,
};
use mail_service::{
    accounts::CheckError,
    actions,
    credentials::{CredentialVault, SystemVault},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
pub(crate) struct Operations {
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}
impl Operations {
    pub fn start(
        store: StorageClient,
        enabled: bool,
        paused: Arc<AtomicBool>,
        revision: Arc<AtomicU64>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut this = Self {
            stop: stop.clone(),
            threads: vec![],
        };
        if enabled {
            for _ in 0..2 {
                let (store, stop, paused, revision) = (
                    store.clone(),
                    stop.clone(),
                    paused.clone(),
                    revision.clone(),
                );
                this.threads.push(
                    thread::Builder::new()
                        .name("qingmail-imap-operations".into())
                        .spawn(move || {
                            while !stop.load(Ordering::Acquire) {
                                if !paused.load(Ordering::Acquire)
                                    && let Ok(Some((op, reconcile))) =
                                        receive(store.claim_operation())
                                {
                                    revision.fetch_add(1, Ordering::Release);
                                    let _permit =
                                        crate::network_gate::acquire(&op.location.account, false);
                                    let (state, detail) = if stop.load(Ordering::Acquire)
                                        || paused.load(Ordering::Acquire)
                                    {
                                        (
                                            if reconcile {
                                                State::Uncertain
                                            } else {
                                                State::Pending
                                            },
                                            None,
                                        )
                                    } else {
                                        execute(&store, &op, reconcile)
                                    };
                                    if detail.as_ref().is_some_and(|d| {
                                        matches!(d.code, ErrorCode::Authentication)
                                    }) {
                                        let _ = store.control(Command::PauseSync {
                                            account: Some(op.location.account.clone()),
                                            paused: true,
                                        });
                                    }
                                    // 写盘失败保留 in_flight；重启按性质恢复，绝不把移动当作未执行。
                                    let _ = receive(store.finish_operation(op.id, state, detail));
                                    revision.fetch_add(1, Ordering::Release);
                                } else {
                                    thread::park_timeout(Duration::from_millis(500));
                                }
                            }
                        })?,
                );
            }
        }
        Ok(this)
    }
    pub(crate) fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        for t in &self.threads {
            t.thread().unpark();
        }
    }
    pub fn stop(&mut self) {
        self.request_stop();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
impl Drop for Operations {
    fn drop(&mut self) {
        self.stop();
    }
}
fn detail(code: ErrorCode, message: impl Into<String>) -> Option<RpcError> {
    Some(RpcError {
        code,
        message: message.into(),
    })
}
fn classify(error: CheckError, may_have_changed: bool) -> (State, Option<RpcError>) {
    if may_have_changed {
        return (
            State::Uncertain,
            detail(
                ErrorCode::Uncertain,
                format!("移动可能已执行，禁止重试；请核对源和目标文件夹：{error}"),
            ),
        );
    }
    match error {
        CheckError::EpochChanged => (
            State::Conflict,
            detail(ErrorCode::Conflict, error.to_string()),
        ),
        CheckError::Authentication => (
            State::Failed,
            detail(ErrorCode::Authentication, error.to_string()),
        ),
        CheckError::Dns | CheckError::Connect | CheckError::Timeout => (
            State::Pending,
            detail(
                ErrorCode::Offline,
                format!("{error}；最多3次安全尝试，之后可手动重试"),
            ),
        ),
        _ => (
            State::Failed,
            detail(ErrorCode::InvalidRequest, error.to_string()),
        ),
    }
}
fn execute(
    store: &StorageClient,
    op: &OperationInfo,
    reconcile: bool,
) -> (State, Option<RpcError>) {
    let result = (|| -> Result<_, String> {
        let access = receive(store.account_version(op.location.account.clone()))?;
        let credentials = SystemVault
            .load(&access.credential_key)
            .map_err(|e| e.to_string())?;
        let folders = receive(store.folders(op.location.account.clone()))?;
        let source = folders
            .iter()
            .find(|f| f.id.0 == op.location.folder_id && f.uidvalidity == op.location.uidvalidity)
            .ok_or("邮件UIDVALIDITY已变化")?
            .remote_name
            .clone();
        let destination = match &op.operation {
            MailOperation::Move { destination } => Some(
                folders
                    .iter()
                    .find(|f| f.id == *destination)
                    .ok_or("目标文件夹已变化")?
                    .remote_name
                    .clone(),
            ),
            MailOperation::Trash { destination } => Some(destination.clone()),
            _ => None,
        };
        Ok((access, credentials, source, destination))
    })();
    let (access, credentials, source, destination) = match result {
        Ok(v) => v,
        Err(e) => {
            return (
                if reconcile {
                    State::Uncertain
                } else {
                    State::Failed
                },
                detail(ErrorCode::Storage, e),
            );
        }
    };
    let server = &access.settings.imap;
    let password = &credentials.imap;
    let epoch = op.location.uidvalidity;
    let uid = op.location.uid;
    let flags = match actions::probe(server, password, &source, epoch, uid) {
        Ok(v) => v,
        Err(e) => {
            if e == CheckError::Authentication {
                let _ = store.control(Command::PauseSync {
                    account: Some(op.location.account.clone()),
                    paused: true,
                });
            }
            // 核对请求仅能读，不得借错误路径把不确定移动改回可执行。
            if reconcile {
                return (
                    State::Uncertain,
                    detail(ErrorCode::Uncertain, format!("只读核对未完成：{e}")),
                );
            }
            return classify(e, false);
        }
    };
    if reconcile {
        return match (&op.operation, flags) {
            (MailOperation::Flags { seen, flagged }, Some(actual))
                if seen.is_none_or(|v| actual.seen == Some(v))
                    && flagged.is_none_or(|v| actual.flagged == Some(v)) =>
            {
                (State::Applied, None)
            }
            (MailOperation::Delete, None) => (State::Applied, None),
            (_, Some(_)) => (
                State::Uncertain,
                detail(
                    ErrorCode::Uncertain,
                    "只读核对：源邮件仍存在，但目标可能已有副本，不能证明未执行。请查看两端后选择保留服务器状态",
                ),
            ),
            (_, None) => (
                State::Uncertain,
                detail(
                    ErrorCode::Uncertain,
                    "只读核对：源邮件已不存在，无法证明目标副本归属。请查看目标后选择保留服务器状态",
                ),
            ),
        };
    }
    if flags.is_none()
        && matches!(
            op.operation,
            MailOperation::Delete | MailOperation::Trash { .. }
        )
    {
        // A delete intent is already fulfilled if the exact source UID no longer exists.
        return (State::Applied, None);
    }
    if flags.is_none() {
        return (
            State::Conflict,
            detail(ErrorCode::Conflict, "远端邮件已删除或移动，停止旧UID操作"),
        );
    }
    match &op.operation {
        MailOperation::Flags { seen, flagged } => {
            match actions::store_flags(
                server,
                password,
                &source,
                epoch,
                &[uid],
                actions::FlagChange {
                    seen: *seen,
                    flagged: *flagged,
                },
            ) {
                Ok(()) => match actions::probe(server, password, &source, epoch, uid) {
                    Ok(Some(actual))
                        if seen.is_none_or(|v| actual.seen == Some(v))
                            && flagged.is_none_or(|v| actual.flagged == Some(v)) =>
                    {
                        (State::Applied, None)
                    }
                    Ok(_) => (
                        State::Conflict,
                        detail(ErrorCode::Conflict, "服务器标记核对不一致，请同步后处理"),
                    ),
                    Err(e) => classify(e, false),
                },
                Err(e) => classify(e, false),
            }
        }
        MailOperation::Move { .. } => match actions::move_checked(
            server,
            password,
            &source,
            epoch,
            &[uid],
            destination.as_deref().unwrap_or(""),
        ) {
            Ok(actions::MoveOutcome::Moved) => (State::Applied, None),
            Ok(actions::MoveOutcome::CopiedOnly) => (
                State::Uncertain,
                detail(
                    ErrorCode::Uncertain,
                    "已复制并标记删除，但未确认源移除；请核对，不自动重放",
                ),
            ),
            Err(e) => classify(e.error, e.may_have_changed),
        },
        MailOperation::Trash { destination } => {
            match actions::move_checked(server, password, &source, epoch, &[uid], destination) {
                Ok(actions::MoveOutcome::Moved) => (State::Applied, None),
                Ok(actions::MoveOutcome::CopiedOnly) => (
                    State::Uncertain,
                    detail(
                        ErrorCode::Uncertain,
                        "已复制至垃圾箱但源移除未确认，请核对，不自动重放",
                    ),
                ),
                Err(e) => classify(e.error, e.may_have_changed),
            }
        }
        MailOperation::Delete => {
            match actions::delete_checked(server, password, &source, epoch, &[uid]) {
                Ok(()) => (State::Applied, None),
                Err(e) => classify(e.error, e.may_have_changed),
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lost_move_response_never_becomes_retryable_and_flags_are_safe() {
        assert_eq!(classify(CheckError::Timeout, true).0, State::Uncertain);
        assert_eq!(classify(CheckError::Connect, false).0, State::Pending);
        assert_eq!(classify(CheckError::EpochChanged, false).0, State::Conflict);
        assert_eq!(classify(CheckError::Authentication, false).0, State::Failed);
    }
}
