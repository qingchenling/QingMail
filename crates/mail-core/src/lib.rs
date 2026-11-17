//! 邮件领域模型与操作语义。
//!
//! 不依赖 GPUI，不依赖任何具体同步或存储实现（主文档 §3.1）。

mod mime;
pub use mime::{MimePart, MimeStructure};

use serde::{Deserialize, Serialize};

/// 账号本地稳定 ID。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct AccountId(pub String);

/// IMAP 邮件定位四元组：账号、文件夹稳定 ID、UIDVALIDITY 与 UID 确定一封邮件（主文档 §4.1）。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct MailUid {
    pub account: AccountId,
    /// 本地稳定文件夹 ID，不直接使用服务端文件夹路径。
    pub folder_id: String,
    /// IMAP UIDVALIDITY，改变时须重建文件夹索引。
    pub uidvalidity: u32,
    pub uid: u32,
}

/// 通用账号的非秘密配置；密码和令牌由 Secret Service 单独管理。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountSettings {
    pub display_name: String,
    pub address: String,
    pub imap: ServerSettings,
    pub smtp: ServerSettings,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub security: ConnectionSecurity,
}

/// 不提供明文和跳过证书验证选项。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionSecurity {
    Tls,
    StartTls,
}

/// LIST 成功快照；目录节点可以不可选择，不虚构 UIDVALIDITY。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteFolder {
    pub name: String,
    pub display_name: String,
    pub delimiter: Option<String>,
    pub selectable: bool,
}
/// 一个文件夹的有界索引批次；Recent 表示最近窗口，History 表示主动更早历史。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchKind {
    Recent,
    History,
}
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MessageFlags {
    pub seen: bool,
    pub flagged: bool,
}
/// 一个文件夹的有界索引批次，不代表完整邮箱镜像。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FolderBatch {
    pub remote_name: String,
    pub uidvalidity: u32,
    pub since: i64,
    pub has_more: bool,
    /// 本次是否覆盖整个查询范围（无分页游标）；只有完整最近批次可对账删除。
    pub complete: bool,
    pub kind: BatchKind,
    pub messages: Vec<(u32, MessageEnvelope)>,
    /// 每封邮件的远端标记；缺失时按未读/未星标处理。
    pub flags: std::collections::BTreeMap<u32, MessageFlags>,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FolderCache {
    pub folder: Folder,
    pub synced_at: i64,
    pub since: i64,
    pub has_more: bool,
    pub messages: Vec<CachedMessage>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct FolderId(pub String);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct MessageId(pub String);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Folder {
    pub id: FolderId,
    pub account: AccountId,
    /// 服务端文件夹定位，不以本地显示名冒充稳定 ID。
    pub remote_name: String,
    pub uidvalidity: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MessageEnvelope {
    pub subject: String,
    pub sender: String,
    pub received_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CachedMessage {
    pub id: MessageId,
    pub location: MailUid,
    pub envelope: MessageEnvelope,
    pub seen: bool,
    pub flagged: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct DraftId(pub String);
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct AttachmentId(pub String);
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct SendTaskId(pub String);

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DraftHeaders {
    pub subject: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendState {
    Pending,
    Sending,
    Sent,
    Failed,
    Uncertain,
    CopyPending,
}

/// 调用方须先取得明确的用户决定，不能把普通自动重试包装成重发。
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum ResendAuthorization {
    UserConfirmed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum SmtpOutcome {
    Accepted,
    DefinitelyRejected,
    Uncertain,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_uid_identity() {
        let a = MailUid {
            account: AccountId("a1".into()),
            folder_id: "folder-1".into(),
            uidvalidity: 42,
            uid: 7,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}

/// 供存储与协议层共享的持久发送快照数据；不含凭据。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttachmentInfo {
    pub id: AttachmentId,
    pub name: String,
    pub media_type: String,
    pub size: u32,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutgoingAttachment {
    pub info: AttachmentInfo,
    pub bytes: Vec<u8>,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutgoingSnapshot {
    pub message_id: String,
    pub date_unix: i64,
    pub sender: String,
    pub headers: DraftHeaders,
    pub plain: String,
    pub html: String,
    pub attachments: Vec<OutgoingAttachment>,
}
