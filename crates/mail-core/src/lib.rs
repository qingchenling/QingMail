//! 邮件领域模型与操作语义。
//!
//! 不依赖 GPUI，不依赖任何具体同步或存储实现（主文档 §3.1）。

/// 账号本地稳定 ID。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountId(pub String);

/// IMAP 邮件定位四元组：账号、文件夹稳定 ID、UIDVALIDITY 与 UID 确定一封邮件（主文档 §4.1）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MailUid {
    pub account: AccountId,
    /// 本地稳定文件夹 ID，不直接使用服务端文件夹路径。
    pub folder_id: String,
    /// IMAP UIDVALIDITY，改变时须重建文件夹索引。
    pub uidvalidity: u32,
    pub uid: u32,
}

/// 通用账号的非秘密配置；密码和令牌由 Secret Service 单独管理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSettings {
    pub display_name: String,
    pub address: String,
    pub imap: ServerSettings,
    pub smtp: ServerSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub security: ConnectionSecurity,
}

/// 不提供明文和跳过证书验证选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionSecurity {
    Tls,
    StartTls,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FolderId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    pub account: AccountId,
    /// 服务端文件夹定位，不以本地显示名冒充稳定 ID。
    pub remote_name: String,
    pub uidvalidity: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEnvelope {
    pub subject: String,
    pub sender: String,
    pub received_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedMessage {
    pub id: MessageId,
    pub location: MailUid,
    pub envelope: MessageEnvelope,
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
