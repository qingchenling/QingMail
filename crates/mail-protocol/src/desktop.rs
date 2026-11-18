//! 桌面偏好由后台持久化，外观偏好仍属于前台。
use mail_core::{AccountId, MailUid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DesktopPreferences {
    pub revision: u64,
    pub notifications: bool,
    pub show_sender_subject: bool,
    pub show_preview: bool,
}
impl Default for DesktopPreferences {
    fn default() -> Self {
        Self {
            revision: 0,
            notifications: true,
            show_sender_subject: false,
            show_preview: false,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopSettings {
    pub preferences: DesktopPreferences,
    /// 调用成功仅表示通知服务接受，不表示已显示或已读。
    pub notification_status: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OpenMailTarget {
    pub account: AccountId,
    /// None 表示账号收件箱摘要。
    pub message: Option<MailUid>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, PROTOCOL_VERSION, ProtocolError, Request, read_request};
    #[test]
    fn desktop_protocol_rejects_old_version_and_cross_account_target() {
        let raw = format!(
            "{{\"version\":1,\"id\":\"{}\",\"command\":{{\"type\":\"update_desktop_settings\"}}}}\n",
            "a".repeat(32)
        );
        assert!(matches!(
            read_request(&mut std::io::Cursor::new(raw)),
            Err(ProtocolError::Version)
        ));
        let mut target = OpenMailTarget {
            account: AccountId("a".repeat(32)),
            message: Some(MailUid {
                account: AccountId("b".repeat(32)),
                folder_id: "c".repeat(32),
                uidvalidity: 1,
                uid: 1,
            }),
        };
        assert!(
            Request::new(
                "d".repeat(32),
                Command::OpenMail {
                    target: target.clone()
                }
            )
            .validate()
            .is_err()
        );
        target.message.as_mut().unwrap().account = target.account.clone();
        let request = Request::new("d".repeat(32), Command::OpenMail { target });
        assert_eq!(request.version, PROTOCOL_VERSION);
        assert!(request.validate().is_ok());
        assert!(serde_json::from_str::<DesktopPreferences>(r#"{"revision":0,"notifications":true,"show_sender_subject":false,"show_preview":false,"password":"secret"}"#).is_err());
    }
}
