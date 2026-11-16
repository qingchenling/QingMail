//! IMAP 同步、SMTP 发送与认证适配。
//!
//! 不直接更新界面（主文档 §3.1）。首发仅支持通用 IMAP/SMTP 账号
//! （ADR-0002）；发送遵循禁止自动重发的状态机（ADR-0005）。

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        // P2 任务：通用 IMAP/SMTP 账号闭环。
    }
}
