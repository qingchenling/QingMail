//! 单进程一次发送协调；存储线程发许可，独立线程做凭据、网络与已发送副本。
use crate::storage::{StorageClient, StorageResult, receive};
use mail_core::{AccountId, DraftHeaders, DraftId, SendState, SendTaskId, ServerSettings};
use mail_service::{
    credentials::{CredentialVault, SystemVault},
    outgoing, sent,
};
use mail_storage::{SendTask, SmtpAttempt, Store, StoredDraft};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
};

/// 无公开构造器；来自已持久化且校验过的特定草稿版本。
pub struct SendReview {
    pub sender: String,
    pub headers: DraftHeaders,
    pub attachments: usize,
    pub attachment_bytes: u64,
    pub preview: String,
    pub(crate) account: AccountId,
    pub(crate) draft: DraftId,
    pub(crate) revision: u32,
    settings: mail_core::AccountSettings,
}
pub(crate) struct ReadySend {
    attempt: SmtpAttempt,
    message: outgoing::PreparedMessage,
    server: mail_core::ServerSettings,
}
pub(crate) fn review(store: &Store, draft: &StoredDraft) -> StorageResult<SendReview> {
    if !store
        .draft_tasks(&draft.account, &draft.id)
        .map_err(|e| e.to_string())?
        .is_empty()
    {
        return Err(
            "此草稿已有发送记录，请查看任务状态；不会重复提交。需要另一封邮件时请新建草稿".into(),
        );
    }
    let snapshot = store
        .draft_send_snapshot(&draft.account, &draft.id, draft.revision)
        .map_err(|e| e.to_string())?;
    // 空主题可预览，但确认步骤必须显式允许；这里不创建任务也不联网。
    outgoing::prepare(&snapshot, true).map_err(|e| e.to_string())?;
    Ok(SendReview {
        sender: snapshot.sender,
        headers: snapshot.headers,
        attachments: snapshot.attachments.len(),
        attachment_bytes: snapshot
            .attachments
            .iter()
            .map(|a| u64::from(a.info.size))
            .sum(),
        preview: snapshot.plain.chars().take(180).collect(),
        account: draft.account.clone(),
        draft: draft.id.clone(),
        revision: draft.revision,
        settings: store
            .account(&draft.account)
            .map_err(|e| e.to_string())?
            .ok_or("发件账号不存在")?,
    })
}
pub(crate) fn begin(
    store: &mut Store,
    review: SendReview,
    allow_empty_subject: bool,
) -> StorageResult<ReadySend> {
    // 在同一存储请求内重新校验版本、附件及历史，重复确认不能获得第二个许可。
    let draft = store
        .draft(&review.account, &review.draft)
        .map_err(|e| e.to_string())?;
    if draft.revision != review.revision {
        return Err("草稿已变更，请重新检查发送内容".into());
    }
    let _ = self::review(store, &draft)?;
    let snapshot = store
        .draft_send_snapshot(&draft.account, &draft.id, draft.revision)
        .map_err(|e| e.to_string())?;
    outgoing::prepare(&snapshot, allow_empty_subject).map_err(|e| e.to_string())?;
    let settings = store
        .account(&draft.account)
        .map_err(|e| e.to_string())?
        .ok_or("发件账号不存在")?;
    if settings != review.settings {
        return Err("发件账号配置已改变，请重新检查发送内容".into());
    }
    let server = settings.smtp;
    let id = store
        .enqueue_draft(&draft.account, &draft.id, draft.revision)
        .map_err(|e| e.to_string())?;
    let frozen = store
        .task_snapshot(&draft.account, &id)
        .map_err(|e| e.to_string())?;
    let message = match outgoing::prepare(&frozen, allow_empty_subject) {
        Ok(message) => message,
        Err(e) => {
            let _ = store.cancel_pending(&draft.account, &id);
            return Err(e.to_string());
        }
    };
    let attempt = store
        .begin_smtp(&draft.account, &id)
        .map_err(|e| e.to_string())?;
    Ok(ReadySend {
        attempt,
        message,
        server,
    })
}

fn copy_outcome(result: sent::CopyResult) -> mail_storage::CopyOutcome {
    match result {
        sent::CopyResult::Saved(saved) => mail_storage::CopyOutcome::Saved {
            folder: saved.folder,
            uidvalidity: saved.uidvalidity,
            uid: saved.uid,
            existed: saved.existed,
        },
        // 只有网络中断等不可确认分支进入 Uncertain；失败与不确定都保持 CopyPending。
        sent::CopyResult::Failed(error) if error.is_uncertain() => {
            mail_storage::CopyOutcome::Uncertain { code: error.code() }
        }
        sent::CopyResult::Failed(error) => mail_storage::CopyOutcome::Failed { code: error.code() },
    }
}

/// 用冻结快照确定性重建同一份 MIME，再交给副本适配；不涉及 SMTP。
fn save_copy(
    store: &StorageClient,
    account: &AccountId,
    task: &SendTaskId,
    imap_password: &str,
    copy: impl FnOnce(&ServerSettings, &str, &str, &[u8], Option<&str>) -> sent::CopyResult,
) -> StorageResult<SendTask> {
    let ready = receive(store.copy_ready(account.clone(), task.clone()))?;
    let message = match outgoing::prepare(&ready.snapshot, true) {
        Ok(message) => message,
        Err(_) => {
            receive(store.finish_copy(
                account.clone(),
                task.clone(),
                mail_storage::CopyOutcome::Failed {
                    code: "copy_prepare_failed",
                },
            ))?;
            return receive(store.task(account.clone(), task.clone()));
        }
    };
    let result = copy(
        &ready.imap,
        imap_password,
        &ready.snapshot.message_id,
        message.raw(),
        ready.folder.as_deref(),
    );
    receive(store.finish_copy(account.clone(), task.clone(), copy_outcome(result)))
        .map_err(|e| format!("副本结果尚未确认写盘，请勿重发，稍后刷新发送记录：{e}"))?;
    receive(store.task(account.clone(), task.clone()))
}

fn dispatch(
    store: StorageClient,
    review: SendReview,
    allow_empty_subject: bool,
    vault: &impl CredentialVault,
    transmit: impl FnOnce(
        &mail_core::ServerSettings,
        &str,
        outgoing::PreparedMessage,
    ) -> outgoing::Delivery,
    copy: impl FnOnce(&ServerSettings, &str, &str, &[u8], Option<&str>) -> sent::CopyResult,
) -> StorageResult<SendTask> {
    // 取密钥先于建立任务；密钥库不可用时草稿仍可编辑且无未提交任务。
    let credentials = vault.load(&review.account).map_err(|e| e.to_string())?;
    let ready = receive(store.begin_send(review, allow_empty_subject))?;
    let result = transmit(&ready.server, &credentials.smtp, ready.message);
    let task = receive(store.finish_send(ready.attempt, result.outcome))
        .map_err(|e| format!("发送结果尚未确认写盘，请勿重复发送：{e}"))?;
    if task.state != SendState::CopyPending {
        return Ok(task);
    }
    // SMTP 已接受；保存副本失败不推翻发送结论，也不自动重发。
    save_copy(
        &store,
        &task.account,
        &task.id,
        credentials.imap.as_str(),
        copy,
    )
}

fn retry_copy_once(
    store: StorageClient,
    vault: &impl CredentialVault,
    account: AccountId,
    task: SendTaskId,
    copy: impl FnOnce(&ServerSettings, &str, &str, &[u8], Option<&str>) -> sent::CopyResult,
) -> StorageResult<SendTask> {
    let credentials = vault.load(&account).map_err(|e| e.to_string())?;
    save_copy(&store, &account, &task, credentials.imap.as_str(), copy)
}

#[derive(Clone, Default)]
pub struct SendCoordinator {
    active: Arc<AtomicBool>,
}
struct Active(Arc<AtomicBool>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl SendCoordinator {
    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    /// 只能由操作者确认当前预览触发；没有定时器、恢复扫描或自动重试入口。
    pub fn confirm(
        &self,
        store: StorageClient,
        review: SendReview,
        allow_empty_subject: bool,
    ) -> StorageResult<Receiver<StorageResult<SendTask>>> {
        self.start(move || {
            dispatch(
                store,
                review,
                allow_empty_subject,
                &SystemVault,
                outgoing::deliver,
                sent::save_copy,
            )
        })
    }

    /// 只允许 CopyPending 任务；重试前按冻结 Message-ID 查重，绝不重发邮件。
    pub fn retry_copy(
        &self,
        store: StorageClient,
        account: AccountId,
        task: SendTaskId,
    ) -> StorageResult<Receiver<StorageResult<SendTask>>> {
        self.start(move || retry_copy_once(store, &SystemVault, account, task, sent::save_copy))
    }

    fn start(
        &self,
        operation: impl FnOnce() -> StorageResult<SendTask> + Send + 'static,
    ) -> StorageResult<Receiver<StorageResult<SendTask>>> {
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "另一封邮件仍在处理，请等待当前任务结果")?;
        let guard = Active(self.active.clone());
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("qingmail-send".into())
            .spawn(move || {
                let _guard = guard;
                let result = operation();
                let _ = tx.send(result);
            })
            .map_err(|_| "无法启动发送任务".to_owned())?;
        Ok(rx)
    }
}

#[cfg(test)]
#[path = "sending_tests.rs"]
mod tests;
