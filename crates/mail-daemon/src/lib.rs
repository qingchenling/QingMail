//! 后台入口、通知、托盘与进程管理。
//!
//! P0–P2 以库形式被前台进程内调用（ADR-0003）；P3 拆分为独立 daemon
//! 进程并承载 Unix socket IPC、实例锁与单实例语义。

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        // P0 任务：无真实邮箱的后台生命周期（进程内等效验证）。
    }
}
