//! 前后台本地通信协议类型。
//!
//! P0–P2 为进程内调用，类型自 P3 拆分起用于 Unix socket JSON 编解码
//! （ADR-0003）。消息类型、版本与请求 ID 的定义从 P2 冻结为 v1。

/// 协议版本，随冻结进度递增。
pub const PROTOCOL_VERSION: u32 = 0;

#[cfg(test)]
mod tests {
    #[test]
    fn protocol_version_placeholder() {
        assert_eq!(super::PROTOCOL_VERSION, 0);
    }
}
