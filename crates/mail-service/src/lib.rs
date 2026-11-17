//! 通用账号、只读同步、发送校验/提交与已发送副本适配；UI 通过 mail-daemon 协调器调用。
pub mod accounts;
pub mod actions;
pub mod credentials;
pub mod folders;
pub mod idle;
pub mod sent;
pub mod sync;

pub mod outgoing;

pub mod parts;
