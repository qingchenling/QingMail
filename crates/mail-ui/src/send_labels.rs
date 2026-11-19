//! 发送状态与固定诊断代码的统一文案；写信与邮件阅读状态共用。
use mail_core::SendState;
use mail_storage::SendTask;

/// 只翻译固定诊断代码；未知代码不展示服务器原文。
pub(crate) fn detail_label(detail: &str) -> Option<&'static str> {
    Some(match detail {
        "" | "copy_already_present" => return None,
        "smtp_rejected" => "SMTP 服务器明确拒绝",
        "interrupted_before_submission" => "提交前进程中断，服务器未接受",
        "cancelled_before_submission" => "已取消，未提交",
        "smtp_result_unknown" => "SMTP 结果未知",
        "interrupted_during_submission" => "提交过程中进程中断",
        "user_stopped_processing" => "已停止处理，未自动重试",
        "copy_no_sent_folder" => "未找到服务器“已发送”文件夹；请添加或重命名后重试副本",
        "copy_auth_failed" => "IMAP 认证失败；请检查凭据后重试副本",
        "copy_tls_failed" => "IMAP 加密连接失败",
        "copy_connection_failed" => "暂时无法连接 IMAP 服务器",
        "copy_timeout" => "IMAP 连接或响应超时",
        "copy_protocol_failed" => "服务器响应不符合预期",
        "copy_search_failed" => "无法确认是否已有同一副本，已停止追加以免重复",
        "copy_append_rejected" => "服务器拒绝保存已发送副本",
        "copy_limit" => "邮件或响应超过副本保存上限",
        "copy_result_unknown" => "副本是否已保存未知；重试前会先查重",
        "copy_prepare_failed" => "无法重建冻结的发送内容，副本未保存",
        "copy_invalid" => "副本保存参数无效",
        _ => "未记录更多原因",
    })
}

pub(crate) fn state_label(task: &SendTask) -> String {
    let base = match task.state {
        SendState::Pending => "待提交；不会自动发送",
        SendState::Sending => "正在交给邮件服务器；请勿重复发送",
        SendState::Sent => {
            if task.detail == "copy_already_present" {
                "发送成功；服务器上已有同一副本，未重复保存"
            } else if task.copy.is_some() {
                "发送成功，已保存副本"
            } else {
                "发送成功"
            }
        }
        SendState::Failed => "发送未完成；不会自动重试",
        SendState::Uncertain => "结果不确定：收件人可能已收到，请勿重复发送",
        SendState::CopyPending => "邮件服务器已接受；已发送副本尚未保存，请勿重发",
    };
    if matches!(
        task.state,
        SendState::Failed | SendState::Uncertain | SendState::CopyPending
    ) && let Some(detail) = detail_label(&task.detail)
    {
        return format!("{base}。{detail}");
    }
    base.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_core::{AccountId, DraftHeaders, DraftId, SendTaskId};
    use mail_storage::SentCopy;

    fn task(state: SendState, detail: &str) -> SendTask {
        SendTask {
            id: SendTaskId("0123456789abcdef0123456789abcdef".into()),
            account: AccountId("account".into()),
            source_draft: DraftId("draft".into()),
            source_revision: 1,
            sender: "sender@example.invalid".into(),
            recipients: DraftHeaders {
                subject: "主题".into(),
                to: vec!["to@example.invalid".into()],
                ..Default::default()
            },
            created_at: 1,
            state,
            detail: detail.into(),
            resend_of: None,
            copy: None,
        }
    }

    #[test]
    fn fixed_details_are_translated_and_unknown_text_is_not_echoed() {
        let failed = task(SendState::Failed, "smtp_rejected");
        assert!(state_label(&failed).contains("SMTP 服务器明确拒绝"));
        let uncertain = task(SendState::Uncertain, "user_stopped_processing");
        assert!(state_label(&uncertain).contains("已停止处理"));
        let copy = task(SendState::CopyPending, "copy_no_sent_folder");
        assert!(state_label(&copy).contains("未找到服务器"));
        let unknown = task(SendState::Failed, "server said: password");
        assert_eq!(
            state_label(&unknown),
            "发送未完成；不会自动重试。未记录更多原因"
        );
        assert_eq!(state_label(&task(SendState::Sent, "")), "发送成功");
        let sent = task(SendState::Sent, "copy_already_present");
        assert!(state_label(&sent).contains("未重复保存"));
        assert!(copy.copy.is_none());
        let with_copy = SendTask {
            copy: Some(SentCopy {
                folder: "Sent".into(),
                uidvalidity: 1,
                uid: Some(2),
            }),
            ..sent
        };
        assert!(state_label(&with_copy).contains("未重复保存"));
    }
}
