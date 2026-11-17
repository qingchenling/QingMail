//! QingMail IPC v1：有界 NDJSON 控制帧；资源数据通过单独连接的二进制帧传输。
mod domain;
pub mod transport;
mod wire;
pub use domain::*;
use mail_content::draft::Draft;
use mail_core::*;
use serde::{Deserialize, Serialize};
pub use wire::*;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_CONTROL_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_CHUNK_BYTES: u32 = 64 * 1024;
pub const MAX_PAGE: u32 = 200;
pub const MAX_BATCH: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    /// 随机 128-bit 十六进制；写操作同时作为持久幂等键，查询不能重新提交操作。
    pub id: String,
    pub command: Command,
}
impl Request {
    pub fn new(id: String, command: Command) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            command,
        }
    }
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.version != PROTOCOL_VERSION {
            return Err(ProtocolError::Version);
        }
        if !valid_id(&self.id) {
            return Err(ProtocolError::Invalid);
        }
        self.command.validate()
    }
}

/// 成员只增补、不改义。未支持的能力明确报 Unsupported；绝不部分执行未知命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Command {
    Hello,
    Status,
    Activate {
        target: WindowTarget,
    },
    AttachUi {
        session: String,
    },
    UiHeartbeat {
        session: String,
        acknowledged: u64,
    },
    DetachUi {
        session: String,
    },
    PrepareShutdown,
    FinishShutdown {
        session: String,
        saved: bool,
    },
    Snapshot,
    Events {
        after: u64,
    },
    MutationResult {
        mutation: String,
    },
    Accounts,
    CheckAccount {
        account: Option<AccountId>,
        settings: AccountSettings,
        credential_ref: String,
    },
    AccountCheckStatus {
        task: String,
    },
    CommitAccount {
        check: String,
        account: Option<AccountId>,
    },
    SetDefaultAccount {
        account: AccountId,
    },
    AccountImpact {
        account: AccountId,
    },
    RemoveAccount {
        account: AccountId,
    },
    Catalog {
        account: AccountId,
    },
    Folders {
        account: AccountId,
    },
    FolderCache {
        account: AccountId,
        name: String,
        before: Option<u32>,
    },
    UnifiedInbox {
        before: Option<InboxCursor>,
        limit: u32,
    },
    Search {
        account: Option<AccountId>,
        query: SearchQuery,
    },
    SyncNow {
        account: Option<AccountId>,
        folder: Option<String>,
        before: Option<u32>,
    },
    PauseSync {
        account: Option<AccountId>,
        paused: bool,
    },
    ReadMessage {
        location: MailUid,
    },
    DownloadAttachment {
        resource: String,
    },
    CancelResource {
        resource: String,
    },
    ReadResource {
        resource: String,
        offset: u64,
        length: u32,
    },
    CreateUpload {
        account: AccountId,
        name: String,
        media_type: String,
        size: u64,
    },
    WriteResource {
        resource: String,
        offset: u64,
        length: u32,
    },
    FinishUpload {
        resource: String,
    },
    BrowserCopy {
        location: MailUid,
    },
    AllowRemoteImages {
        location: MailUid,
    },
    ClearBrowserCopies,
    OpenDraft {
        account: Option<AccountId>,
        initial: Draft,
    },
    NewDraft {
        account: Option<AccountId>,
        document: DraftDocument,
    },
    Drafts {
        account: Option<AccountId>,
    },
    SelectDraft {
        account: Option<AccountId>,
        draft: DraftId,
    },
    SaveDraft {
        account: AccountId,
        draft: DraftId,
        revision: u32,
        document: DraftDocument,
    },
    SelectedDraftBody {
        account: Option<AccountId>,
    },
    DiscardSelectedDraft {
        account: Option<AccountId>,
    },
    AttachmentInfos {
        account: AccountId,
        ids: Vec<AttachmentId>,
    },
    ReviewSend {
        account: AccountId,
        draft: DraftId,
        revision: u32,
    },
    ConfirmSend {
        review: String,
        allow_empty_subject: bool,
    },
    Tasks {
        account: AccountId,
    },
    DraftTasks {
        account: AccountId,
        draft: DraftId,
    },
    TaskSnapshot {
        account: AccountId,
        task: SendTaskId,
    },
    RetryCopy {
        account: AccountId,
        task: SendTaskId,
    },
    Resend {
        account: AccountId,
        task: SendTaskId,
    },
    StopUncertain {
        account: AccountId,
        task: SendTaskId,
    },
    EnqueueOperation {
        locations: Vec<MailUid>,
        operation: MailOperation,
    },
    Operations {
        account: Option<AccountId>,
    },
    RetryOperation {
        operation: String,
    },
    ResolveOperation {
        operation: String,
        resolution: OperationResolution,
    },
    SyncHistory {
        account: AccountId,
        folder: String,
        before: Option<u32>,
    },
    DraftConflicts {
        account: Option<AccountId>,
    },
    DraftSyncFolder {
        account: AccountId,
        folder: String,
    },
    ResolveDraftConflict {
        account: AccountId,
        conflict: String,
        resolution: DraftResolution,
    },
    CacheStatus,
    SetCacheBudget {
        bytes: u64,
    },
    ClearCache,
}
impl Command {
    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::UnifiedInbox { limit, .. } if !(1..=MAX_PAGE).contains(limit) => {
                Err(ProtocolError::Invalid)
            }
            Self::ReadResource {
                resource, length, ..
            }
            | Self::WriteResource {
                resource, length, ..
            } if !valid_id(resource) || !(1..=MAX_CHUNK_BYTES).contains(length) => {
                Err(ProtocolError::Invalid)
            }
            Self::EnqueueOperation { locations, .. }
                if locations.is_empty()
                    || locations.len() > MAX_BATCH
                    || locations.iter().any(|l| l.uid == 0 || l.uidvalidity == 0) =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::CheckAccount { credential_ref, .. } if !valid_id(credential_ref) => {
                Err(ProtocolError::Invalid)
            }
            Self::AccountCheckStatus { task } if !valid_id(task) => Err(ProtocolError::Invalid),
            Self::CommitAccount { check, account }
                if !valid_id(check) || account.as_ref().is_some_and(|id| !valid_id(&id.0)) =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::AttachmentInfos { ids, .. } if ids.len() > 32 => Err(ProtocolError::Invalid),
            Self::CreateUpload { name, size, .. }
                if *size > 25 * 1024 * 1024
                    || name.is_empty()
                    || name.len() > 1024
                    || name.contains(['/', '\\'])
                    || name.chars().any(char::is_control) =>
            {
                Err(ProtocolError::Invalid)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowTarget {
    Mailbox,
    Composer,
    Reader,
    Accounts,
    SendTasks,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxCursor {
    pub received_at: i64,
    pub account: AccountId,
    pub message: MessageId,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MailOperation {
    Flags {
        seen: Option<bool>,
        flagged: Option<bool>,
    },
    Move {
        destination: FolderId,
    },
    Delete,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Pending,
    InFlight,
    Applied,
    Failed,
    Conflict,
    Uncertain,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationResolution {
    KeepServer,
    Reconcile,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftResolution {
    KeepBoth,
    KeepLocal,
    KeepRemote,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u32,
    pub id: String,
    pub result: Result<Reply, RpcError>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionMismatch,
    InvalidRequest,
    Unsupported,
    Busy,
    NotFound,
    Conflict,
    Offline,
    Authentication,
    Storage,
    Uncertain,
    Internal,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub revision: u64,
    pub topic: EventTopic,
    pub account: Option<AccountId>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTopic {
    Accounts,
    Mail,
    Drafts,
    Operations,
    Resources,
    Sending,
    Shutdown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBatch {
    pub revision: u64,
    pub resnapshot: bool,
    pub events: Vec<Event>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceChunk {
    pub resource: String,
    pub offset: u64,
    pub length: u32,
    pub total: u64,
    pub complete: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub revision: u64,
    pub sync_paused: bool,
    pub sync_status: String,
    pub sending: bool,
    pub tray_status: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendPreview {
    pub token: String,
    pub sender: String,
    pub headers: DraftHeaders,
    pub attachments: usize,
    pub attachment_bytes: u64,
    pub preview: String,
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Hello 返回已接入的精确命令集合；接口存在不表示实现已启用。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandName {
    Hello,
    Status,
    Activate,
    AttachUi,
    UiHeartbeat,
    DetachUi,
    PrepareShutdown,
    FinishShutdown,
    Snapshot,
    Events,
    MutationResult,
    Accounts,
    CheckAccount,
    AccountCheckStatus,
    SetDefaultAccount,
    SyncHistory,
    DraftConflicts,
    CommitAccount,
    AccountImpact,
    RemoveAccount,
    Catalog,
    Folders,
    FolderCache,
    UnifiedInbox,
    Search,
    SyncNow,
    PauseSync,
    ReadMessage,
    DownloadAttachment,
    CancelResource,
    ReadResource,
    CreateUpload,
    WriteResource,
    FinishUpload,
    BrowserCopy,
    AllowRemoteImages,
    ClearBrowserCopies,
    OpenDraft,
    NewDraft,
    Drafts,
    SelectDraft,
    SaveDraft,
    SelectedDraftBody,
    DiscardSelectedDraft,
    AttachmentInfos,
    ReviewSend,
    ConfirmSend,
    Tasks,
    DraftTasks,
    TaskSnapshot,
    RetryCopy,
    Resend,
    StopUncertain,
    EnqueueOperation,
    Operations,
    RetryOperation,
    ResolveOperation,
    DraftSyncFolder,
    ResolveDraftConflict,
    CacheStatus,
    SetCacheBudget,
    ClearCache,
}
impl Command {
    pub fn name(&self) -> CommandName {
        match self {
            Self::SetDefaultAccount { .. } => CommandName::SetDefaultAccount,
            Self::SyncHistory { .. } => CommandName::SyncHistory,
            Self::DraftConflicts { .. } => CommandName::DraftConflicts,
            Self::Hello => CommandName::Hello,
            Self::Status => CommandName::Status,
            Self::Activate { .. } => CommandName::Activate,
            Self::AttachUi { .. } => CommandName::AttachUi,
            Self::UiHeartbeat { .. } => CommandName::UiHeartbeat,
            Self::DetachUi { .. } => CommandName::DetachUi,
            Self::PrepareShutdown => CommandName::PrepareShutdown,
            Self::FinishShutdown { .. } => CommandName::FinishShutdown,
            Self::Snapshot => CommandName::Snapshot,
            Self::Events { .. } => CommandName::Events,
            Self::MutationResult { .. } => CommandName::MutationResult,
            Self::Accounts => CommandName::Accounts,
            Self::CheckAccount { .. } => CommandName::CheckAccount,
            Self::AccountCheckStatus { .. } => CommandName::AccountCheckStatus,
            Self::CommitAccount { .. } => CommandName::CommitAccount,
            Self::AccountImpact { .. } => CommandName::AccountImpact,
            Self::RemoveAccount { .. } => CommandName::RemoveAccount,
            Self::Catalog { .. } => CommandName::Catalog,
            Self::Folders { .. } => CommandName::Folders,
            Self::FolderCache { .. } => CommandName::FolderCache,
            Self::UnifiedInbox { .. } => CommandName::UnifiedInbox,
            Self::Search { .. } => CommandName::Search,
            Self::SyncNow { .. } => CommandName::SyncNow,
            Self::PauseSync { .. } => CommandName::PauseSync,
            Self::ReadMessage { .. } => CommandName::ReadMessage,
            Self::DownloadAttachment { .. } => CommandName::DownloadAttachment,
            Self::CancelResource { .. } => CommandName::CancelResource,
            Self::ReadResource { .. } => CommandName::ReadResource,
            Self::CreateUpload { .. } => CommandName::CreateUpload,
            Self::WriteResource { .. } => CommandName::WriteResource,
            Self::FinishUpload { .. } => CommandName::FinishUpload,
            Self::BrowserCopy { .. } => CommandName::BrowserCopy,
            Self::AllowRemoteImages { .. } => CommandName::AllowRemoteImages,
            Self::ClearBrowserCopies => CommandName::ClearBrowserCopies,
            Self::OpenDraft { .. } => CommandName::OpenDraft,
            Self::NewDraft { .. } => CommandName::NewDraft,
            Self::Drafts { .. } => CommandName::Drafts,
            Self::SelectDraft { .. } => CommandName::SelectDraft,
            Self::SaveDraft { .. } => CommandName::SaveDraft,
            Self::SelectedDraftBody { .. } => CommandName::SelectedDraftBody,
            Self::DiscardSelectedDraft { .. } => CommandName::DiscardSelectedDraft,
            Self::AttachmentInfos { .. } => CommandName::AttachmentInfos,
            Self::ReviewSend { .. } => CommandName::ReviewSend,
            Self::ConfirmSend { .. } => CommandName::ConfirmSend,
            Self::Tasks { .. } => CommandName::Tasks,
            Self::DraftTasks { .. } => CommandName::DraftTasks,
            Self::TaskSnapshot { .. } => CommandName::TaskSnapshot,
            Self::RetryCopy { .. } => CommandName::RetryCopy,
            Self::Resend { .. } => CommandName::Resend,
            Self::StopUncertain { .. } => CommandName::StopUncertain,
            Self::EnqueueOperation { .. } => CommandName::EnqueueOperation,
            Self::Operations { .. } => CommandName::Operations,
            Self::RetryOperation { .. } => CommandName::RetryOperation,
            Self::ResolveOperation { .. } => CommandName::ResolveOperation,
            Self::DraftSyncFolder { .. } => CommandName::DraftSyncFolder,
            Self::ResolveDraftConflict { .. } => CommandName::ResolveDraftConflict,
            Self::CacheStatus => CommandName::CacheStatus,
            Self::SetCacheBudget { .. } => CommandName::SetCacheBudget,
            Self::ClearCache => CommandName::ClearCache,
        }
    }
}

/// 响应按业务类型区分；不把私有 StorageRequest、数据库行或发送许可暴露到协议。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Reply {
    Hello {
        commands: Vec<CommandName>,
        epoch: String,
    },
    Status(DaemonStatus),
    Snapshot(AppSnapshot),
    Events(EventBatch),
    Mutation(MutationStatus),
    Accounts(Vec<(AccountId, AccountSettings)>),
    AccountCheck(AccountCheckResult),
    AccountCheckPending {
        task: String,
    },
    AccountSaved(AccountId),
    AccountImpact(AccountImpact),
    Catalog {
        folders: Vec<RemoteFolder>,
        synced_at: Option<i64>,
    },
    Folders(Vec<Folder>),
    FolderCache(Option<FolderCache>),
    Inbox(MessagePage),
    Search(Vec<SearchHit>),
    Message(MessageView),
    Resource(ResourceChunk),
    ResourceCreated(ResourceRef),
    OptionalResource(Option<ResourceRef>),
    Attachment(AttachmentInfo),
    Attachments(Vec<AttachmentInfo>),
    Draft(StoredDraft),
    Drafts(Vec<StoredDraft>),
    SendPreview(SendPreview),
    Task(SendTask),
    Tasks(Vec<SendTask>),
    /// 冻结快照的展示/导出资源，不传携带附件字节的 OutgoingSnapshot。
    TaskResource(ResourceRef),
    Operations(Vec<OperationInfo>),
    DraftConflicts(Vec<DraftConflict>),
    Cache(CacheInfo),
    BrowserCopy {
        url: String,
    },
    Count(u64),
    Acknowledged,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSnapshot {
    pub epoch: String,
    pub revision: u64,
    pub status: DaemonStatus,
    pub accounts: Vec<(AccountId, AccountSettings)>,
    pub default_account: Option<AccountId>,
    pub pending_operations: u64,
    pub draft_conflicts: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "state",
    content = "result",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MutationStatus {
    NotFound,
    InProgress,
    /// 崩溃中断，需读取关联业务状态，不授权重放。
    Uncertain,
    Finished(Box<Result<Reply, RpcError>>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountCheckResult {
    /// 仅两端都通过时提供；短时 token 绑定非秘密配置与 staging 引用。
    pub check: Option<String>,
    pub imap: Result<Vec<RemoteFolder>, RpcError>,
    pub smtp: Result<(), RpcError>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessagePage {
    pub messages: Vec<CachedMessage>,
    pub next: Option<InboxCursor>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRef {
    pub id: String,
    pub size: u64,
    pub media_type: String,
    pub sha256: Option<String>,
    pub state: ResourceState,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    NotCached,
    Downloading,
    Ready,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageView {
    pub location: MailUid,
    pub subject: String,
    pub sender: String,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub reply_to: Vec<String>,
    pub date_unix: i64,
    pub date_text: String,
    /// application/vnd.qingmail.document+json；在资源通道传输受控树。
    pub document: ResourceRef,
    pub plain: ResourceRef,
    pub attachments: Vec<ReceivedAttachment>,
    pub images: Vec<ReceivedImage>,
    pub warnings: Vec<String>,
    /// 仅展示阻止数量；授权仍针对 location，不把任意 URL 交给 daemon。
    pub remote_images: u32,
    pub is_html: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivedAttachment {
    pub name: String,
    pub resource: ResourceRef,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivedImage {
    pub key: String,
    pub resource: ResourceRef,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationInfo {
    pub id: String,
    pub location: MailUid,
    pub operation: MailOperation,
    pub state: OperationState,
    pub detail: Option<RpcError>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftConflict {
    pub id: String,
    pub account: AccountId,
    pub local: DraftId,
    pub remote_copy: DraftId,
    pub original: ResourceRef,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheInfo {
    pub budget_bytes: u64,
    pub evictable_bytes: u64,
    pub protected_bytes: u64,
    pub downloading_bytes: u64,
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    #[test]
    fn responses_keep_request_identity_and_typed_failures() {
        let id = "f".repeat(32);
        let response = Response {
            version: 1,
            id: id.clone(),
            result: Ok(Reply::Mutation(MutationStatus::Uncertain)),
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        let decoded: Response = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.id, id);
        assert!(matches!(
            decoded.result,
            Ok(Reply::Mutation(MutationStatus::Uncertain))
        ));
        let error = Response {
            version: 1,
            id,
            result: Err(RpcError {
                code: ErrorCode::Conflict,
                message: "草稿版本已改变".into(),
            }),
        };
        assert!(matches!(
            serde_json::from_slice::<Response>(&serde_json::to_vec(&error).unwrap())
                .unwrap()
                .result,
            Err(RpcError {
                code: ErrorCode::Conflict,
                ..
            })
        ));
    }
    #[test]
    fn nested_credentials_cannot_be_silently_accepted() {
        let settings = AccountSettings {
            display_name: "test".into(),
            address: "a@example.test".into(),
            imap: ServerSettings {
                host: "imap.example.test".into(),
                port: 993,
                username: "a".into(),
                security: ConnectionSecurity::Tls,
            },
            smtp: ServerSettings {
                host: "smtp.example.test".into(),
                port: 465,
                username: "a".into(),
                security: ConnectionSecurity::Tls,
            },
        };
        let request = Request::new(
            "a".repeat(32),
            Command::CheckAccount {
                account: None,
                settings,
                credential_ref: "b".repeat(32),
            },
        );
        let mut json = serde_json::to_value(request).unwrap();
        json["command"]["args"]["settings"]["imap"]["password"] = "do-not-accept".into();
        assert!(serde_json::from_value::<Request>(json).is_err());
    }
}
