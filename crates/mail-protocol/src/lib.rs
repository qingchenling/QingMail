//! QingMail IPC v4：有界 NDJSON 控制帧；资源数据通过单独连接的二进制帧传输。
mod business;
mod desktop;
pub use desktop::*;
mod domain;
pub use business::*;
pub mod transport;
mod wire;
pub use domain::*;
use mail_content::draft::Draft;
use mail_core::*;
use serde::{Deserialize, Serialize};
pub use wire::*;

pub const PROTOCOL_VERSION: u32 = 4;
pub const MAX_STORAGE_PAYLOAD: u64 = 16 * 1024 * 1024;
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
    CreateStorageRequest {
        size: u64,
    },
    StorageRequest {
        resource: String,
    },
    DraftPage {
        account: Option<AccountId>,
        after: Option<String>,
    },
    TaskPage {
        account: AccountId,
        after: Option<String>,
    },
    Business {
        operation: BusinessOperation,
    },
    JobResult {
        task: String,
    },
    Hello,
    AutostartSettings,
    SetAutostart {
        enabled: bool,
        revision: u64,
    },
    DesktopSettings,
    UpdateDesktopSettings {
        preferences: DesktopPreferences,
    },
    OpenMail {
        target: OpenMailTarget,
    },
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
    AccountOptions,
    Accounts,
    BeginMicrosoftLogin {
        address: String,
        authentication: Authentication,
    },
    MicrosoftLoginStatus {
        session: String,
    },
    CancelMicrosoftLogin {
        session: String,
    },
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
    ResourceStatus {
        resource: String,
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
    /// v3: user deletion removes the editor draft and durably queues known remote versions for Trash.
    DiscardDraft {
        account: AccountId,
        draft: DraftId,
        revision: u32,
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
    DraftSyncStatus {
        account: AccountId,
    },
    FlushDraftSync {
        account: AccountId,
    },
    ServerDrafts {
        account: AccountId,
    },
    ConvertServerDraft {
        account: AccountId,
        source: String,
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
            Self::OpenMail { target }
                if !valid_id(&target.account.0)
                    || target.message.as_ref().is_some_and(|m| {
                        m.account != target.account
                            || !valid_id(&m.folder_id)
                            || m.uid == 0
                            || m.uidvalidity == 0
                    }) =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::CreateStorageRequest { size } if *size == 0 || *size > MAX_STORAGE_PAYLOAD => {
                Err(ProtocolError::Invalid)
            }
            Self::ResolveDraftConflict { conflict, .. } if !valid_id(conflict) => {
                Err(ProtocolError::Invalid)
            }
            Self::DraftPage {
                after: Some(after), ..
            }
            | Self::TaskPage {
                after: Some(after), ..
            } if after.len() > 128 => Err(ProtocolError::Invalid),
            Self::StorageRequest { resource } if !valid_id(resource) => Err(ProtocolError::Invalid),
            Self::Business { operation } => operation.validate(),
            Self::JobResult { task } if !valid_id(task) => Err(ProtocolError::Invalid),
            Self::AttachUi { session }
            | Self::DetachUi { session }
            | Self::UiHeartbeat { session, .. }
            | Self::FinishShutdown { session, .. }
                if !valid_id(session) =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::FinishUpload { resource }
            | Self::CancelResource { resource }
            | Self::ResourceStatus { resource }
            | Self::DownloadAttachment { resource }
                if !valid_id(resource) =>
            {
                Err(ProtocolError::Invalid)
            }
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
            Self::BeginMicrosoftLogin {
                address,
                authentication,
            } if address.starts_with('@')
                || address.ends_with('@')
                || address.len() > 320
                || address.matches('@').count() != 1
                || address.chars().any(|c| c.is_control() || c.is_whitespace())
                || *authentication == Authentication::Password =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::MicrosoftLoginStatus { session } | Self::CancelMicrosoftLogin { session }
                if !valid_id(session) =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::CheckAccount { credential_ref, .. } if !valid_id(credential_ref) => {
                Err(ProtocolError::Invalid)
            }
            Self::AccountCheckStatus { task } if !valid_id(task) => Err(ProtocolError::Invalid),
            Self::RetryOperation { operation } | Self::ResolveOperation { operation, .. }
                if !valid_id(operation) =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::CommitAccount { check, account }
                if !valid_id(check) || account.as_ref().is_some_and(|id| !valid_id(&id.0)) =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::DraftSyncFolder { folder, .. }
                if folder.is_empty()
                    || folder.len() > 4096
                    || folder.chars().any(char::is_control) =>
            {
                Err(ProtocolError::Invalid)
            }
            Self::ConvertServerDraft { source, .. } if !valid_id(source) => {
                Err(ProtocolError::Invalid)
            }
            Self::DiscardDraft {
                account,
                draft,
                revision,
            } if !valid_id(&account.0) || !valid_id(&draft.0) || *revision == 0 => {
                Err(ProtocolError::Invalid)
            }
            Self::AttachmentInfos { ids, .. } if ids.len() > 32 => Err(ProtocolError::Invalid),
            Self::CreateUpload {
                name,
                size,
                media_type,
                ..
            } if *size > 25 * 1024 * 1024
                || matches!(name.as_str(), "." | "..")
                || media_type.len() > 128
                || !media_type.contains('/')
                || !media_type
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/!#$&^_.+-".contains(&b))
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowTarget {
    Mailbox,
    Mail { target: OpenMailTarget },
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
    /// Frozen remote Trash name; does not require a locally synchronized target epoch.
    Trash {
        destination: String,
    },
    /// Permanent deletion, only enqueued when the source is the resolved Trash folder.
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
    AccountOptions,
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
pub struct AccountSyncState {
    pub account: AccountId,
    pub paused: bool,
    pub status: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    #[serde(default)]
    pub account_sync: Vec<AccountSyncState>,
    pub pid: u32,
    pub revision: u64,
    pub sync_paused: bool,
    pub sync_status: String,
    pub sending: bool,
    pub account_saving: bool,
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
    CreateStorageRequest,
    StorageRequest,
    DraftPage,
    TaskPage,
    Business,
    JobResult,
    Hello,
    AutostartSettings,
    SetAutostart,
    DesktopSettings,
    UpdateDesktopSettings,
    OpenMail,
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
    AccountOptions,
    Accounts,
    BeginMicrosoftLogin,
    MicrosoftLoginStatus,
    CancelMicrosoftLogin,
    CheckAccount,
    AccountCheckStatus,
    SetDefaultAccount,
    SyncHistory,
    DraftSyncStatus,
    FlushDraftSync,
    ServerDrafts,
    ConvertServerDraft,
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
    ResourceStatus,
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
    DiscardDraft,
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
            Self::CreateStorageRequest { .. } => CommandName::CreateStorageRequest,
            Self::StorageRequest { .. } => CommandName::StorageRequest,
            Self::DraftPage { .. } => CommandName::DraftPage,
            Self::TaskPage { .. } => CommandName::TaskPage,
            Self::Business { .. } => CommandName::Business,
            Self::JobResult { .. } => CommandName::JobResult,
            Self::SetDefaultAccount { .. } => CommandName::SetDefaultAccount,
            Self::SyncHistory { .. } => CommandName::SyncHistory,
            Self::DraftSyncStatus { .. } => CommandName::DraftSyncStatus,
            Self::FlushDraftSync { .. } => CommandName::FlushDraftSync,
            Self::ServerDrafts { .. } => CommandName::ServerDrafts,
            Self::ConvertServerDraft { .. } => CommandName::ConvertServerDraft,
            Self::DraftConflicts { .. } => CommandName::DraftConflicts,
            Self::Hello => CommandName::Hello,
            Self::AutostartSettings => CommandName::AutostartSettings,
            Self::SetAutostart { .. } => CommandName::SetAutostart,
            Self::DesktopSettings => CommandName::DesktopSettings,
            Self::UpdateDesktopSettings { .. } => CommandName::UpdateDesktopSettings,
            Self::OpenMail { .. } => CommandName::OpenMail,
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
            Self::AccountOptions => CommandName::AccountOptions,
            Self::Accounts => CommandName::Accounts,
            Self::BeginMicrosoftLogin { .. } => CommandName::BeginMicrosoftLogin,
            Self::MicrosoftLoginStatus { .. } => CommandName::MicrosoftLoginStatus,
            Self::CancelMicrosoftLogin { .. } => CommandName::CancelMicrosoftLogin,
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
            Self::ResourceStatus { .. } => CommandName::ResourceStatus,
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
            Self::DiscardDraft { .. } => CommandName::DiscardDraft,
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
    AccountOptions {
        default: Option<AccountId>,
        paused: Vec<AccountId>,
    },
    Encoded(ResourceRef),
    DraftPage {
        items: Vec<StoredDraft>,
        next: Option<String>,
    },
    TaskPage {
        items: Vec<SendTask>,
        next: Option<String>,
    },
    Pending {
        task: String,
    },
    Job {
        result: Option<Box<Result<Reply, RpcError>>>,
    },
    History {
        count: u64,
        has_more: bool,
    },
    Moved {
        count: u64,
        copied_only: bool,
        destination: String,
    },
    Hello {
        commands: Vec<CommandName>,
        epoch: String,
    },
    Status(DaemonStatus),
    /// attached=false 表示已有前台；不返回现有会话的秘密令牌。
    UiAttached {
        attached: bool,
    },
    UiState {
        activation: Option<Activation>,
        shutdown_requested: bool,
    },
    Snapshot(AppSnapshot),
    Events(EventBatch),
    Mutation(MutationStatus),
    Accounts(Vec<(AccountId, AccountSettings)>),
    MicrosoftLoginStarted {
        session: String,
        authorization_url: String,
    },
    MicrosoftLoginPending,
    MicrosoftLoginReady {
        credential_ref: String,
    },
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
    Download(DownloadInfo),
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
    ServerDrafts {
        folder: Option<String>,
        items: Vec<ServerDraftSource>,
        #[serde(default)]
        notice: String,
    },
    ServerDraftIndex {
        folder: Option<String>,
        uidvalidity: u32,
        uids: Vec<u32>,
        has_more: bool,
        choices: Vec<RemoteFolder>,
    },
    ServerDraftSource(ServerDraftSource),
    DraftSyncStatus(Vec<DraftSyncInfo>),
    DraftConflicts(Vec<DraftConflict>),
    Cache(CacheInfo),
    AutostartSettings(AutostartSettings),
    DesktopSettings(DesktopSettings),
    BrowserCopy {
        url: String,
    },
    Count(u64),
    Acknowledged,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub sequence: u64,
    pub target: WindowTarget,
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
    pub location: Option<MailUid>,
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
    pub remote_copy: Option<DraftId>,
    /// 受控原件ID，通过ReadServerDraft读取；删除冲突可能无远端原件。
    pub original: Option<String>,
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
                authentication: Default::default(),
                host: "imap.example.test".into(),
                port: 993,
                username: "a".into(),
                security: ConnectionSecurity::Tls,
            },
            smtp: ServerSettings {
                authentication: Default::default(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadInfo {
    pub resource: ResourceRef,
    /// 网络获取的编码字节数；Ready时resource.size为完整解码大小。
    pub received: u64,
    pub total: u64,
    pub error: Option<String>,
}

#[cfg(test)]
mod draft_conflict_contract_tests {
    use super::*;
    #[test]
    fn missing_remote_is_explicit_and_resolution_requires_controlled_identity() {
        let reply = Reply::DraftConflicts(vec![DraftConflict {
            id: "a".repeat(32),
            account: AccountId("b".repeat(32)),
            local: DraftId("c".repeat(32)),
            remote_copy: None,
            original: None,
            reason: "远端已删除，保留本地".into(),
        }]);
        let json = serde_json::to_vec(&reply).unwrap();
        let roundtrip: Reply = serde_json::from_slice(&json).unwrap();
        assert!(
            matches!(roundtrip,Reply::DraftConflicts(v) if v[0].original.is_none() && v[0].remote_copy.is_none())
        );
        let request = Request::new(
            "d".repeat(32),
            Command::ResolveDraftConflict {
                account: AccountId("b".repeat(32)),
                conflict: "../outside".into(),
                resolution: DraftResolution::KeepBoth,
            },
        );
        assert!(request.validate().is_err());
    }
}

#[cfg(test)]
mod microsoft_protocol_tests {
    use super::*;
    #[test]
    fn microsoft_ipc_v4_validates_public_configuration_and_rejects_old_peers() {
        let command = Command::BeginMicrosoftLogin {
            address: "test@outlook.com".into(),
            authentication: Authentication::MicrosoftPersonal,
        };
        let mut request = Request::new("a".repeat(32), command);
        assert!(request.validate().is_ok());
        request.version = 3;
        assert!(matches!(request.validate(), Err(ProtocolError::Version)));
        request.version = PROTOCOL_VERSION;
        let json = serde_json::to_value(&request).unwrap();
        let mut injected = json.clone();
        injected["command"]["args"]["access_token"] = "must-not-enter-ipc".into();
        assert!(serde_json::from_value::<Request>(injected).is_err());
        let mut invalid = json.clone();
        invalid["command"]["args"]["authentication"] = "password".into();
        assert!(
            serde_json::from_value::<Request>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut invalid = json;
        invalid["command"]["args"]["client_id"] = "cannot-override-built-in-id".into();
        assert!(serde_json::from_value::<Request>(invalid).is_err());
        for command in [
            Command::MicrosoftLoginStatus {
                session: "../bad".into(),
            },
            Command::CancelMicrosoftLogin { session: "".into() },
        ] {
            assert!(Request::new("a".repeat(32), command).validate().is_err());
        }
    }
}
