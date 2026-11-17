//! 进程内事件驱动的存储线程；SQLite 连接仅在此线程建立和使用。
#[path = "remote_storage.rs"]
mod remote;
#[path = "rpc_storage.rs"]
mod rpc;
use mail_content::draft::Draft;
use mail_core::{AccountId, DraftHeaders};
use mail_service::credentials::CredentialVault as _;
use mail_storage::{AppPaths, DraftDocument, Store, StoredDraft};
pub(crate) use rpc::COMMANDS as RPC_COMMANDS;
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};

pub type StorageResult<T> = Result<T, String>;
pub(crate) type RpcStorageResult<T> = StorageResult<Result<T, mail_protocol::RpcError>>;
pub type CatalogCache = (Vec<mail_core::RemoteFolder>, Option<i64>);
pub type RemovedAccount = (mail_storage::AccountImpact, Vec<AccountId>);
pub type AccountList = Vec<(AccountId, mail_core::AccountSettings)>;

#[derive(Clone)]
pub struct StorageClient {
    queue: SyncSender<Request>,
    remote: Option<mail_protocol::transport::Client>,
}

pub struct StorageWorker {
    client: StorageClient,
    thread: Option<thread::JoinHandle<()>>,
}

enum Request {
    RemoveLocal {
        account: AccountId,
        reply: SyncSender<StorageResult<(mail_storage::AccountImpact, Vec<AccountId>)>>,
    },
    CredentialReferenced {
        key: AccountId,
        reply: SyncSender<StorageResult<bool>>,
    },
    AccountVersion {
        account: AccountId,
        reply: SyncSender<StorageResult<mail_storage::AccountVersion>>,
    },
    ClaimMutation {
        request: Box<mail_protocol::Request>,
        reply: SyncSender<RpcStorageResult<mail_storage::MutationClaim>>,
    },
    FinishMutation {
        request: Box<mail_protocol::Request>,
        result: Box<Result<mail_protocol::Reply, mail_protocol::RpcError>>,
        reply: SyncSender<RpcStorageResult<()>>,
    },
    ValidateAccountCommit {
        baseline: Option<Box<mail_storage::AccountVersion>>,
        reply: SyncSender<RpcStorageResult<()>>,
    },
    CommitAccountCredentials {
        request: Box<mail_protocol::Request>,
        account: AccountId,
        settings: Box<mail_core::AccountSettings>,
        key: AccountId,
        baseline: Option<Box<mail_storage::AccountVersion>>,
        reply: SyncSender<RpcStorageResult<()>>,
    },
    Rpc {
        request: Box<mail_protocol::Request>,
        reply: SyncSender<StorageResult<mail_protocol::Response>>,
    },
    Open {
        account: Option<AccountId>,
        initial: Draft,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    Save {
        previous: Box<StoredDraft>,
        body: Draft,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    SaveDocument {
        previous: Box<StoredDraft>,
        document: DraftDocument,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    ImportAttachment {
        account: AccountId,
        path: PathBuf,
        reply: SyncSender<StorageResult<mail_storage::AttachmentInfo>>,
    },
    ImportBytes {
        account: AccountId,
        name: String,
        media_type: String,
        bytes: Vec<u8>,
        reply: SyncSender<StorageResult<mail_storage::AttachmentInfo>>,
    },
    AttachmentInfos {
        account: AccountId,
        ids: Vec<mail_core::AttachmentId>,
        reply: SyncSender<StorageResult<Vec<mail_storage::AttachmentInfo>>>,
    },
    New {
        account: Option<AccountId>,
        body: Draft,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    NewDocument {
        account: AccountId,
        document: Box<mail_storage::DraftDocument>,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    SelectedDraftBody {
        account: Option<AccountId>,
        reply: SyncSender<StorageResult<Option<Vec<u8>>>>,
    },
    DiscardSelectedDraft {
        account: Option<AccountId>,
        reply: SyncSender<StorageResult<()>>,
    },
    SetFlags {
        account: AccountId,
        folder: String,
        uidvalidity: u32,
        uids: Vec<u32>,
        seen: Option<bool>,
        flagged: Option<bool>,
        reply: SyncSender<StorageResult<()>>,
    },
    RemoveCached {
        account: AccountId,
        folder: String,
        uidvalidity: u32,
        uids: Vec<u32>,
        reply: SyncSender<StorageResult<()>>,
    },
    Folders {
        account: AccountId,
        reply: SyncSender<StorageResult<Vec<mail_core::Folder>>>,
    },
    Search {
        account: AccountId,
        query: mail_storage::SearchQuery,
        reply: SyncSender<StorageResult<Vec<mail_storage::SearchHit>>>,
    },
    MarkAttachments {
        location: mail_core::MailUid,
        count: u32,
        reply: SyncSender<StorageResult<()>>,
    },
    List {
        account: Option<AccountId>,
        reply: SyncSender<StorageResult<Vec<StoredDraft>>>,
    },
    Select {
        account: Option<AccountId>,
        id: mail_core::DraftId,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    AddAccount {
        settings: Box<mail_core::AccountSettings>,
        credentials: mail_service::credentials::Credentials,
        reply: SyncSender<StorageResult<AccountId>>,
    },
    Accounts {
        reply: SyncSender<StorageResult<AccountList>>,
    },
    UpdateAccount {
        account: AccountId,
        settings: Box<mail_core::AccountSettings>,
        credentials: mail_service::credentials::Credentials,
        reply: SyncSender<StorageResult<()>>,
    },
    AccountImpact {
        account: AccountId,
        reply: SyncSender<StorageResult<mail_storage::AccountImpact>>,
    },
    RemoveAccount {
        account: AccountId,
        reply: SyncSender<StorageResult<mail_storage::AccountImpact>>,
    },
    Catalog {
        account: AccountId,
        reply: SyncSender<StorageResult<CatalogCache>>,
    },
    FolderCache {
        account: AccountId,
        name: String,
        before: Option<u32>,
        reply: SyncSender<StorageResult<Option<mail_core::FolderCache>>>,
    },
    PublishCatalog {
        account: AccountId,
        folders: Vec<mail_core::RemoteFolder>,
        now: i64,
        reply: SyncSender<StorageResult<()>>,
    },
    PublishFolder {
        account: AccountId,
        batch: mail_core::FolderBatch,
        now: i64,
        reply: SyncSender<StorageResult<()>>,
    },
    MessageBody {
        location: mail_core::MailUid,
        reply: SyncSender<StorageResult<Option<Vec<u8>>>>,
    },
    SaveMessageBody {
        location: mail_core::MailUid,
        raw: Vec<u8>,
        reply: SyncSender<StorageResult<()>>,
    },
    ReviewSend {
        draft: Box<StoredDraft>,
        reply: SyncSender<StorageResult<crate::sending::SendReview>>,
    },
    ReviewVersion {
        account: AccountId,
        draft: mail_core::DraftId,
        revision: u32,
        reply: SyncSender<StorageResult<crate::sending::SendReview>>,
    },
    BeginSend {
        review: crate::sending::SendReview,
        allow_empty_subject: bool,
        reply: SyncSender<StorageResult<crate::sending::ReadySend>>,
    },
    FinishSend {
        attempt: mail_storage::SmtpAttempt,
        outcome: mail_core::SmtpOutcome,
        reply: SyncSender<StorageResult<mail_storage::SendTask>>,
    },
    CopyReady {
        account: AccountId,
        task: mail_core::SendTaskId,
        reply: SyncSender<StorageResult<mail_storage::CopyReady>>,
    },
    FinishCopy {
        account: AccountId,
        task: mail_core::SendTaskId,
        outcome: mail_storage::CopyOutcome,
        reply: SyncSender<StorageResult<mail_storage::SendTask>>,
    },
    Task {
        account: AccountId,
        id: mail_core::SendTaskId,
        reply: SyncSender<StorageResult<mail_storage::SendTask>>,
    },
    DraftTasks {
        account: AccountId,
        id: mail_core::DraftId,
        reply: SyncSender<StorageResult<Vec<mail_storage::SendTask>>>,
    },
    Tasks {
        account: AccountId,
        reply: SyncSender<StorageResult<Vec<mail_storage::SendTask>>>,
    },
    StopUncertain {
        account: AccountId,
        task: mail_core::SendTaskId,
        reply: SyncSender<StorageResult<mail_storage::SendTask>>,
    },
    TaskSnapshot {
        account: AccountId,
        task: mail_core::SendTaskId,
        reply: SyncSender<StorageResult<mail_storage::OutgoingSnapshot>>,
    },
    /// 受控重发：确认旧任务、创建一个新任务并返回冻结快照；尚未取得 Sending 许可。
    Resend {
        account: AccountId,
        previous: mail_core::SendTaskId,
        reply: SyncSender<
            StorageResult<(
                mail_core::SendTaskId,
                mail_storage::OutgoingSnapshot,
                mail_core::ServerSettings,
            )>,
        >,
    },
    BeginTask {
        account: AccountId,
        task: mail_core::SendTaskId,
        reply: SyncSender<StorageResult<mail_storage::SmtpAttempt>>,
    },
    CancelPending {
        account: AccountId,
        task: mail_core::SendTaskId,
        reply: SyncSender<StorageResult<()>>,
    },
    Stop,
}

impl StorageWorker {
    /// None 使用正式 XDG data；显式路径供隔离验证，不从 UI 请求接收路径。
    pub fn start(path: Option<PathBuf>) -> std::io::Result<Self> {
        let (queue, receiver) = mpsc::sync_channel(16);
        let thread = thread::Builder::new()
            .name("qingmail-storage".into())
            .spawn(move || run(path, receiver))?;
        Ok(Self {
            client: StorageClient {
                queue,
                remote: None,
            },
            thread: Some(thread),
        })
    }
    pub fn client(&self) -> StorageClient {
        self.client.clone()
    }
    pub fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.client.queue.send(Request::Stop);
            let _ = thread.join();
        }
    }
}
impl Drop for StorageWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

impl StorageClient {
    pub(crate) fn remove_local(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<RemovedAccount>>> {
        self.request(|reply| Request::RemoveLocal { account, reply })
    }
    pub fn ipc_client(&self) -> Option<mail_protocol::transport::Client> {
        self.remote.clone()
    }

    pub(crate) fn import_bytes(
        &self,
        account: AccountId,
        name: String,
        media_type: String,
        bytes: Vec<u8>,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::AttachmentInfo>>> {
        self.request(|reply| Request::ImportBytes {
            account,
            name,
            media_type,
            bytes,
            reply,
        })
    }
    pub(crate) fn remote_client(&self) -> Option<mail_protocol::transport::Client> {
        self.remote.clone()
    }
    pub(crate) fn review_version(
        &self,
        account: AccountId,
        draft: mail_core::DraftId,
        revision: u32,
    ) -> StorageResult<Receiver<StorageResult<crate::sending::SendReview>>> {
        self.request(|reply| Request::ReviewVersion {
            account,
            draft,
            revision,
            reply,
        })
    }
    pub(crate) fn credential_referenced(
        &self,
        key: AccountId,
    ) -> StorageResult<Receiver<StorageResult<bool>>> {
        self.request(|reply| Request::CredentialReferenced { key, reply })
    }
    pub fn account_version(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::AccountVersion>>> {
        self.request(|reply| Request::AccountVersion { account, reply })
    }
    pub(crate) fn claim_mutation(
        &self,
        request: mail_protocol::Request,
    ) -> StorageResult<Receiver<RpcStorageResult<mail_storage::MutationClaim>>> {
        self.request(|reply| Request::ClaimMutation {
            request: Box::new(request),
            reply,
        })
    }
    pub(crate) fn finish_mutation(
        &self,
        request: mail_protocol::Request,
        result: Result<mail_protocol::Reply, mail_protocol::RpcError>,
    ) -> StorageResult<Receiver<RpcStorageResult<()>>> {
        self.request(|reply| Request::FinishMutation {
            request: Box::new(request),
            result: Box::new(result),
            reply,
        })
    }
    pub(crate) fn validate_account_commit(
        &self,
        baseline: Option<mail_storage::AccountVersion>,
    ) -> StorageResult<Receiver<RpcStorageResult<()>>> {
        self.request(|reply| Request::ValidateAccountCommit {
            baseline: baseline.map(Box::new),
            reply,
        })
    }
    pub(crate) fn commit_account_credentials(
        &self,
        request: mail_protocol::Request,
        account: AccountId,
        settings: mail_core::AccountSettings,
        key: AccountId,
        baseline: Option<mail_storage::AccountVersion>,
    ) -> StorageResult<Receiver<RpcStorageResult<()>>> {
        self.request(|reply| Request::CommitAccountCredentials {
            request: Box::new(request),
            account,
            settings: Box::new(settings),
            key,
            baseline: baseline.map(Box::new),
            reply,
        })
    }
    pub fn rpc(
        &self,
        request: mail_protocol::Request,
    ) -> StorageResult<Receiver<StorageResult<mail_protocol::Response>>> {
        self.request(|reply| Request::Rpc {
            request: Box::new(request),
            reply,
        })
    }
    fn request<T>(
        &self,
        build: impl FnOnce(SyncSender<StorageResult<T>>) -> Request,
    ) -> StorageResult<Receiver<StorageResult<T>>> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.queue
            .try_send(build(reply))
            .map_err(|_| "存储队列繁忙或后台已停止，请重试".to_owned())?;
        Ok(receive)
    }
    pub fn add_account(
        &self,
        settings: mail_core::AccountSettings,
        credentials: mail_service::credentials::Credentials,
    ) -> StorageResult<Receiver<StorageResult<AccountId>>> {
        self.request(|reply| Request::AddAccount {
            settings: Box::new(settings),
            credentials,
            reply,
        })
    }
    pub fn accounts(&self) -> StorageResult<Receiver<StorageResult<AccountList>>> {
        self.request(|reply| Request::Accounts { reply })
    }
    /// 修改连接配置并更新同一账号的密钥库凭据；账号 ID 与本地数据不变。
    pub fn update_account(
        &self,
        account: AccountId,
        settings: mail_core::AccountSettings,
        credentials: mail_service::credentials::Credentials,
    ) -> StorageResult<Receiver<StorageResult<()>>> {
        self.request(|reply| Request::UpdateAccount {
            account,
            settings: Box::new(settings),
            credentials,
            reply,
        })
    }
    pub fn account_impact(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::AccountImpact>>> {
        self.request(|reply| Request::AccountImpact { account, reply })
    }
    /// 只移除本地账号与缓存；远端邮件不受影响。
    pub fn remove_account(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::AccountImpact>>> {
        self.request(|reply| Request::RemoveAccount { account, reply })
    }
    pub fn catalog(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<CatalogCache>>> {
        self.request(|reply| Request::Catalog { account, reply })
    }
    pub fn folder_cache(
        &self,
        account: AccountId,
        name: String,
        before: Option<u32>,
    ) -> StorageResult<Receiver<StorageResult<Option<mail_core::FolderCache>>>> {
        self.request(|reply| Request::FolderCache {
            account,
            name,
            before,
            reply,
        })
    }
    pub fn publish_catalog(
        &self,
        account: AccountId,
        folders: Vec<mail_core::RemoteFolder>,
        now: i64,
    ) -> StorageResult<Receiver<StorageResult<()>>> {
        self.request(|reply| Request::PublishCatalog {
            account,
            folders,
            now,
            reply,
        })
    }
    pub fn publish_folder(
        &self,
        account: AccountId,
        batch: mail_core::FolderBatch,
        now: i64,
    ) -> StorageResult<Receiver<StorageResult<()>>> {
        self.request(|reply| Request::PublishFolder {
            account,
            batch,
            now,
            reply,
        })
    }
    pub fn message_body(
        &self,
        location: mail_core::MailUid,
    ) -> StorageResult<Receiver<StorageResult<Option<Vec<u8>>>>> {
        self.request(|reply| Request::MessageBody { location, reply })
    }
    pub fn save_message_body(
        &self,
        location: mail_core::MailUid,
        raw: Vec<u8>,
    ) -> StorageResult<Receiver<StorageResult<()>>> {
        self.request(|reply| Request::SaveMessageBody {
            location,
            raw,
            reply,
        })
    }
    pub fn open_editor(
        &self,
        initial: Draft,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::Open {
            account: None,
            initial,
            reply,
        })
    }
    pub fn save(
        &self,
        previous: StoredDraft,
        body: Draft,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::Save {
            previous: Box::new(previous),
            body,
            reply,
        })
    }
    pub fn review_send(
        &self,
        draft: StoredDraft,
    ) -> StorageResult<Receiver<StorageResult<crate::sending::SendReview>>> {
        self.request(|reply| Request::ReviewSend {
            draft: Box::new(draft),
            reply,
        })
    }
    pub(crate) fn begin_send(
        &self,
        review: crate::sending::SendReview,
        allow_empty_subject: bool,
    ) -> StorageResult<Receiver<StorageResult<crate::sending::ReadySend>>> {
        self.request(|reply| Request::BeginSend {
            review,
            allow_empty_subject,
            reply,
        })
    }
    pub(crate) fn finish_send(
        &self,
        attempt: mail_storage::SmtpAttempt,
        outcome: mail_core::SmtpOutcome,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::SendTask>>> {
        self.request(|reply| Request::FinishSend {
            attempt,
            outcome,
            reply,
        })
    }
    pub(crate) fn copy_ready(
        &self,
        account: AccountId,
        task: mail_core::SendTaskId,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::CopyReady>>> {
        self.request(|reply| Request::CopyReady {
            account,
            task,
            reply,
        })
    }
    pub(crate) fn finish_copy(
        &self,
        account: AccountId,
        task: mail_core::SendTaskId,
        outcome: mail_storage::CopyOutcome,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::SendTask>>> {
        self.request(|reply| Request::FinishCopy {
            account,
            task,
            outcome,
            reply,
        })
    }
    pub(crate) fn task(
        &self,
        account: AccountId,
        id: mail_core::SendTaskId,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::SendTask>>> {
        self.request(|reply| Request::Task { account, id, reply })
    }
    pub fn draft_tasks(
        &self,
        account: AccountId,
        id: mail_core::DraftId,
    ) -> StorageResult<Receiver<StorageResult<Vec<mail_storage::SendTask>>>> {
        self.request(|reply| Request::DraftTasks { account, id, reply })
    }
    pub fn tasks(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<Vec<mail_storage::SendTask>>>> {
        self.request(|reply| Request::Tasks { account, reply })
    }
    pub fn task_snapshot(
        &self,
        account: AccountId,
        task: mail_core::SendTaskId,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::OutgoingSnapshot>>> {
        self.request(|reply| Request::TaskSnapshot {
            account,
            task,
            reply,
        })
    }
    pub fn stop_uncertain(
        &self,
        account: AccountId,
        task: mail_core::SendTaskId,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::SendTask>>> {
        self.request(|reply| Request::StopUncertain {
            account,
            task,
            reply,
        })
    }
    pub(crate) fn resend(
        &self,
        account: AccountId,
        previous: mail_core::SendTaskId,
    ) -> StorageResult<
        Receiver<
            StorageResult<(
                mail_core::SendTaskId,
                mail_storage::OutgoingSnapshot,
                mail_core::ServerSettings,
            )>,
        >,
    > {
        self.request(|reply| Request::Resend {
            account,
            previous,
            reply,
        })
    }
    pub(crate) fn begin_task(
        &self,
        account: AccountId,
        task: mail_core::SendTaskId,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::SmtpAttempt>>> {
        self.request(|reply| Request::BeginTask {
            account,
            task,
            reply,
        })
    }
    pub(crate) fn cancel_pending(
        &self,
        account: AccountId,
        task: mail_core::SendTaskId,
    ) -> StorageResult<Receiver<StorageResult<()>>> {
        self.request(|reply| Request::CancelPending {
            account,
            task,
            reply,
        })
    }
    pub fn open_account_editor(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::Open {
            account: Some(account),
            initial: Draft::plain("").expect("empty"),
            reply,
        })
    }
    pub fn account_drafts(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<Vec<StoredDraft>>>> {
        self.request(|reply| Request::List {
            account: Some(account),
            reply,
        })
    }
    pub fn new_account_draft(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::New {
            account: Some(account),
            body: Draft::plain("").expect("empty"),
            reply,
        })
    }
    pub fn set_flags(
        &self,
        account: AccountId,
        folder: String,
        uidvalidity: u32,
        uids: Vec<u32>,
        seen: Option<bool>,
        flagged: Option<bool>,
    ) -> StorageResult<Receiver<StorageResult<()>>> {
        self.request(|reply| Request::SetFlags {
            account,
            folder,
            uidvalidity,
            uids,
            seen,
            flagged,
            reply,
        })
    }
    pub fn remove_cached(
        &self,
        account: AccountId,
        folder: String,
        uidvalidity: u32,
        uids: Vec<u32>,
    ) -> StorageResult<Receiver<StorageResult<()>>> {
        self.request(|reply| Request::RemoveCached {
            account,
            folder,
            uidvalidity,
            uids,
            reply,
        })
    }
    pub fn folders(
        &self,
        account: AccountId,
    ) -> StorageResult<Receiver<StorageResult<Vec<mail_core::Folder>>>> {
        self.request(|reply| Request::Folders { account, reply })
    }
    pub fn search(
        &self,
        account: AccountId,
        query: mail_storage::SearchQuery,
    ) -> StorageResult<Receiver<StorageResult<Vec<mail_storage::SearchHit>>>> {
        self.request(|reply| Request::Search {
            account,
            query,
            reply,
        })
    }
    pub fn mark_attachments(
        &self,
        location: mail_core::MailUid,
        count: u32,
    ) -> StorageResult<Receiver<StorageResult<()>>> {
        self.request(|reply| Request::MarkAttachments {
            location,
            count,
            reply,
        })
    }
    /// 坏草稿恢复：读取选中草稿的原始正文字节，不要求正文可解析。
    pub fn selected_draft_body(
        &self,
        account: Option<AccountId>,
    ) -> StorageResult<Receiver<StorageResult<Option<Vec<u8>>>>> {
        self.request(|reply| Request::SelectedDraftBody { account, reply })
    }
    /// 坏草稿明确放弃：只删草稿行，保留对象文件。
    pub fn discard_selected_draft(
        &self,
        account: Option<AccountId>,
    ) -> StorageResult<Receiver<StorageResult<()>>> {
        self.request(|reply| Request::DiscardSelectedDraft { account, reply })
    }
    /// 回复/转发入口：以预填的收件人、主题与清理后的正文创建新草稿并选中。
    pub fn new_account_draft_document(
        &self,
        account: AccountId,
        document: mail_storage::DraftDocument,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::NewDocument {
            account,
            document: Box::new(document),
            reply,
        })
    }
    pub fn select_account_draft(
        &self,
        account: AccountId,
        id: mail_core::DraftId,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::Select {
            account: Some(account),
            id,
            reply,
        })
    }
    pub fn save_document(
        &self,
        previous: StoredDraft,
        document: DraftDocument,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::SaveDocument {
            previous: Box::new(previous),
            document,
            reply,
        })
    }
    /// 仅传入操作者在文件选择器明确选择的路径；后台只读打开，不执行文件。
    pub fn import_attachment(
        &self,
        account: AccountId,
        path: PathBuf,
    ) -> StorageResult<Receiver<StorageResult<mail_storage::AttachmentInfo>>> {
        self.request(|reply| Request::ImportAttachment {
            account,
            path,
            reply,
        })
    }
    pub fn attachment_infos(
        &self,
        account: AccountId,
        ids: Vec<mail_core::AttachmentId>,
    ) -> StorageResult<Receiver<StorageResult<Vec<mail_storage::AttachmentInfo>>>> {
        self.request(|reply| Request::AttachmentInfos {
            account,
            ids,
            reply,
        })
    }
    pub fn new_draft(&self, body: Draft) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::New {
            account: None,
            body,
            reply,
        })
    }
    pub fn list(&self) -> StorageResult<Receiver<StorageResult<Vec<StoredDraft>>>> {
        self.request(|reply| Request::List {
            account: None,
            reply,
        })
    }
    pub fn select(
        &self,
        id: mail_core::DraftId,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::Select {
            account: None,
            id,
            reply,
        })
    }
}

/// 在已有后台 future 中等待，不在 GPUI listener 内调用。
pub fn receive<T>(reply: StorageResult<Receiver<StorageResult<T>>>) -> StorageResult<T> {
    reply?
        .recv()
        .map_err(|_| "存储线程已停止，尚未收到保存确认".to_owned())?
}

fn run(path: Option<PathBuf>, receiver: Receiver<Request>) {
    let mut store: Option<Store> = None;
    for request in receiver {
        if matches!(request, Request::Stop) {
            break;
        }
        // 打开失败可以通过后续明确重试恢复；不自动丢弃坏库或覆盖原文件。
        let opened = (|| {
            if store.is_none() {
                let path = path
                    .clone()
                    .map(Ok)
                    .unwrap_or_else(|| AppPaths::discover().map(|p| p.data))
                    .map_err(|e| e.to_string())?;
                store = Some(Store::open(&path).map_err(|e| e.to_string())?);
            }
            Ok::<_, String>(())
        })();
        let opened = opened.map(|()| store.as_mut().expect("opened"));
        match request {
            Request::CredentialReferenced { key, reply } => {
                let _ =
                    reply
                        .send(opened.and_then(|s| {
                            s.credential_referenced(&key).map_err(|e| e.to_string())
                        }));
            }
            Request::AccountVersion { account, reply } => {
                let _ = reply.send(
                    opened.and_then(|s| s.account_version(&account).map_err(|e| e.to_string())),
                );
            }
            Request::ClaimMutation { request, reply } => {
                let _ = reply
                    .send(opened.map(|s| s.claim_mutation(&request).map_err(rpc::storage_error)));
            }
            Request::FinishMutation {
                request,
                result,
                reply,
            } => {
                let _ = reply.send(opened.map(|s| {
                    s.finish_mutation(&request, &result)
                        .map_err(rpc::storage_error)
                }));
            }
            Request::ValidateAccountCommit { baseline, reply } => {
                let _ = reply.send(opened.map(|s| {
                    s.validate_account_commit(baseline.as_deref())
                        .map_err(rpc::storage_error)
                }));
            }
            Request::CommitAccountCredentials {
                request,
                account,
                settings,
                key,
                baseline,
                reply,
            } => {
                let _ = reply.send(opened.map(|s| {
                    s.commit_account_credentials(
                        Some(&request),
                        &account,
                        &settings,
                        &key,
                        baseline.as_deref(),
                    )
                    .map_err(rpc::storage_error)
                }));
            }
            Request::Rpc { request, reply } => {
                let _ = reply.send(opened.map(|store| rpc::dispatch(store, *request)));
            }
            Request::Open {
                account,
                initial,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    if let Some(account) = account {
                        s.account(&account)
                            .map_err(|e| e.to_string())?
                            .ok_or("发件账号不存在或不是远端账号")?;
                        open_for(s, &account, initial)
                    } else {
                        open(s, initial)
                    }
                }));
            }
            Request::Save {
                previous,
                body,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    let mut document = previous.document;
                    document.body = body;
                    s.save_draft(
                        &previous.account,
                        &previous.id,
                        previous.revision,
                        &document,
                    )
                    .map_err(|e| e.to_string())
                }));
            }
            Request::SaveDocument {
                previous,
                document,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.save_draft(
                        &previous.account,
                        &previous.id,
                        previous.revision,
                        &document,
                    )
                    .map_err(|e| e.to_string())
                }));
            }
            Request::ImportBytes {
                account,
                name,
                media_type,
                bytes,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|store| {
                    store
                        .import_attachment(
                            &account,
                            &name,
                            &media_type,
                            &mut std::io::Cursor::new(bytes),
                        )
                        .map_err(|e| e.to_string())
                }));
            }
            Request::ImportAttachment {
                account,
                path,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| import_attachment(s, &account, &path)));
            }
            Request::AttachmentInfos {
                account,
                ids,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    if ids.len() > 32 {
                        return Err("附件数量超过上限".into());
                    }
                    ids.iter()
                        .map(|id| s.attachment(&account, id).map_err(|e| e.to_string()))
                        .collect()
                }));
            }
            Request::NewDocument {
                account,
                document,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.account(&account)
                        .map_err(|e| e.to_string())?
                        .ok_or("发件账号不存在或不是远端账号")?;
                    create_document(s, &account, *document)
                }));
            }
            Request::SetFlags {
                account,
                folder,
                uidvalidity,
                uids,
                seen,
                flagged,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.set_flags(&account, &folder, uidvalidity, &uids, seen, flagged)
                        .map_err(|e| e.to_string())
                }));
            }
            Request::Search {
                account,
                query,
                reply,
            } => {
                let _ = reply.send(
                    opened.and_then(|s| s.search(&account, &query).map_err(|e| e.to_string())),
                );
            }
            Request::MarkAttachments {
                location,
                count,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.mark_attachments(&location, count)
                        .map_err(|e| e.to_string())
                }));
            }
            Request::Folders { account, reply } => {
                let _ =
                    reply.send(opened.and_then(|s| s.folders(&account).map_err(|e| e.to_string())));
            }
            Request::RemoveCached {
                account,
                folder,
                uidvalidity,
                uids,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.remove_cached(&account, &folder, uidvalidity, &uids)
                        .map_err(|e| e.to_string())
                }));
            }
            Request::SelectedDraftBody { account, reply } => {
                let _ = reply.send(opened.and_then(|s| {
                    let account = editor_account(s, account)?;
                    s.selected_draft_body(&account).map_err(|e| e.to_string())
                }));
            }
            Request::DiscardSelectedDraft { account, reply } => {
                let _ = reply.send(opened.and_then(|s| {
                    let account = editor_account(s, account)?;
                    s.discard_selected_draft(&account)
                        .map_err(|e| e.to_string())
                }));
            }
            Request::New {
                account,
                body,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    if let Some(account) = account {
                        s.account(&account)
                            .map_err(|e| e.to_string())?
                            .ok_or("发件账号不存在或不是远端账号")?;
                        create(s, &account, body)
                    } else {
                        new(s, body)
                    }
                }));
            }
            Request::List { account, reply } => {
                let _ = reply.send(opened.and_then(|s| {
                    let account = editor_account(s, account)?;
                    s.draft_ids(&account)
                        .map_err(|e| e.to_string())?
                        .iter()
                        .map(|id| s.draft(&account, id).map_err(|e| e.to_string()))
                        .collect()
                }));
            }
            Request::Select { account, id, reply } => {
                let _ = reply.send(opened.and_then(|s| {
                    let account = editor_account(s, account)?;
                    let draft = s.draft(&account, &id).map_err(|e| e.to_string())?;
                    s.select_editor_draft(&account, &id)
                        .map_err(|e| e.to_string())?;
                    Ok(draft)
                }));
            }
            Request::Accounts { reply } => {
                let _ = reply.send(opened.and_then(|s| s.accounts().map_err(|e| e.to_string())));
            }
            Request::AccountImpact { account, reply } => {
                let _ = reply.send(
                    opened.and_then(|s| s.account_impact(&account).map_err(|e| e.to_string())),
                );
            }
            Request::UpdateAccount {
                account,
                settings,
                credentials,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    persist_update_account(
                        s,
                        &mail_service::credentials::SystemVault,
                        &account,
                        &settings,
                        &credentials,
                    )
                }));
            }
            Request::RemoveLocal { account, reply } => {
                let _ = reply.send(opened.and_then(|s| {
                    let keys = s.credential_keys(&account).map_err(|e| e.to_string())?;
                    let impact = s.remove_account(&account).map_err(|e| e.to_string())?;
                    Ok((impact, keys))
                }));
            }
            Request::RemoveAccount { account, reply } => {
                let _ = reply.send(opened.and_then(|s| {
                    let keys = s.credential_keys(&account).map_err(|e| e.to_string())?;
                    let impact = s.remove_account(&account).map_err(|e| e.to_string())?;
                    let mut failed = false;
                    for key in keys {
                        failed |= mail_service::credentials::SystemVault.delete(&key).is_err();
                    }
                    if failed {
                        return Err(
                            "账号已从本地移除，但部分密钥库凭据版本可能仍在，请检查密钥库".into(),
                        );
                    }
                    Ok(impact)
                }));
            }
            Request::AddAccount {
                settings,
                credentials,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    persist_account(
                        s,
                        &mail_service::credentials::SystemVault,
                        &settings,
                        &credentials,
                    )
                }));
            }
            Request::Catalog { account, reply } => {
                let _ = reply.send(opened.and_then(|s| {
                    Ok((
                        s.remote_folders(&account).map_err(|e| e.to_string())?,
                        s.catalog_synced_at(&account).map_err(|e| e.to_string())?,
                    ))
                }));
            }
            Request::FolderCache {
                account,
                name,
                before,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.folder_cache(&account, &name, before)
                        .map_err(|e| e.to_string())
                }));
            }
            Request::PublishCatalog {
                account,
                folders,
                now,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.replace_remote_folders(&account, &folders, now)
                        .map_err(|e| e.to_string())
                }));
            }
            Request::PublishFolder {
                account,
                batch,
                now,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.apply_folder_batch(&account, &batch, now)
                        .map_err(|e| e.to_string())
                }));
            }
            Request::MessageBody { location, reply } => {
                let _ = reply.send(
                    opened.and_then(|s| s.message_body(&location).map_err(|e| e.to_string())),
                );
            }
            Request::SaveMessageBody {
                location,
                raw,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.save_message_body(&location, &raw)
                        .map_err(|e| e.to_string())
                }));
            }
            Request::ReviewVersion {
                account,
                draft,
                revision,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|store| {
                    let draft = store.draft(&account, &draft).map_err(|e| e.to_string())?;
                    if draft.revision != revision {
                        return Err("草稿版本已改变，请重新检查发送".into());
                    }
                    crate::sending::review(store, &draft)
                }));
            }
            Request::ReviewSend { draft, reply } => {
                let _ = reply.send(opened.and_then(|s| crate::sending::review(s, &draft)));
            }
            Request::BeginSend {
                review,
                allow_empty_subject,
                reply,
            } => {
                let _ = reply.send(
                    opened.and_then(|s| crate::sending::begin(s, review, allow_empty_subject)),
                );
            }
            Request::FinishSend {
                attempt,
                outcome,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    let id = attempt.task().clone();
                    // account从许可内的持久任务定位，不能接受UI提供的另一个账号。
                    let account = attempt.account().clone();
                    s.finish_smtp(attempt, outcome).map_err(|e| e.to_string())?;
                    s.task(&account, &id).map_err(|e| e.to_string())
                }));
            }
            Request::CopyReady {
                account,
                task,
                reply,
            } => {
                let _ =
                    reply
                        .send(opened.and_then(|s| {
                            s.copy_ready(&account, &task).map_err(|e| e.to_string())
                        }));
            }
            Request::FinishCopy {
                account,
                task,
                outcome,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.finish_copy(&account, &task, outcome)
                        .map_err(|e| e.to_string())?;
                    s.task(&account, &task).map_err(|e| e.to_string())
                }));
            }
            Request::Task { account, id, reply } => {
                let _ = reply
                    .send(opened.and_then(|s| s.task(&account, &id).map_err(|e| e.to_string())));
            }
            Request::TaskSnapshot {
                account,
                task,
                reply,
            } => {
                let _ = reply.send(
                    opened
                        .and_then(|s| s.task_snapshot(&account, &task).map_err(|e| e.to_string())),
                );
            }
            Request::Tasks { account, reply } => {
                let _ = reply
                    .send(opened.and_then(|s| s.task_list(&account).map_err(|e| e.to_string())));
            }
            Request::StopUncertain {
                account,
                task,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    s.stop_uncertain(&account, &task)
                        .map_err(|e| e.to_string())?;
                    s.task(&account, &task).map_err(|e| e.to_string())
                }));
            }
            Request::Resend {
                account,
                previous,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    let next = s
                        .resend_task(
                            &account,
                            &previous,
                            mail_core::ResendAuthorization::UserConfirmed,
                        )
                        .map_err(|e| e.to_string())?;
                    let result = (|| {
                        let snapshot = s
                            .task_snapshot(&account, &next)
                            .map_err(|e| e.to_string())?;
                        let smtp = s
                            .account(&account)
                            .map_err(|e| e.to_string())?
                            .ok_or("发件账号不存在")?
                            .smtp;
                        Ok((next.clone(), snapshot, smtp))
                    })();
                    if result.is_err() {
                        // 快照不可用时不留一个永远 Pending 的新任务。
                        let _ = s.cancel_pending(&account, &next);
                    }
                    result
                }));
            }
            Request::BeginTask {
                account,
                task,
                reply,
            } => {
                let _ =
                    reply
                        .send(opened.and_then(|s| {
                            s.begin_smtp(&account, &task).map_err(|e| e.to_string())
                        }));
            }
            Request::CancelPending {
                account,
                task,
                reply,
            } => {
                let _ = reply
                    .send(opened.and_then(|s| {
                        s.cancel_pending(&account, &task).map_err(|e| e.to_string())
                    }));
            }
            Request::DraftTasks { account, id, reply } => {
                let _ = reply.send(
                    opened.and_then(|s| s.draft_tasks(&account, &id).map_err(|e| e.to_string())),
                );
            }
            Request::Stop => break,
        }
    }
}
fn selected_attachment_file(path: &std::path::Path) -> StorageResult<(String, std::fs::File)> {
    use rustix::fs::{Mode, OFlags, open};
    if !path.is_absolute() {
        return Err("请选择绝对路径的普通文件".into());
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("附件文件名不是有效UTF-8")?;
    // NONBLOCK避免FIFO阻塞；打开后核对同一个句柄，不使用检查后再打开的方式。
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| "无法打开附件，请选择可读的普通文件")?;
    let file = std::fs::File::from(fd);
    let metadata = file.metadata().map_err(|_| "无法读取附件信息")?;
    if !metadata.is_file() || metadata.len() > mail_storage::MAX_ATTACHMENT_BYTES {
        return Err("附件须为不超过25MiB的普通文件".into());
    }
    Ok((name.into(), file))
}
fn import_attachment(
    store: &mut Store,
    account: &AccountId,
    path: &std::path::Path,
) -> StorageResult<mail_storage::AttachmentInfo> {
    let (name, mut file) = selected_attachment_file(path)?;
    store
        .import_attachment(account, &name, "application/octet-stream", &mut file)
        .map_err(|e| e.to_string())
}

fn editor_account(store: &mut Store, account: Option<AccountId>) -> StorageResult<AccountId> {
    if let Some(account) = account {
        store
            .account(&account)
            .map_err(|e| e.to_string())?
            .ok_or("发件账号不存在或不是远端账号")?;
        Ok(account)
    } else {
        store.local_draft_account().map_err(|e| e.to_string())
    }
}
fn open(store: &mut Store, initial: Draft) -> StorageResult<StoredDraft> {
    let account = store.local_draft_account().map_err(|e| e.to_string())?;
    open_for(store, &account, initial)
}
fn open_for(store: &mut Store, account: &AccountId, initial: Draft) -> StorageResult<StoredDraft> {
    if let Some(id) = store
        .selected_local_draft(account)
        .map_err(|e| e.to_string())?
    {
        return store.draft(account, &id).map_err(|e| e.to_string());
    }
    // 上次可能在创建草稿后、选中前退出；优先恢复已存在草稿，不反复创建。
    if let Some(id) = store.draft_ids(account).map_err(|e| e.to_string())?.first() {
        let draft = store.draft(account, id).map_err(|e| e.to_string())?;
        store
            .select_editor_draft(account, id)
            .map_err(|e| e.to_string())?;
        return Ok(draft);
    }
    create(store, account, initial)
}
fn new(store: &mut Store, body: Draft) -> StorageResult<StoredDraft> {
    let account = store.local_draft_account().map_err(|e| e.to_string())?;
    create(store, &account, body)
}
fn create(store: &mut Store, account: &AccountId, body: Draft) -> StorageResult<StoredDraft> {
    create_document(
        store,
        account,
        DraftDocument {
            headers: DraftHeaders::default(),
            body,
            attachments: vec![],
        },
    )
}
fn create_document(
    store: &mut Store,
    account: &AccountId,
    document: DraftDocument,
) -> StorageResult<StoredDraft> {
    let draft = store
        .create_draft(account, &document)
        .map_err(|e| e.to_string())?;
    store
        .select_editor_draft(account, &draft.id)
        .map_err(|e| e.to_string())?;
    Ok(draft)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_preserves_revision_order_switches_and_reopens_selected_draft() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        let client = worker.client();
        let first = receive(client.open_editor(Draft::plain("first🙂").unwrap())).unwrap();
        let update = receive(client.save(first.clone(), Draft::plain("saved🙂").unwrap())).unwrap();
        assert!(receive(client.save(first.clone(), Draft::plain("stale").unwrap())).is_err());
        let second = receive(client.new_draft(Draft::plain("second").unwrap())).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(receive(client.list()).unwrap().len(), 2);
        assert_eq!(receive(client.select(first.id)).unwrap(), update);
        worker.stop();
        assert!(client.list().is_err());
        let mut worker = StorageWorker::start(Some(path)).unwrap();
        assert_eq!(
            receive(
                worker
                    .client()
                    .open_editor(Draft::plain("must not replace").unwrap())
            )
            .unwrap(),
            update
        );
        worker.stop();
    }

    #[test]
    fn catalog_queue_reads_published_cache_and_rejects_unknown_accounts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let account = {
            let mut store = Store::open(&path).unwrap();
            let server = mail_core::ServerSettings {
                host: "example.invalid".into(),
                port: 993,
                username: "synthetic".into(),
                security: mail_core::ConnectionSecurity::Tls,
            };
            store
                .create_account(&mail_core::AccountSettings {
                    display_name: "synthetic".into(),
                    address: "synthetic@example.invalid".into(),
                    imap: server.clone(),
                    smtp: server,
                })
                .unwrap()
        };
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        let client = worker.client();
        let folders = vec![mail_core::RemoteFolder {
            name: "INBOX".into(),
            display_name: "收件箱".into(),
            delimiter: None,
            selectable: true,
        }];
        receive(client.publish_catalog(account.clone(), folders.clone(), 1)).unwrap();
        assert_eq!(
            receive(client.catalog(account.clone())).unwrap(),
            (folders, Some(1))
        );
        let batch = mail_core::FolderBatch {
            remote_name: "INBOX".into(),
            uidvalidity: 42,
            since: 0,
            has_more: false,
            complete: true,
            kind: mail_core::BatchKind::Recent,
            flags: Default::default(),
            messages: vec![(
                7,
                mail_core::MessageEnvelope {
                    subject: "后台同步".into(),
                    sender: "test".into(),
                    received_at: 1,
                },
            )],
        };
        receive(client.publish_folder(account.clone(), batch, 2)).unwrap();
        let cache = receive(client.folder_cache(account.clone(), "INBOX".into(), None))
            .unwrap()
            .unwrap();
        assert_eq!(cache.messages[0].location.uid, 7);
        let location = cache.messages[0].location.clone();
        let raw = b"Subject: cached\r\n\r\nbody".to_vec();
        assert_eq!(
            receive(client.message_body(location.clone())).unwrap(),
            None
        );
        receive(client.save_message_body(location.clone(), raw.clone())).unwrap();
        assert_eq!(
            receive(client.message_body(location.clone())).unwrap(),
            Some(raw.clone())
        );
        assert!(receive(client.publish_catalog(AccountId("missing".into()), vec![], 3)).is_err());
        worker.stop();
        let mut worker = StorageWorker::start(Some(path)).unwrap();
        assert_eq!(
            receive(worker.client().folder_cache(account, "INBOX".into(), None)).unwrap(),
            Some(cache)
        );
        assert_eq!(
            receive(worker.client().message_body(location)).unwrap(),
            Some(raw)
        );
        worker.stop();
    }

    #[test]
    fn reply_document_creates_draft_and_raw_body_can_be_saved_or_discarded() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let account = {
            let mut store = Store::open(&path).unwrap();
            let server = mail_core::ServerSettings {
                host: "example.invalid".into(),
                port: 993,
                username: "synthetic".into(),
                security: mail_core::ConnectionSecurity::Tls,
            };
            store
                .create_account(&mail_core::AccountSettings {
                    display_name: "synthetic".into(),
                    address: "sender@example.invalid".into(),
                    imap: server.clone(),
                    smtp: server,
                })
                .unwrap()
        };
        let mut worker = StorageWorker::start(Some(path)).unwrap();
        let client = worker.client();
        let document = DraftDocument {
            headers: DraftHeaders {
                subject: "Re: original".into(),
                to: vec!["alice@example.invalid".into()],
                ..Default::default()
            },
            body: Draft::plain("引用的正文").unwrap(),
            attachments: vec![],
        };
        let draft =
            receive(client.new_account_draft_document(account.clone(), document.clone())).unwrap();
        assert_eq!(draft.document, document);
        let raw = receive(client.selected_draft_body(Some(account.clone())))
            .unwrap()
            .expect("raw body");
        assert_eq!(
            Draft::from_json(&raw).unwrap().text(),
            "引用的正文".to_string()
        );
        receive(client.discard_selected_draft(Some(account.clone()))).unwrap();
        assert!(
            receive(client.account_drafts(account.clone()))
                .unwrap()
                .is_empty()
        );
        worker.stop();
    }

    #[test]
    fn account_drafts_keep_sender_identity_and_do_not_adopt_local_or_other_drafts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let (account, other) = {
            let mut store = Store::open(&path).unwrap();
            let server = mail_core::ServerSettings {
                host: "example.invalid".into(),
                port: 993,
                username: "synthetic".into(),
                security: mail_core::ConnectionSecurity::Tls,
            };
            let settings = mail_core::AccountSettings {
                display_name: "synthetic".into(),
                address: "sender@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            };
            (
                store.create_account(&settings).unwrap(),
                store.create_account(&settings).unwrap(),
            )
        };
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        let c = worker.client();
        let local = receive(c.open_editor(Draft::plain("local preserved").unwrap())).unwrap();
        let remote = receive(c.open_account_editor(account.clone())).unwrap();
        assert_eq!(remote.account, account);
        assert!(remote.document.body.text().is_empty());
        assert_eq!(
            receive(c.account_drafts(account.clone())).unwrap(),
            vec![remote.clone()]
        );
        assert_eq!(receive(c.list()).unwrap(), vec![local.clone()]);
        assert!(receive(c.select_account_draft(account.clone(), local.id.clone())).is_err());
        assert!(receive(c.select_account_draft(other.clone(), remote.id.clone())).is_err());
        assert!(receive(c.open_account_editor(local.account.clone())).is_err());
        assert!(receive(c.open_account_editor(AccountId("missing".into()))).is_err());
        let mut document = remote.document.clone();
        document.headers.subject = "账号草稿".into();
        document.body = Draft::plain("remote preserved").unwrap();
        let saved = receive(c.save_document(remote, document)).unwrap();
        let second = receive(c.new_account_draft(account.clone())).unwrap();
        assert_ne!(second.id, saved.id);
        assert_eq!(second.account, account);
        receive(c.select_account_draft(account.clone(), saved.id.clone())).unwrap();
        worker.stop();
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        let c = worker.client();
        assert_eq!(
            receive(c.open_account_editor(account.clone())).unwrap(),
            saved
        );
        assert_eq!(
            receive(c.open_editor(Draft::plain("").unwrap())).unwrap(),
            local
        );
        // 仅记忆最近一个编辑上下文；切回账号时回退到已有草稿，不创建或跨账号搬迁。
        let reopened = receive(c.open_account_editor(account.clone())).unwrap();
        assert_eq!(reopened.account, account);
        assert!(reopened.id == saved.id || reopened.id == second.id);
        assert_eq!(
            receive(c.select_account_draft(account.clone(), saved.id.clone())).unwrap(),
            saved
        );
        assert!(receive(c.account_drafts(other)).unwrap().is_empty());
        worker.stop();
        let store = Store::open(&path).unwrap();
        assert!(store.task_ids(&account).unwrap().is_empty());
    }

    #[test]
    fn full_document_and_attachment_survive_source_removal_and_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let source = root.path().join("中文附件.txt");
        std::fs::write(&source, "附件原文🙂").unwrap();
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        let client = worker.client();
        let first = receive(client.open_editor(Draft::plain("正文").unwrap())).unwrap();
        let info =
            receive(client.import_attachment(first.account.clone(), source.clone())).unwrap();
        let mut document = first.document.clone();
        document.headers = DraftHeaders {
            subject: "中文主题🙂".into(),
            to: vec!["partial@".into()],
            cc: vec!["cc@example.invalid".into()],
            bcc: vec!["bcc@example.invalid".into()],
        };
        document.attachments = vec![info.id.clone()];
        let saved = receive(client.save_document(first.clone(), document.clone())).unwrap();
        assert!(receive(client.save_document(first, document.clone())).is_err());
        std::fs::remove_file(&source).unwrap();
        let second = receive(client.new_draft(Draft::plain("另一份").unwrap())).unwrap();
        assert_eq!(second.document.headers, DraftHeaders::default());
        assert!(second.document.attachments.is_empty());
        assert_eq!(receive(client.select(saved.id.clone())).unwrap(), saved);
        worker.stop();
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        assert_eq!(
            receive(worker.client().open_editor(Draft::plain("").unwrap())).unwrap(),
            saved
        );
        assert_eq!(
            receive(
                worker
                    .client()
                    .attachment_infos(saved.account.clone(), vec![info.id.clone()])
            )
            .unwrap(),
            vec![info]
        );
        worker.stop();
        let mut store = Store::open(&path).unwrap();
        assert!(store.task_ids(&saved.account).unwrap().is_empty());
        // 移除仅改变草稿关联；其他版本和原始文件不受影响。
        document.attachments.clear();
        let removed = store
            .save_draft(&saved.account, &saved.id, saved.revision, &document)
            .unwrap();
        assert!(removed.document.attachments.is_empty());
    }

    #[test]
    fn selected_attachment_refuses_links_special_files_oversize_and_missing() {
        use rustix::fs::{CWD, Mode, mkfifoat};
        let root = tempfile::tempdir().unwrap();
        let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let client = worker.client();
        let draft = receive(client.open_editor(Draft::plain("").unwrap())).unwrap();
        let source = root.path().join("file");
        std::fs::write(&source, b"safe").unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        let fifo = root.path().join("fifo");
        mkfifoat(CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        let large = root.path().join("large");
        std::fs::File::create(&large)
            .unwrap()
            .set_len(mail_storage::MAX_ATTACHMENT_BYTES + 1)
            .unwrap();
        for path in [
            link,
            fifo,
            large,
            root.path().to_path_buf(),
            root.path().join("missing"),
            PathBuf::from("relative"),
        ] {
            assert!(receive(client.import_attachment(draft.account.clone(), path)).is_err());
        }
        assert_eq!(
            receive(client.open_editor(Draft::plain("").unwrap())).unwrap(),
            draft
        );
        worker.stop();
    }

    #[test]
    fn dropped_reply_does_not_block_worker_or_lose_accepted_save() {
        let root = tempfile::tempdir().unwrap();
        let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let client = worker.client();
        let draft = receive(client.open_editor(Draft::plain("old").unwrap())).unwrap();
        drop(client.save(draft, Draft::plain("new").unwrap()));
        assert_eq!(
            receive(client.list()).unwrap()[0].document.body.text(),
            "new"
        );
        worker.stop();
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    #[test]
    fn failed_open_can_retry_without_discarding_database() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        let client = worker.client();
        assert!(receive(client.open_editor(Draft::plain("initial").unwrap())).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let draft = receive(client.open_editor(Draft::plain("initial").unwrap())).unwrap();
        assert_eq!(draft.document.body.text(), "initial");
        worker.stop();
    }
}

fn persist_update_account(
    store: &mut Store,
    vault: &impl mail_service::credentials::CredentialVault,
    account: &AccountId,
    settings: &mail_core::AccountSettings,
    credentials: &mail_service::credentials::Credentials,
) -> StorageResult<()> {
    mail_service::accounts::validate(settings).map_err(|e| e.to_string())?;
    let baseline = store.account_version(account).map_err(|e| e.to_string())?;
    let key = loop {
        let key = AccountId(mail_protocol::transport::request_id().map_err(|e| e.to_string())?);
        if &key != account
            && !store
                .credential_referenced(&key)
                .map_err(|e| e.to_string())?
        {
            break key;
        }
    };
    vault
        .save(&key, credentials)
        .map_err(|e| format!("新凭据未确认保存：{e}；旧配置与旧凭据未更改"))?;
    if let Err(e) = store.commit_account_credentials(None, account, settings, &key, Some(&baseline))
    {
        // 只有确认新键不是活动版本才清理；不删除旧键，也不猜测失败的提交结果。
        match store.account_version(account) {
            Ok(current) if current.credential_key != key => {
                let _ = vault.delete(&key);
                return Err(format!("账号更新失败，旧配置与凭据已保留：{e}"));
            }
            _ => return Err("账号更新结果不确定，凭据版本已保留，请刷新后检查".into()),
        }
    }
    Ok(())
}
fn persist_account(
    store: &mut Store,
    vault: &impl mail_service::credentials::CredentialVault,
    settings: &mail_core::AccountSettings,
    credentials: &mail_service::credentials::Credentials,
) -> StorageResult<AccountId> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| "无法生成账号 ID".to_owned())?;
    let id = AccountId(format!("{:032x}", u128::from_be_bytes(random)));
    persist_account_with_id(store, vault, settings, credentials, id)
}
fn persist_account_with_id(
    store: &mut Store,
    vault: &impl mail_service::credentials::CredentialVault,
    settings: &mail_core::AccountSettings,
    credentials: &mail_service::credentials::Credentials,
    id: AccountId,
) -> StorageResult<AccountId> {
    mail_service::accounts::validate(settings).map_err(|e| e.to_string())?;
    if !store.accounts().map_err(|e| e.to_string())?.is_empty() {
        return Err("P2 当前仅支持一个远程账号".into());
    }
    vault.save(&id, credentials).map_err(|e| match e {
        mail_service::credentials::VaultError::OperationFailed
        | mail_service::credentials::VaultError::Timeout => {
            format!("{e}；账号未写入，密钥库可能留有凭据项")
        }
        _ => e.to_string(),
    })?;
    if let Err(e) = store.insert_account(&id, settings) {
        // 提交结果可能不确定；只有确认账号不存在才删密钥。
        match store.account(&id) {
            Ok(None) => {
                if vault.delete(&id).is_err() {
                    return Err("账号保存失败，密钥库可能保留孤立凭据，请检查后重试".into());
                }
            }
            _ => return Err("账号保存结果需重新检查，凭据已保留，请刷新账号列表".into()),
        }
        return Err(e.to_string());
    }
    Ok(id)
}

#[cfg(test)]
mod account_tests {
    use super::*;
    use mail_core::{AccountSettings, ConnectionSecurity, ServerSettings};
    use mail_service::credentials::{CredentialVault, Credentials, VaultError};
    use std::cell::Cell;
    struct Vault {
        error: Option<VaultError>,
        saves: Cell<usize>,
        deletes: Cell<usize>,
    }
    impl CredentialVault for Vault {
        fn status(&self) -> Result<(), VaultError> {
            self.error.map_or(Ok(()), Err)
        }
        fn save(&self, _: &AccountId, _: &Credentials) -> Result<(), VaultError> {
            self.saves.set(self.saves.get() + 1);
            self.status()
        }
        fn load(&self, _: &AccountId) -> Result<Credentials, VaultError> {
            Err(VaultError::Missing)
        }
        fn delete(&self, _: &AccountId) -> Result<(), VaultError> {
            self.deletes.set(self.deletes.get() + 1);
            Ok(())
        }
    }
    fn settings() -> AccountSettings {
        let server = ServerSettings {
            host: "example.invalid".into(),
            port: 993,
            username: "synthetic".into(),
            security: ConnectionSecurity::Tls,
        };
        AccountSettings {
            display_name: "测试".into(),
            address: "test@example.invalid".into(),
            imap: server.clone(),
            smtp: server,
        }
    }
    #[test]
    fn legacy_update_switches_versions_and_database_failure_never_overwrites_old_key() {
        use std::cell::RefCell;
        struct TrackingVault {
            saves: RefCell<Vec<AccountId>>,
            deletes: RefCell<Vec<AccountId>>,
        }
        impl CredentialVault for TrackingVault {
            fn status(&self) -> Result<(), VaultError> {
                Ok(())
            }
            fn save(&self, key: &AccountId, _: &Credentials) -> Result<(), VaultError> {
                self.saves.borrow_mut().push(key.clone());
                Ok(())
            }
            fn load(&self, _: &AccountId) -> Result<Credentials, VaultError> {
                Err(VaultError::Missing)
            }
            fn delete(&self, key: &AccountId) -> Result<(), VaultError> {
                self.deletes.borrow_mut().push(key.clone());
                Ok(())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut store = Store::open(&data).unwrap();
        let account = store.create_account(&settings()).unwrap();
        let baseline = store.account_version(&account).unwrap();
        let vault = TrackingVault {
            saves: RefCell::new(vec![]),
            deletes: RefCell::new(vec![]),
        };
        let credentials = Credentials::new("imap".into(), "smtp".into()).unwrap();
        let conn = rusqlite::Connection::open(data.join("state.sqlite3")).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_switch BEFORE INSERT ON account_credentials BEGIN SELECT RAISE(ABORT,'disk failure'); END;").unwrap();
        let mut changed = settings();
        changed.imap.host = "changed.example.test".into();
        assert!(
            persist_update_account(&mut store, &vault, &account, &changed, &credentials).is_err()
        );
        assert_eq!(store.account_version(&account).unwrap(), baseline);
        assert_ne!(vault.saves.borrow()[0], account);
        assert_eq!(*vault.deletes.borrow(), *vault.saves.borrow());
        conn.execute_batch("DROP TRIGGER fail_switch;").unwrap();
        persist_update_account(&mut store, &vault, &account, &changed, &credentials).unwrap();
        let current = store.account_version(&account).unwrap();
        assert_eq!(current.settings, changed);
        assert_eq!(current.credential_key, vault.saves.borrow()[1]);
        assert!(store.credential_keys(&account).unwrap().contains(&account));
        assert!(!vault.deletes.borrow().contains(&account));
    }
    #[test]
    fn unavailable_locked_and_unknown_vault_results_never_insert_account() {
        for error in [
            VaultError::Unavailable,
            VaultError::Locked,
            VaultError::Missing,
            VaultError::OperationFailed,
            VaultError::Timeout,
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut store = Store::open(&root.path().join("data")).unwrap();
            let vault = Vault {
                error: Some(error),
                saves: Cell::new(0),
                deletes: Cell::new(0),
            };
            assert!(
                persist_account(
                    &mut store,
                    &vault,
                    &settings(),
                    &Credentials::new("synthetic-imap".into(), "synthetic-smtp".into()).unwrap()
                )
                .is_err()
            );
            assert!(store.accounts().unwrap().is_empty());
            assert_eq!(vault.saves.get(), 1);
            assert_eq!(vault.deletes.get(), 0);
        }
    }
    #[test]
    fn confirmed_vault_save_precedes_account_and_second_account_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let vault = Vault {
            error: None,
            saves: Cell::new(0),
            deletes: Cell::new(0),
        };
        let c = Credentials::new("synthetic-imap".into(), "synthetic-smtp".into()).unwrap();
        let id = persist_account(&mut store, &vault, &settings(), &c).unwrap();
        assert_eq!(store.account(&id).unwrap(), Some(settings()));
        assert!(persist_account(&mut store, &vault, &settings(), &c).is_err());
        assert_eq!(vault.saves.get(), 1);
        drop(store);
        let store = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(store.accounts().unwrap().len(), 1);
        // 真实 SQLite 字节不能含任何密码。
        let bytes = std::fs::read(root.path().join("data/state.sqlite3")).unwrap();
        assert!(!bytes.windows(c.imap.len()).any(|w| w == c.imap.as_bytes()));
        assert!(!bytes.windows(c.smtp.len()).any(|w| w == c.smtp.as_bytes()));
    }
    #[test]
    fn failed_database_insert_cleans_only_confirmed_absent_remote_account() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let id = store.local_draft_account().unwrap();
        let vault = Vault {
            error: None,
            saves: Cell::new(0),
            deletes: Cell::new(0),
        };
        assert!(
            persist_account_with_id(
                &mut store,
                &vault,
                &settings(),
                &Credentials::new("imap".into(), "smtp".into()).unwrap(),
                id
            )
            .is_err()
        );
        assert!(store.accounts().unwrap().is_empty());
        assert_eq!(vault.saves.get(), 1);
        assert_eq!(vault.deletes.get(), 1);
    }
}
