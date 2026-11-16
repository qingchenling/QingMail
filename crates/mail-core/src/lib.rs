//! 邮件领域模型与操作语义。
//!
//! 不依赖 GPUI，不依赖任何具体同步或存储实现（主文档 §3.1）。

/// 账号本地稳定 ID。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountId(pub String);

/// IMAP 邮件定位三元组：文件夹唯一性与 UID 共同确定一封邮件（主文档 §4.1）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MailUid {
    pub account: AccountId,
    pub folder_id: String,
    /// IMAP UIDVALIDITY，改变时须重建文件夹索引。
    pub uidvalidity: u32,
    pub uid: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_uid_identity() {
        let a = MailUid {
            account: AccountId("a1".into()),
            folder_id: "INBOX".into(),
            uidvalidity: 42,
            uid: 7,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
