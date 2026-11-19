//! 持久邮件操作状态的统一文案。
use mail_protocol::OperationState as State;

pub(super) fn state_label(state: State) -> &'static str {
    match state {
        State::Pending => "待同步",
        State::InFlight => "执行中",
        State::Applied => "已处理",
        State::Failed => "失败",
        State::Conflict => "冲突",
        State::Uncertain => "结果不确定",
    }
}
