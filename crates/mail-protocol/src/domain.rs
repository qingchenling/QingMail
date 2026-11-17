//! 跨进程值对象；不包含存储实现、凭据或网络许可。
use mail_content::draft::Draft;
use mail_core::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DraftDocument {
    pub headers: DraftHeaders,
    pub body: Draft,
    pub attachments: Vec<AttachmentId>,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredDraft {
    pub id: DraftId,
    pub account: AccountId,
    pub revision: u32,
    pub document: DraftDocument,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SendTask {
    pub id: SendTaskId,
    pub account: AccountId,
    pub source_draft: DraftId,
    pub source_revision: u32,
    pub sender: String,
    /// 冻结的收件人与主题；主题在 `recipients.subject`，不另存一份。
    pub recipients: DraftHeaders,
    pub created_at: i64,
    pub state: SendState,
    /// 固定诊断代码，不接受包含密码或服务器原始响应的错误字符串。
    pub detail: String,
    pub resend_of: Option<SendTaskId>,
    /// 只有副本确认保存或查重命中后才存在；不表示允许重发。
    pub copy: Option<SentCopy>,
}
/// 已保存副本的服务器位置。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SentCopy {
    pub folder: String,
    pub uidvalidity: u32,
    /// 服务器未提供 APPENDUID 时为 None。
    pub uid: Option<u32>,
}
/// 本地搜索条件；只查询已缓存索引，不访问网络。
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    pub text: String,
    pub sender: String,
    pub has_attachments: bool,
    /// None 表示整个账号范围；Some 限定一个远端文件夹名。
    pub folder: Option<String>,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchHit {
    pub message: CachedMessage,
    pub folder: String,
}

/// 本地移除账号前的影响统计；不含远端邮件。
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountImpact {
    pub cached_messages: u64,
    pub drafts: u64,
    pub tasks: u64,
    pub pending_tasks: u64,
    pub object_bytes: u64,
}
