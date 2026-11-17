//! 正式前台启动器：只在后台不存在时启动；协议不兼容时不执行写操作。
use mail_protocol::{
    transport::{Client, ClientError, request_id},
    *,
};
use std::{
    io,
    path::PathBuf,
    process::{Command as Process, Stdio},
    time::{Duration, Instant},
};
#[derive(Clone)]
pub struct Frontend {
    pub client: Client,
    pub session: String,
    pub snapshot: AppSnapshot,
}
pub fn runtime_root() -> io::Result<PathBuf> {
    if let Some(root) = std::env::var_os("QINGMAIL_IPC_ROOT") {
        return Ok(PathBuf::from(root).join("runtime"));
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| io::Error::other("缺少绝对 XDG_RUNTIME_DIR"))
}
pub fn connect(target: WindowTarget) -> Result<Option<Frontend>, String> {
    let client = Client::new(runtime_root().map_err(|e| e.to_string())?);
    match client.request(Command::Hello) {
        Ok(reply) => compatible(reply)?,
        Err(ClientError::Protocol(ProtocolError::Io(e)))
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            let executable = std::env::current_exe()
                .map_err(|e| e.to_string())?
                .with_file_name("mail-daemon");
            let mut child = Process::new(executable);
            if let Some(root) = std::env::var_os("QINGMAIL_IPC_ROOT") {
                child.arg("--isolated-root").arg(root);
            } else {
                child.arg("--production");
            }
            let mut child = child
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("无法启动同目录 mail-daemon：{e}"))?;
            let deadline = Instant::now() + Duration::from_secs(12);
            loop {
                match client.request(Command::Hello) {
                    Ok(reply) => {
                        compatible(reply)?;
                        break;
                    }
                    Err(ClientError::Remote(error)) => return Err(error.message),
                    Err(_) => (),
                }
                if Instant::now() >= deadline {
                    return Err("后台未能就绪；请检查 mail-daemon 日志和数据迁移状态".into());
                }
                let _ = child.try_wait();
                std::thread::sleep(Duration::from_millis(80));
            }
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => return Err(error.to_string()),
    }
    let session = request_id().map_err(|e| e.to_string())?;
    let Reply::UiAttached { attached } = client
        .request(Command::AttachUi {
            session: session.clone(),
        })
        .map_err(|e| e.to_string())?
    else {
        return Err("后台会话响应不兼容".into());
    };
    // 每次启动都触发一次合并收信；隔离验证后台明确拒绝联网是预期行为。
    let _ = client.request(Command::SyncNow {
        account: None,
        folder: None,
        before: None,
    });
    if !attached {
        client
            .request(Command::Activate { target })
            .map_err(|e| e.to_string())?;
        return Ok(None);
    }
    match client.request(Command::Snapshot) {
        Ok(Reply::Snapshot(snapshot)) => Ok(Some(Frontend {
            client,
            session,
            snapshot,
        })),
        result => {
            let _ = client.request(Command::DetachUi { session });
            Err(format!("后台快照不可用：{result:?}"))
        }
    }
}
fn compatible(reply: Reply) -> Result<(), String> {
    match reply {
        Reply::Hello { commands, .. }
            if [
                CommandName::AttachUi,
                CommandName::Snapshot,
                CommandName::Business,
                CommandName::JobResult,
                CommandName::SaveDraft,
            ]
            .iter()
            .all(|c| commands.contains(c)) =>
        {
            Ok(())
        }
        _ => Err("前后台协议/能力不兼容，请同时更新两个程序后重开".into()),
    }
}
