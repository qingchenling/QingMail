//! 预览令牌与显式发送命令。只恢复结果，不扫描/重放发送请求。
use super::rpc_error;
use crate::{
    sending::{SendCoordinator, SendReview},
    storage::{StorageClient, receive},
};
use mail_protocol::*;
use mail_storage::MutationClaim;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
const TTL: Duration = Duration::from_secs(300);
struct State {
    reviews: HashMap<String, (Instant, SendReview)>,
    /// 同时仅一个 SMTP/副本任务；保留原始请求用于拒绝相同ID不同参数。
    active: Option<Request>,
    worker: Option<thread::JoinHandle<()>>,
}
pub(super) struct Sending {
    state: Arc<Mutex<State>>,
    storage: StorageClient,
    enabled: bool,
    revision: Arc<AtomicU64>,
}
impl Sending {
    pub fn new(storage: StorageClient, enabled: bool, revision: Arc<AtomicU64>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                reviews: HashMap::new(),
                active: None,
                worker: None,
            })),
            storage,
            enabled,
            revision,
        }
    }
    pub fn active(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .is_some()
    }
    pub fn owns(&self, id: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .as_ref()
            .is_some_and(|r| r.id == id)
    }
    pub fn review(
        &self,
        account: mail_core::AccountId,
        draft: mail_core::DraftId,
        revision: u32,
    ) -> Result<Reply, RpcError> {
        let review = receive(self.storage.review_version(account, draft, revision))
            .map_err(|e| rpc_error(ErrorCode::Conflict, e))?;
        let token =
            transport::request_id().map_err(|e| rpc_error(ErrorCode::Internal, e.to_string()))?;
        let preview = SendPreview {
            token: token.clone(),
            sender: review.sender.clone(),
            headers: review.headers.clone(),
            attachments: review.attachments,
            attachment_bytes: review.attachment_bytes,
            preview: review.preview.clone(),
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.reviews.retain(|_, (time, _)| time.elapsed() < TTL);
        if state.reviews.len() >= 16 {
            return Err(rpc_error(ErrorCode::Busy, "发送预览过多，请稍后重新检查"));
        }
        state.reviews.insert(token, (Instant::now(), review));
        Ok(Reply::SendPreview(preview))
    }
    pub fn submit(&self, request: Request) -> Result<Reply, RpcError> {
        if !self.enabled {
            return Err(rpc_error(
                ErrorCode::Unsupported,
                "隔离后台禁止发送及访问密钥库",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = &state.active {
            if active.id != request.id {
                return Err(rpc_error(ErrorCode::Busy, "另一发送或副本任务正在处理"));
            }
            if serde_json::to_vec(active).ok() != serde_json::to_vec(&request).ok() {
                return Err(rpc_error(ErrorCode::Conflict, "请求ID已用于不同操作"));
            }
            return Ok(Reply::Mutation(MutationStatus::InProgress));
        }
        if let Some(worker) = state.worker.take() {
            let _ = worker.join();
        }
        let claim = receive(self.storage.claim_mutation(request.clone()))
            .map_err(|e| rpc_error(ErrorCode::Storage, e))??;
        match claim {
            MutationClaim::Existing(MutationStatus::Finished(result)) => return *result,
            MutationClaim::Existing(_) => {
                return Err(rpc_error(
                    ErrorCode::Uncertain,
                    "发送确认曾被处理或中断；请核对发送记录，禁止重新执行",
                ));
            }
            MutationClaim::Acquired => (),
        }
        let review = if let Command::ConfirmSend { review, .. } = &request.command {
            match state.reviews.remove(review) {
                Some((created, value)) if created.elapsed() < TTL => Some(value),
                _ => {
                    let result = Err(rpc_error(
                        ErrorCode::Conflict,
                        "预览已失效或已确认，请核对原发送记录",
                    ));
                    receive(self.storage.finish_mutation(request, result.clone()))
                        .map_err(|e| rpc_error(ErrorCode::Storage, e))??;
                    return result;
                }
            }
        } else {
            None
        };
        state.active = Some(request.clone());
        let shared = self.state.clone();
        let storage = self.storage.clone();
        let revision = self.revision.clone();
        let operation = request.clone();
        match thread::Builder::new()
            .name("qingmail-send-ipc".into())
            .spawn(move || {
                let coordinator = SendCoordinator::default();
                let result = (|| {
                    let receiver = match operation.command.clone() {
                        Command::ConfirmSend {
                            allow_empty_subject,
                            ..
                        } => coordinator.confirm(
                            storage.clone(),
                            review.ok_or("发送预览缺失")?,
                            allow_empty_subject,
                        ),
                        Command::Resend { account, task } => {
                            coordinator.resend(storage.clone(), account, task)
                        }
                        Command::RetryCopy { account, task } => {
                            coordinator.retry_copy(storage.clone(), account, task)
                        }
                        _ => Err("不支持的发送命令".into()),
                    }?;
                    receiver
                        .recv()
                        .map_err(|_| "发送工作线程中断，必须核对记录，不会自动重发".to_string())?
                })()
                .map(Reply::Task)
                .map_err(|e| rpc_error(ErrorCode::Uncertain, e));
                let _ = receive(storage.finish_mutation(operation, result));
                revision.fetch_add(1, Ordering::Release);
                shared.lock().unwrap_or_else(|e| e.into_inner()).active = None;
            }) {
            Ok(worker) => {
                state.worker = Some(worker);
                Ok(Reply::Mutation(MutationStatus::InProgress))
            }
            Err(e) => {
                state.active = None;
                let result = Err(rpc_error(
                    ErrorCode::Internal,
                    format!("未能启动发送工作线程：{e}"),
                ));
                receive(self.storage.finish_mutation(request, result.clone()))
                    .map_err(|e| rpc_error(ErrorCode::Storage, e))??;
                result
            }
        }
    }
    pub fn stop(&mut self) {
        // 不持有状态锁等待：工作线程完成时需要同一锁发布结果。
        let worker = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .worker
            .take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}
impl Drop for Sending {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageWorker;
    use mail_core::*;
    fn setup() -> (tempfile::TempDir, StorageWorker, mail_storage::StoredDraft) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut store = mail_storage::Store::open(&path).unwrap();
        let server = ServerSettings {
            authentication: Default::default(),
            host: "example.invalid".into(),
            port: 993,
            username: "synthetic".into(),
            security: ConnectionSecurity::Tls,
        };
        let account = store
            .create_account(&AccountSettings {
                display_name: "synthetic".into(),
                address: "sender@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        let draft = store
            .create_draft(
                &account,
                &mail_storage::DraftDocument {
                    headers: DraftHeaders {
                        subject: "预览".into(),
                        to: vec!["to@example.invalid".into()],
                        ..Default::default()
                    },
                    body: mail_content::draft::Draft::plain("不访问密钥库或SMTP").unwrap(),
                    attachments: vec![],
                },
            )
            .unwrap();
        drop(store);
        (root, StorageWorker::start(Some(path)).unwrap(), draft)
    }
    #[test]
    fn preview_has_expiry_and_limits_and_never_creates_send_permission() {
        let (_root, mut worker, draft) = setup();
        let sending = Sending::new(worker.client(), false, Default::default());
        let Reply::SendPreview(preview) = sending
            .review(draft.account.clone(), draft.id.clone(), draft.revision)
            .unwrap()
        else {
            panic!()
        };
        assert!(valid_id(&preview.token));
        assert_eq!(preview.sender, "sender@example.invalid");
        assert!(
            sending
                .review(draft.account.clone(), draft.id.clone(), draft.revision + 1)
                .is_err()
        );
        for _ in 1..16 {
            sending
                .review(draft.account.clone(), draft.id.clone(), draft.revision)
                .unwrap();
        }
        assert!(
            sending
                .review(draft.account.clone(), draft.id.clone(), draft.revision)
                .is_err()
        );
        let request = Request::new(
            "a".repeat(32),
            Command::ConfirmSend {
                review: preview.token.clone(),
                allow_empty_subject: false,
            },
        );
        assert_eq!(
            sending.submit(request).unwrap_err().code,
            ErrorCode::Unsupported
        );
        assert!(
            receive(worker.client().tasks(draft.account.clone()))
                .unwrap()
                .is_empty()
        );
        drop(sending);
        worker.stop();
    }
    #[test]
    fn expired_preview_and_interrupted_mutation_never_execute_or_replay() {
        let (_root, mut worker, draft) = setup();
        // enabled=true 仅在以下测试失效令牌/孤立日志分支，永不进入凭据或网络执行。
        let mut sending = Sending::new(worker.client(), true, Default::default());
        let Reply::SendPreview(preview) = sending
            .review(draft.account.clone(), draft.id.clone(), draft.revision)
            .unwrap()
        else {
            panic!()
        };
        sending
            .state
            .lock()
            .unwrap()
            .reviews
            .get_mut(&preview.token)
            .unwrap()
            .0 = Instant::now() - TTL;
        let request = Request::new(
            "b".repeat(32),
            Command::ConfirmSend {
                review: preview.token,
                allow_empty_subject: false,
            },
        );
        assert_eq!(
            sending.submit(request.clone()).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            sending.submit(request).unwrap_err().code,
            ErrorCode::Conflict
        );
        let interrupted = Request::new(
            "c".repeat(32),
            Command::Resend {
                account: draft.account.clone(),
                task: SendTaskId("d".repeat(32)),
            },
        );
        assert!(matches!(
            receive(worker.client().claim_mutation(interrupted.clone()))
                .unwrap()
                .unwrap(),
            MutationClaim::Acquired
        ));
        assert_eq!(
            sending.submit(interrupted).unwrap_err().code,
            ErrorCode::Uncertain
        );
        assert!(!sending.active());
        assert!(
            receive(worker.client().tasks(draft.account))
                .unwrap()
                .is_empty()
        );
        sending.stop();
        worker.stop();
    }
}
