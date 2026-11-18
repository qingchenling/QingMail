//! User-owned XDG entry, coordinated after a durable intent on the storage thread.
use mail_protocol::{
    AutostartSettings, Command, ErrorCode, MutationStatus, Reply, Request, Response, RpcError,
};
use mail_storage::{AutostartIntent, MutationClaim, Store};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
const NAME: &str = "org.qingmail.QingMail.desktop";
const LIMIT: u64 = 8192;
#[derive(Clone)]
pub(crate) struct Controller {
    directory: PathBuf,
    executable: PathBuf,
}
impl Controller {
    #[cfg(test)]
    pub(crate) fn fixture(directory: PathBuf, executable: PathBuf) -> Self {
        Self {
            directory,
            executable,
        }
    }
    pub fn production() -> io::Result<Self> {
        let config = mail_storage::AppPaths::discover()
            .map_err(io::Error::other)?
            .config;
        Ok(Self {
            directory: config
                .parent()
                .ok_or_else(|| io::Error::other("配置目录无效"))?
                .join("autostart"),
            executable: std::env::current_exe()?,
        })
    }
    fn executable(&self) -> io::Result<String> {
        if !self.executable.is_absolute()
            || self
                .executable
                .file_name()
                .is_none_or(|n| n != "mail-daemon")
        {
            return Err(io::Error::other("自启动须使用正式 mail-daemon 入口"));
        }
        let path = self.executable.canonicalize()?;
        let m = path.metadata()?;
        if !m.is_file() || m.permissions().mode() & 0o111 == 0 {
            return Err(io::Error::other("后台路径不是可执行文件"));
        }
        let value = path
            .to_str()
            .ok_or_else(|| io::Error::other("后台路径须为有效 UTF-8"))?;
        // Desktop Entry cannot represent control characters in a safe executable token.
        if value.chars().any(char::is_control) {
            return Err(io::Error::other("后台路径含控制字符"));
        }
        Ok(value.to_owned())
    }
    fn directory(&self, create: bool) -> io::Result<File> {
        if create {
            fs::create_dir_all(&self.directory)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(
                rustix::fs::OFlags::DIRECTORY.bits() as i32
                    | rustix::fs::OFlags::NOFOLLOW.bits() as i32,
            )
            .open(&self.directory)?;
        let m = file.metadata()?;
        if m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o022 != 0 {
            return Err(io::Error::other(
                "自启动目录须属于当前用户且不可由其他用户写入",
            ));
        }
        Ok(file)
    }
    fn read(directory: &File) -> io::Result<Option<String>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(fdpath(directory).join(NAME))
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let m = file.metadata()?;
        if !m.is_file()
            || m.uid() != rustix::process::geteuid().as_raw()
            || m.mode() & 0o022 != 0
            || m.nlink() != 1
            || m.len() > LIMIT
        {
            return Err(io::Error::other("同名启动项不是受管普通文件，已保留"));
        }
        let mut text = String::new();
        file.take(LIMIT + 1).read_to_string(&mut text)?;
        if text.len() as u64 > LIMIT || managed_path(&text).is_none() {
            return Err(io::Error::other(
                "存在同名非 QingMail 受管启动项，已保留；请自行检查",
            ));
        }
        Ok(Some(text))
    }
    fn apply(&self, intent: &AutostartIntent) -> io::Result<()> {
        if intent.desired && self.executable()? != intent.executable {
            return Err(io::Error::other(
                "后台位置已变化，请重新开启自启动以确认新路径",
            ));
        }
        let directory = match self.directory(intent.desired) {
            Ok(d) => d,
            Err(e) if !intent.desired && e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let original = Self::read(&directory)?;
        let path = fdpath(&directory);
        if !intent.desired {
            if original.is_some() {
                fs::remove_file(path.join(NAME))?;
                directory.sync_all()?;
            }
            return Ok(());
        }
        let text = entry(&intent.executable);
        if original.as_deref() == Some(&text) {
            return Ok(());
        }
        let temporary = format!(".{NAME}.{}.tmp", mail_protocol::transport::request_id()?);
        let temp = path.join(&temporary);
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            if Self::read(&directory)? != original {
                return Err(io::Error::other("启动项在保存时发生变化，请刷新后重试"));
            }
            if original.is_none() {
                rustix::fs::renameat_with(
                    &directory,
                    &temporary,
                    &directory,
                    NAME,
                    rustix::fs::RenameFlags::NOREPLACE,
                )?;
            } else {
                fs::rename(&temp, path.join(NAME))?;
            }
            directory.sync_all()
        })();
        let _ = fs::remove_file(&temp);
        result
    }
    fn inspect(&self, intent: &AutostartIntent) -> (bool, String) {
        let result = (|| {
            let directory = self.directory(false)?;
            Self::read(&directory)
        })();
        match result {
            Ok(Some(text)) => {
                let path = managed_path(&text).unwrap();
                let valid = Path::new(&path)
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0);
                if !valid {
                    return (
                        false,
                        "启动项中的后台路径已失效，请重新开启以确认当前位置".into(),
                    );
                }
                if self.executable().is_ok_and(|p| p == path)
                    && intent.desired
                    && path == intent.executable
                {
                    (true, "已开启登录启动项：由桌面会话执行，仅启动后台".into())
                } else {
                    (
                        true,
                        "受管启动项与当前设置或后台位置不一致；请明确重新开启或关闭".into(),
                    )
                }
            }
            Ok(None) => (
                false,
                if intent.desired {
                    "启动项缺失，请重新开启"
                } else {
                    "已关闭登录后后台启动"
                }
                .into(),
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (
                false,
                if intent.desired {
                    "启动项缺失，请重新开启"
                } else {
                    "已关闭登录后后台启动"
                }
                .into(),
            ),
            Err(e) => (false, format!("无法核对自启动状态：{e}")),
        }
    }
    fn reconcile(&self, store: &mut Store) -> Result<AutostartSettings, RpcError> {
        let intent = store.autostart_intent().map_err(storage_error)?;
        let mut failure = None;
        if intent.pending {
            match self.apply(&intent) {
                Ok(()) => store
                    .finish_autostart_intent(intent.revision)
                    .map_err(storage_error)?,
                Err(e) => failure = Some(e.to_string()),
            }
        }
        let intent = store.autostart_intent().map_err(storage_error)?;
        let (enabled, mut status) = self.inspect(&intent);
        if let Some(e) = failure {
            status = format!("设置未完成：{e}；请刷新核对实际状态并重试");
        }
        Ok(AutostartSettings {
            revision: intent.revision,
            desired: intent.desired,
            pending: intent.pending,
            enabled,
            status,
        })
    }
}
fn fdpath(file: &File) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
// Double escaping is required: Exec token quoting, followed by Desktop Entry string escaping.
fn string(value: &str) -> String {
    value.replace('\\', "\\\\").replace(' ', "\\s")
}
fn entry(path: &str) -> String {
    let mut exec = String::from("\"");
    for c in path.chars() {
        match c {
            '\\' | '"' | '`' | '$' => {
                exec.push('\\');
                exec.push(c)
            }
            '%' => exec.push_str("%%"),
            _ => exec.push(c),
        }
    }
    exec.push('"');
    format!(
        "[Desktop Entry]\nType=Application\nName=QingMail Background\nComment=Start QingMail background only\nExec={} --production\nTryExec={}\nTerminal=false\nX-QingMail-Managed=1\n",
        string(&exec),
        string(path)
    )
}
fn managed_path(text: &str) -> Option<String> {
    let encoded = text.lines().find_map(|l| l.strip_prefix("TryExec="))?;
    let mut path = String::new();
    let mut chars = encoded.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                's' => path.push(' '),
                '\\' => path.push('\\'),
                _ => return None,
            }
        } else {
            path.push(c)
        }
    }
    if !Path::new(&path).is_absolute() || path.chars().any(char::is_control) || entry(&path) != text
    {
        return None;
    }
    Some(path)
}
fn error(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}
fn storage_error(e: mail_storage::Error) -> RpcError {
    error(
        if matches!(e, mail_storage::Error::Conflict) {
            ErrorCode::Conflict
        } else {
            ErrorCode::Storage
        },
        e.to_string(),
    )
}
pub(crate) fn dispatch(
    store: &mut Store,
    controller: Option<&Controller>,
    request: Request,
) -> Response {
    let result = (|| {
        request
            .validate()
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        let controller = controller
            .ok_or_else(|| error(ErrorCode::Unsupported, "隔离/测试后台不允许修改登录启动项"))?;
        if matches!(request.command, Command::AutostartSettings) {
            return controller.reconcile(store).map(Reply::AutostartSettings);
        }
        let Command::SetAutostart { enabled, revision } = request.command else {
            return Err(error(ErrorCode::Unsupported, "非自启动请求"));
        };
        match store.claim_mutation(&request).map_err(storage_error)? {
            MutationClaim::Existing(MutationStatus::Finished(result)) => return *result,
            MutationClaim::Existing(_) => {
                return Err(error(
                    ErrorCode::Uncertain,
                    "请刷新核对持久意图，不会重放设置请求",
                ));
            }
            MutationClaim::Acquired => (),
        }
        let result = (|| {
            let old = store.autostart_intent().map_err(storage_error)?;
            let executable = if enabled {
                controller
                    .executable()
                    .map_err(|e| error(ErrorCode::Storage, e.to_string()))?
            } else {
                old.executable
            };
            store
                .set_autostart_intent(revision, enabled, &executable)
                .map_err(storage_error)?;
            controller.reconcile(store).map(Reply::AutostartSettings)
        })();
        store
            .finish_mutation(&request, &result)
            .map_err(storage_error)?;
        result
    })();
    Response {
        version: mail_protocol::PROTOCOL_VERSION,
        id: request.id,
        result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    fn setup() -> (tempfile::TempDir, Store, Controller) {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("mail-daemon");
        fs::write(&executable, b"fixture, never executed").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let controller = Controller {
            directory: root.path().join("config/autostart"),
            executable,
        };
        let store = Store::open(&root.path().join("data")).unwrap();
        (root, store, controller)
    }
    fn request(enabled: bool, revision: u64) -> Request {
        Request::new(
            mail_protocol::transport::request_id().unwrap(),
            Command::SetAutostart { enabled, revision },
        )
    }
    fn settings(response: Response) -> AutostartSettings {
        match response.result.unwrap() {
            Reply::AutostartSettings(v) => v,
            _ => panic!("wrong reply"),
        }
    }
    #[test]
    fn autostart_default_enable_disable_replay_and_revision_conflict() {
        let (_root, mut store, c) = setup();
        let initial = c.reconcile(&mut store).unwrap();
        assert!(!initial.enabled && !initial.desired && !initial.pending);
        assert!(!c.directory.exists());
        let req = request(true, 0);
        let enabled = settings(dispatch(&mut store, Some(&c), req.clone()));
        assert!(enabled.enabled && enabled.desired && !enabled.pending);
        let text = fs::read_to_string(c.directory.join(NAME)).unwrap();
        assert!(text.contains(" --production\n"));
        assert!(!text.contains("mail-ui"));
        assert_eq!(
            settings(dispatch(&mut store, Some(&c), req.clone())),
            enabled
        );
        assert_eq!(
            dispatch(&mut store, Some(&c), request(false, 0))
                .result
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        let disabled = settings(dispatch(&mut store, Some(&c), request(false, 1)));
        assert!(!disabled.enabled && !disabled.pending && !disabled.desired);
        // A lost reply may be queried even after a later setting changed actual state.
        assert!(matches!(
            store.mutation_result(&req.id).unwrap(),
            MutationStatus::Finished(_)
        ));
        assert!(!c.directory.join(NAME).exists());
    }
    #[test]
    fn autostart_conflicts_links_and_write_failure_preserve_pending_intent() {
        let (_root, mut store, c) = setup();
        fs::create_dir_all(&c.directory).unwrap();
        let path = c.directory.join(NAME);
        fs::write(&path, "[Desktop Entry]\nName=User\n").unwrap();
        let failed = settings(dispatch(&mut store, Some(&c), request(true, 0)));
        assert!(failed.pending && !failed.enabled && failed.status.contains("非 QingMail"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[Desktop Entry]\nName=User\n"
        );
        let failed = settings(dispatch(&mut store, Some(&c), request(false, 1)));
        assert!(failed.pending);
        fs::remove_file(&path).unwrap();
        symlink(&c.executable, &path).unwrap();
        assert!(c.reconcile(&mut store).unwrap().pending);
        assert!(path.is_symlink());
        fs::remove_file(&path).unwrap();
        fs::set_permissions(&c.directory, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(settings(dispatch(&mut store, Some(&c), request(true, 2))).pending);
        fs::set_permissions(&c.directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(c.reconcile(&mut store).unwrap().enabled);
    }
    #[test]
    fn autostart_pending_restart_recovery_and_moved_binary_require_explicit_confirmation() {
        let (root, mut store, c) = setup();
        let executable = c.executable().unwrap();
        store.set_autostart_intent(0, true, &executable).unwrap();
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert!(c.reconcile(&mut store).unwrap().enabled);
        let moved_dir = root.path().join("moved");
        fs::create_dir(&moved_dir).unwrap();
        let moved = Controller {
            directory: c.directory.clone(),
            executable: moved_dir.join("mail-daemon"),
        };
        fs::rename(&c.executable, &moved.executable).unwrap();
        let state = moved.reconcile(&mut store).unwrap();
        assert!(!state.enabled && state.status.contains("失效"));
        // No pending operation: refresh never silently points the entry at the new binary.
        assert_eq!(
            managed_path(&fs::read_to_string(c.directory.join(NAME)).unwrap()).unwrap(),
            executable
        );
        let state = settings(dispatch(&mut store, Some(&moved), request(true, 1)));
        assert!(state.enabled);
        // Closing the daemon retains the entry and next-login preference.
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert!(moved.reconcile(&mut store).unwrap().enabled);
        store
            .set_autostart_intent(2, false, &moved.executable().unwrap())
            .unwrap();
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert!(!moved.reconcile(&mut store).unwrap().enabled);
        assert!(!moved.directory.join(NAME).exists());
    }
    #[test]
    fn autostart_desktop_encoding_and_isolated_refusal() {
        let (root, mut store, c) = setup();
        for path in ["/opt/Qing Mail/mail-daemon", "/opt/a\"$`%\\b/mail-daemon"] {
            let text = entry(path);
            assert_eq!(managed_path(&text).as_deref(), Some(path));
            assert!(text.contains("X-QingMail-Managed=1"));
        }
        assert!(managed_path("[Desktop Entry]\nX-QingMail-Managed=1\nTryExec=/bin/sh\n").is_none());
        assert_eq!(
            dispatch(&mut store, None, request(true, 0))
                .result
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
        assert_eq!(store.autostart_intent().unwrap().revision, 0);
        let invalid = Controller {
            directory: c.directory.clone(),
            executable: root.path().join("missing"),
        };
        assert!(
            dispatch(&mut store, Some(&invalid), request(true, 0))
                .result
                .is_err()
        );
        assert_eq!(store.autostart_intent().unwrap().revision, 0);
        assert!(!c.directory.exists());
    }
    #[test]
    fn autostart_entry_standard_validator_when_available() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            "/usr/bin/mail-daemon",
            "/opt/Qing Mail/a\"$`%\\b/mail-daemon",
        ] {
            let file = root.path().join(NAME);
            fs::write(&file, entry(path)).unwrap();
            match std::process::Command::new("desktop-file-validate")
                .arg(&file)
                .output()
            {
                Ok(output) => assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stdout)
                ),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    eprintln!(
                        "desktop-file-validate unavailable; skipped optional external validation"
                    );
                    return;
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
    #[test]
    fn autostart_external_edits_are_not_repaired_on_refresh() {
        let (_root, mut store, c) = setup();
        settings(dispatch(&mut store, Some(&c), request(true, 0)));
        fs::remove_file(c.directory.join(NAME)).unwrap();
        let state = c.reconcile(&mut store).unwrap();
        assert!(!state.enabled && !state.pending && state.desired);
        assert!(!c.directory.join(NAME).exists());
    }
}
