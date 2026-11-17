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

/// 保留的不可变服务器原件；编辑副本不覆盖它。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerDraftSource {
    pub id: String,
    pub account: AccountId,
    pub folder: String,
    pub uidvalidity: u32,
    pub uid: u32,
    pub subject: String,
    pub size: u32,
    pub converted: Option<DraftId>,
}

#[cfg(test)]
mod server_draft_contract_tests {
    use crate::*;
    #[test]
    fn server_draft_commands_validate_and_roundtrip_without_paths_or_mime_payloads() {
        let account = AccountId("a".repeat(32));
        for command in [
            Command::ServerDrafts {
                account: account.clone(),
            },
            Command::ConvertServerDraft {
                account: account.clone(),
                source: "b".repeat(32),
            },
            Command::DraftSyncFolder {
                account: account.clone(),
                folder: "自选草稿".into(),
            },
            Command::Business {
                operation: BusinessOperation::ImportServerDraft {
                    account: account.clone(),
                    folder: "Drafts".into(),
                    uidvalidity: 42,
                    uid: 7,
                },
            },
        ] {
            let request = Request::new("c".repeat(32), command);
            request.validate().unwrap();
            let wire = serde_json::to_vec(&request).unwrap();
            let decoded: Request = serde_json::from_slice(&wire).unwrap();
            decoded.validate().unwrap();
            assert_eq!(wire, serde_json::to_vec(&decoded).unwrap());
        }
        for command in [
            Command::ConvertServerDraft {
                account: account.clone(),
                source: "../arbitrary-file".into(),
            },
            Command::DraftSyncFolder {
                account: account.clone(),
                folder: "Drafts\r\nDELETE INBOX".into(),
            },
            Command::Business {
                operation: BusinessOperation::ReadServerDraft {
                    account: account.clone(),
                    source: "/tmp/raw.eml".into(),
                },
            },
            Command::Business {
                operation: BusinessOperation::ImportServerDraft {
                    account,
                    folder: "Drafts".into(),
                    uidvalidity: 0,
                    uid: 7,
                },
            },
        ] {
            assert!(Request::new("d".repeat(32), command).validate().is_err());
        }
    }
}
