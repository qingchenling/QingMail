//! 桌面通知：索引事务候选、至多一次外部尝试、有界动作与标准会话 D-Bus。
use crate::storage::{StorageClient, receive};
use mail_protocol::OpenMailTarget;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zbus::{
    MatchRule,
    blocking::{Connection, MessageIterator, Proxy, connection::Builder},
    message::Type,
};

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
#[derive(Clone)]
struct Action {
    owner: String,
    token: String,
    target: OpenMailTarget,
    created: Instant,
}
#[derive(Default)]
struct Actions {
    items: HashMap<u32, Action>,
}
impl Actions {
    fn prune(&mut self, now: Instant) {
        self.items
            .retain(|_, a| now.saturating_duration_since(a.created) < Duration::from_secs(86400));
    }
    fn insert(&mut self, id: u32, action: Action) {
        self.prune(action.created);
        if self.items.len() >= 256
            && let Some(oldest) = self
                .items
                .iter()
                .min_by_key(|(_, a)| a.created)
                .map(|(id, _)| *id)
        {
            self.items.remove(&oldest);
        }
        self.items.insert(id, action);
    }
    fn invoke(&mut self, owner: &str, id: u32, token: &str) -> Option<OpenMailTarget> {
        self.prune(Instant::now());
        let a = self.items.get(&id)?;
        if a.owner != owner || (a.token != token && token != "default") {
            return None;
        }
        self.items.remove(&id).map(|a| a.target)
    }
}
/// Latest click wins; a burst cannot spawn an unbounded frontend or intent queue.
#[derive(Clone, Default)]
pub(crate) struct OpenIntents(Arc<Mutex<Option<OpenMailTarget>>>);
impl OpenIntents {
    fn push(&self, target: OpenMailTarget) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(target);
    }
    pub fn take(&self) -> Option<OpenMailTarget> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    #[cfg(test)]
    fn recv_timeout(&self, timeout: Duration) -> Result<OpenMailTarget, ()> {
        let end = Instant::now() + timeout;
        loop {
            if let Some(target) = self.take() {
                return Ok(target);
            }
            if Instant::now() >= end {
                return Err(());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
pub(crate) struct Notifications {
    pub status: Arc<Mutex<String>>,
    wake: SyncSender<()>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    connection: Arc<Mutex<Option<Connection>>>,
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn status(state: &Mutex<String>, value: impl Into<String>) {
    *state.lock().unwrap_or_else(|e| e.into_inner()) = value.into();
}
fn owner(conn: &Connection) -> zbus::Result<String> {
    Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )?
    .call("GetNameOwner", &(SERVICE,))
}
fn failure(error: &zbus::Error) -> String {
    let text = error.to_string();
    if text.contains("ServiceUnknown") || text.contains("NameHasNoOwner") {
        "系统没有通知服务；收信正常".into()
    } else if text.contains("AccessDenied") || text.contains("AuthFailed") {
        "系统拒绝通知权限；收信正常".into()
    } else {
        "通知调用失败或超时；收信正常，不重复提示旧信".into()
    }
}
/// Escaped even when body-markup is supported; headers are never interpreted as markup.
fn safe(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(limit)
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn content(batch: &mail_storage::NotificationBatch, preview: Option<&str>) -> (String, String) {
    let title = format!("QingMail · {}", safe(&batch.account_name, 80));
    let mut body = format!("{} 封新邮件", batch.count);
    if batch.preferences.show_sender_subject && batch.count == 1 {
        body.push_str(&format!(
            "\n{}\n{}",
            safe(&batch.sender, 160),
            safe(&batch.subject, 200)
        ));
    }
    if batch.preferences.show_preview
        && let Some(preview) = preview
    {
        body.push('\n');
        body.push_str(&safe(preview, 160));
    }
    (title, body)
}
fn cached_preview(storage: &StorageClient, location: &mail_core::MailUid) -> Option<String> {
    use std::io::Read;
    if let Some(raw) = receive(storage.message_body(location.clone()))
        .ok()
        .flatten()
    {
        return mail_content::parse(&raw).ok().map(|c| c.plain);
    }
    let structure = receive(storage.mime_structure(location.clone()))
        .ok()
        .flatten()?;
    for media in ["text/plain", "text/html"] {
        for part in structure
            .parts
            .iter()
            .filter(|p| p.media_type == media && !p.attachment)
        {
            let Some(file) = receive(storage.part_file(location.clone(), part.clone()))
                .ok()
                .flatten()
            else {
                continue;
            };
            let mut bytes = Vec::new();
            file.take(mail_content::MAX_BODY_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > mail_content::MAX_BODY_BYTES {
                continue;
            }
            let input = mail_content::SectionInput {
                media_type: &part.media_type,
                charset: part.charset.as_deref(),
                cid: None,
                bytes: &bytes,
            };
            if let Ok(content) = mail_content::parse_sections(&structure.headers, &[input]) {
                return Some(content.plain);
            }
        }
    }
    None
}
impl Notifications {
    pub fn start(
        storage: StorageClient,
        enabled: bool,
        open: OpenIntents,
    ) -> std::io::Result<Self> {
        Self::start_with_connection(storage, enabled, open, || {
            Builder::session().and_then(|b| b.method_timeout(Duration::from_secs(3)).build())
        })
    }
    fn start_with_connection(
        storage: StorageClient,
        enabled: bool,
        open: OpenIntents,
        connect: impl FnOnce() -> zbus::Result<Connection> + Send + 'static,
    ) -> std::io::Result<Self> {
        let (wake, receiver) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(if enabled {
            "正在检查通知服务".into()
        } else {
            "隔离验证未启用系统通知".into()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let connection = Arc::new(Mutex::new(None));
        let mut this = Self {
            status: state.clone(),
            wake: wake.clone(),
            stop: stop.clone(),
            thread: None,
            connection: connection.clone(),
        };
        storage
            .notification_wake(wake.clone())
            .map_err(std::io::Error::other)?;
        this.thread = Some(
            thread::Builder::new()
                .name("qingmail-notifications".into())
                .spawn(move || {
                    let actions = Arc::new(Mutex::new(Actions::default()));
                    let bus = if enabled {
                        match connect() {
                            Ok(conn) => {
                                *connection.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(conn.clone());
                                match listen(&conn, actions.clone(), state.clone(), open) {
                                    Ok(()) => Some(conn),
                                    Err(e) => {
                                        status(&state, failure(&e));
                                        None
                                    }
                                }
                            }
                            Err(_) => {
                                status(&state, "无法连接桌面会话总线；收信正常");
                                None
                            }
                        }
                    } else {
                        None
                    };
                    // Drain persisted candidates once at startup, then only on commit/deadline.
                    let mut due = Some(now());
                    while !stop.load(Ordering::Acquire) {
                        if let Some(at) = due {
                            let wait = Duration::from_secs(at.saturating_sub(now()).max(0) as u64);
                            if !wait.is_zero() {
                                let _ = receiver.recv_timeout(wait);
                            }
                        } else if receiver.recv().is_err() {
                            break;
                        }
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        let result = receive(storage.notifications(now()));
                        let (batches, next) = match result {
                            Ok(v) => v,
                            Err(_) => {
                                status(&state, "通知候选读取失败；收信正常，稍后同步时重试");
                                due = None;
                                continue;
                            }
                        };
                        due = next;
                        for batch in batches {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            let Some(conn) = &bus else {
                                continue;
                            };
                            let result = (|| -> zbus::Result<()> {
                                let owner = owner(conn)?;
                                let proxy = Proxy::new(conn, owner.as_str(), PATH, SERVICE)?;
                                let capabilities: Vec<String> =
                                    proxy.call("GetCapabilities", &())?;
                                let token = mail_protocol::transport::request_id()
                                    .map_err(|e| zbus::Error::InputOutput(Arc::new(e)))?;
                                let has_actions = capabilities.iter().any(|s| s == "actions");
                                let action_names = if has_actions {
                                    vec![
                                        "default".to_string(),
                                        "打开邮件".into(),
                                        token.clone(),
                                        "打开".into(),
                                    ]
                                } else {
                                    vec![]
                                };
                                // Cache-only preview; never invokes the network reading path.
                                let preview = if batch.preferences.show_preview {
                                    batch
                                        .target
                                        .message
                                        .as_ref()
                                        .and_then(|loc| cached_preview(&storage, loc))
                                } else {
                                    None
                                };
                                let (title, body) = content(&batch, preview.as_deref());
                                let hints: HashMap<&str, zbus::zvariant::Value<'_>> =
                                    HashMap::from([(
                                        "desktop-entry",
                                        zbus::zvariant::Value::from("org.qingmail.QingMail"),
                                    )]);
                                let id: u32 = proxy.call(
                                    "Notify",
                                    &(
                                        "QingMail",
                                        0u32,
                                        "mail-unread",
                                        title,
                                        body,
                                        action_names,
                                        hints,
                                        -1i32,
                                    ),
                                )?;
                                if has_actions {
                                    actions.lock().unwrap_or_else(|e| e.into_inner()).insert(
                                        id,
                                        Action {
                                            owner: owner.clone(),
                                            token,
                                            target: batch.target,
                                            created: Instant::now(),
                                        },
                                    );
                                }
                                status(&state, "通知服务已接受提醒（显示与勿扰由系统决定）");
                                Ok(())
                            })();
                            if let Err(e) = result {
                                status(&state, failure(&e));
                            }
                        }
                    }
                })?,
        );
        Ok(this)
    }
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
        if let Some(conn) = self
            .connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = conn.close();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Notifications {
    fn drop(&mut self) {
        self.stop();
    }
}
fn listen(
    conn: &Connection,
    actions: Arc<Mutex<Actions>>,
    state: Arc<Mutex<String>>,
    open: OpenIntents,
) -> zbus::Result<()> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(SERVICE)?
        .path(PATH)?
        .build();
    let iter = MessageIterator::for_match_rule(rule, conn, Some(64))?;
    let signal_conn = conn.clone();
    let signal_actions = actions.clone();
    thread::spawn(move || {
        for message in iter {
            let Ok(message) = message else {
                break;
            };
            let header = message.header();
            let Some(sender) = header.sender() else {
                continue;
            };
            // The current well-known owner and the saved unique owner must both match.
            if owner(&signal_conn).ok().as_deref() != Some(sender.as_str()) {
                continue;
            }
            let mut actions = signal_actions.lock().unwrap_or_else(|e| e.into_inner());
            match header.member().map(|m| m.as_str()) {
                Some("ActionInvoked") => {
                    if let Ok((id, token)) = message.body().deserialize::<(u32, String)>()
                        && let Some(target) = actions.invoke(sender.as_str(), id, &token)
                    {
                        open.push(target);
                    }
                }
                Some("NotificationClosed") => {
                    if let Ok((id, _)) = message.body().deserialize::<(u32, u32)>() {
                        actions.items.remove(&id);
                    }
                }
                _ => (),
            }
        }
    });
    match owner(conn) {
        Ok(_) => status(&state, "通知服务可用"),
        Err(e) => status(&state, failure(&e)),
    };
    let proxy = Proxy::new(conn, SERVICE, PATH, SERVICE)?;
    let changes = proxy.receive_owner_changed()?;
    let observed = conn.clone();
    thread::spawn(move || {
        for change in changes {
            actions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .items
                .clear();
            if change.is_some() {
                status(&state, "通知服务可用");
            } else {
                status(&state, "系统没有通知服务；收信正常");
            }
        }
        let _ = observed;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_core::*;
    use mail_protocol::{Command, DesktopPreferences, Reply};
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command as Process, Stdio},
    };
    #[derive(Clone, Debug)]
    struct Captured {
        summary: String,
        body: String,
        actions: Vec<String>,
    }
    struct Mock {
        calls: Arc<Mutex<Vec<Captured>>>,
        fail: bool,
        actions: bool,
    }
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Mock {
        fn get_capabilities(&self) -> Vec<String> {
            if self.actions {
                vec!["actions".into(), "body-markup".into()]
            } else {
                vec![]
            }
        }
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            _app_name: String,
            _replaces_id: u32,
            _app_icon: String,
            summary: String,
            body: String,
            actions: Vec<String>,
            _hints: HashMap<String, zbus::zvariant::OwnedValue>,
            _expire_timeout: i32,
        ) -> zbus::fdo::Result<u32> {
            self.calls.lock().unwrap().push(Captured {
                summary,
                body,
                actions,
            });
            if self.fail {
                Err(zbus::fdo::Error::Failed(
                    "simulated unconfirmed result".into(),
                ))
            } else {
                Ok(7)
            }
        }
    }
    struct Bus {
        child: Child,
        address: String,
    }
    impl Bus {
        fn new() -> Self {
            let mut child = Process::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut address = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();
            assert!(!address.trim().is_empty(), "private D-Bus did not start");
            Self {
                child,
                address: address.trim().into(),
            }
        }
        fn connect(&self) -> Connection {
            Builder::address(self.address.as_str())
                .unwrap()
                .method_timeout(Duration::from_secs(3))
                .build()
                .unwrap()
        }
    }
    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    fn settings() -> AccountSettings {
        let server = ServerSettings {
            authentication: Default::default(),
            host: "test.invalid".into(),
            port: 993,
            username: "test".into(),
            security: ConnectionSecurity::Tls,
        };
        AccountSettings {
            display_name: "test account".into(),
            address: "test@test.invalid".into(),
            imap: server.clone(),
            smtp: server,
        }
    }
    fn batch(uids: &[u32]) -> FolderBatch {
        FolderBatch {
            remote_name: "INBOX".into(),
            uidvalidity: 1,
            since: 0,
            has_more: false,
            complete: true,
            kind: BatchKind::Recent,
            messages: uids
                .iter()
                .map(|uid| {
                    (
                        *uid,
                        MessageEnvelope {
                            subject: "<b>private subject</b>".into(),
                            sender: "private sender".into(),
                            received_at: 1,
                        },
                    )
                })
                .collect(),
            flags: Default::default(),
        }
    }
    fn seed() -> (tempfile::TempDir, AccountId) {
        let root = tempfile::tempdir().unwrap();
        let mut store = mail_storage::Store::open(&root.path().join("data")).unwrap();
        let account = store.create_account(&settings()).unwrap();
        store
            .replace_remote_folders(
                &account,
                &[RemoteFolder {
                    name: "INBOX".into(),
                    display_name: "收件箱".into(),
                    delimiter: None,
                    selectable: true,
                }],
                now(),
            )
            .unwrap();
        store
            .apply_folder_batch(&account, &batch(&[1]), now() - 10)
            .unwrap();
        (root, account)
    }
    fn wait_for(mut predicate: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < end, "notification deadline exceeded");
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn notification_privacy_actions_are_bounded_and_owner_bound() {
        let intents = OpenIntents::default();
        intents.push(OpenMailTarget {
            account: AccountId("a".repeat(32)),
            message: None,
        });
        intents.push(OpenMailTarget {
            account: AccountId("b".repeat(32)),
            message: None,
        });
        assert_eq!(intents.take().unwrap().account.0, "b".repeat(32));
        assert!(intents.take().is_none());
        let mut batch = mail_storage::NotificationBatch {
            target: OpenMailTarget {
                account: AccountId("a".repeat(32)),
                message: None,
            },
            account_name: "账号".into(),
            count: 1,
            subject: "<b>secret</b>".into(),
            sender: "sender".into(),
            preferences: DesktopPreferences::default(),
        };
        let (_, body) = content(&batch, Some("preview"));
        assert!(!body.contains("secret") && !body.contains("sender") && !body.contains("preview"));
        batch.preferences.show_sender_subject = true;
        batch.preferences.show_preview = true;
        let (_, body) = content(&batch, Some("<script>"));
        assert!(body.contains("&lt;b&gt;secret"));
        assert!(!body.contains("<script>"));
        let mut actions = Actions::default();
        for id in 0..260 {
            actions.insert(
                id,
                Action {
                    owner: ":1.1".into(),
                    token: "opaque".into(),
                    target: batch.target.clone(),
                    created: Instant::now(),
                },
            );
        }
        assert_eq!(actions.items.len(), 256);
        assert!(actions.invoke(":1.2", 259, "opaque").is_none());
        assert!(actions.invoke(":1.1", 259, "forged").is_none());
        assert!(actions.invoke(":1.1", 259, "opaque").is_some());
        assert!(actions.invoke(":1.1", 259, "opaque").is_none());
        actions.items.get_mut(&258).unwrap().created = Instant::now() - Duration::from_secs(86401);
        assert!(actions.invoke(":1.1", 258, "default").is_none());
    }
    #[test]
    fn notification_private_bus_delivery_click_and_spoof_rejection() {
        let bus = Bus::new();
        let calls = Arc::new(Mutex::new(vec![]));
        let service = Builder::address(bus.address.as_str())
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .serve_at(
                PATH,
                Mock {
                    calls: calls.clone(),
                    fail: false,
                    actions: true,
                },
            )
            .unwrap()
            .build()
            .unwrap();
        let (root, account) = seed();
        let mut storage =
            crate::storage::StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let open = OpenIntents::default();
        let requests = open.clone();
        let conn = bus.connect();
        let mut worker =
            Notifications::start_with_connection(storage.client(), true, open, move || Ok(conn))
                .unwrap();
        receive(
            storage
                .client()
                .publish_folder(account.clone(), batch(&[1, 2]), now() - 3),
        )
        .unwrap();
        wait_for(|| !calls.lock().unwrap().is_empty());
        wait_for(|| worker.status.lock().unwrap().contains("接受"));
        let notification = calls.lock().unwrap()[0].clone();
        assert_eq!(notification.summary, "QingMail · test account");
        assert_eq!(notification.body, "1 封新邮件");
        assert_eq!(notification.actions[0], "default");
        let attacker = bus.connect();
        attacker
            .emit_signal(
                None::<&str>,
                PATH,
                SERVICE,
                "ActionInvoked",
                &(7u32, "default"),
            )
            .unwrap();
        assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
        service
            .emit_signal(
                None::<&str>,
                PATH,
                SERVICE,
                "ActionInvoked",
                &(7u32, notification.actions[2].clone()),
            )
            .unwrap();
        let target = requests.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(target.account, account);
        assert_eq!(target.message.unwrap().uid, 2);
        receive(
            storage
                .client()
                .publish_folder(account, batch(&[1, 2]), now()),
        )
        .unwrap();
        worker.stop();
        storage.stop();
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
    #[test]
    fn notification_unconfirmed_result_is_not_replayed_after_restart() {
        let bus = Bus::new();
        let calls = Arc::new(Mutex::new(vec![]));
        let _service = Builder::address(bus.address.as_str())
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .serve_at(
                PATH,
                Mock {
                    calls: calls.clone(),
                    fail: true,
                    actions: false,
                },
            )
            .unwrap()
            .build()
            .unwrap();
        let (root, account) = seed();
        for round in 0..2 {
            let mut storage =
                crate::storage::StorageWorker::start(Some(root.path().join("data"))).unwrap();
            let open = OpenIntents::default();
            let conn = bus.connect();
            let mut worker =
                Notifications::start_with_connection(storage.client(), true, open, move || {
                    Ok(conn)
                })
                .unwrap();
            if round == 0 {
                receive(storage.client().publish_folder(
                    account.clone(),
                    batch(&[1, 2]),
                    now() - 3,
                ))
                .unwrap();
                wait_for(|| !calls.lock().unwrap().is_empty());
                wait_for(|| worker.status.lock().unwrap().contains("失败"));
            } else {
                receive(
                    storage
                        .client()
                        .publish_folder(account.clone(), batch(&[1, 2]), now()),
                )
                .unwrap();
                thread::sleep(Duration::from_millis(100));
            }
            worker.stop();
            storage.stop();
        }
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].actions.is_empty());
    }
    #[test]
    fn notification_service_missing_is_visible_and_does_not_block_settings() {
        let bus = Bus::new();
        let (root, account) = seed();
        let mut storage =
            crate::storage::StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let open = OpenIntents::default();
        let conn = bus.connect();
        let mut worker =
            Notifications::start_with_connection(storage.client(), true, open, move || Ok(conn))
                .unwrap();
        receive(
            storage
                .client()
                .publish_folder(account, batch(&[1, 2]), now() - 3),
        )
        .unwrap();
        wait_for(|| worker.status.lock().unwrap().contains("没有通知服务"));
        assert!(matches!(
            storage.client().control(Command::DesktopSettings).unwrap(),
            Reply::DesktopSettings(_)
        ));
        worker.stop();
        storage.stop();
    }
    #[test]
    fn notification_closed_and_service_replacement_invalidate_actions() {
        let bus = Bus::new();
        let calls = Arc::new(Mutex::new(vec![]));
        let service = Builder::address(bus.address.as_str())
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .serve_at(
                PATH,
                Mock {
                    calls: calls.clone(),
                    fail: false,
                    actions: true,
                },
            )
            .unwrap()
            .build()
            .unwrap();
        let (root, account) = seed();
        let mut storage =
            crate::storage::StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let open = OpenIntents::default();
        let requests = open.clone();
        let conn = bus.connect();
        let mut worker =
            Notifications::start_with_connection(storage.client(), true, open, move || Ok(conn))
                .unwrap();
        receive(
            storage
                .client()
                .publish_folder(account, batch(&[1, 2]), now() - 3),
        )
        .unwrap();
        wait_for(|| worker.status.lock().unwrap().contains("接受"));
        service
            .emit_signal(
                None::<&str>,
                PATH,
                SERVICE,
                "NotificationClosed",
                &(7u32, 2u32),
            )
            .unwrap();
        service
            .emit_signal(
                None::<&str>,
                PATH,
                SERVICE,
                "ActionInvoked",
                &(7u32, "default"),
            )
            .unwrap();
        assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
        service.release_name(SERVICE).unwrap();
        wait_for(|| worker.status.lock().unwrap().contains("没有通知服务"));
        let replacement = Builder::address(bus.address.as_str())
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .serve_at(
                PATH,
                Mock {
                    calls: calls.clone(),
                    fail: false,
                    actions: true,
                },
            )
            .unwrap()
            .build()
            .unwrap();
        wait_for(|| worker.status.lock().unwrap().contains("可用"));
        replacement
            .emit_signal(
                None::<&str>,
                PATH,
                SERVICE,
                "ActionInvoked",
                &(7u32, "default"),
            )
            .unwrap();
        assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(calls.lock().unwrap().len(), 1);
        worker.stop();
        storage.stop();
    }
    #[test]
    fn notification_preview_reads_only_existing_body_or_part_cache() {
        use std::io::Write;
        let (root, account) = seed();
        let mut store = mail_storage::Store::open(&root.path().join("data")).unwrap();
        let location = store
            .folder_cache(&account, "INBOX", None)
            .unwrap()
            .unwrap()
            .messages[0]
            .location
            .clone();
        let part = MimePart {
            section: "1".into(),
            media_type: "text/html".into(),
            charset: Some("utf-8".into()),
            encoding: "8bit".into(),
            encoded_size: 100,
            name: None,
            cid: None,
            attachment: false,
        };
        store
            .save_mime_structure(
                &location,
                &MimeStructure {
                    total_size: 100,
                    headers: b"Subject: preview\r\n\r\n".to_vec(),
                    parts: vec![part.clone()],
                },
            )
            .unwrap();
        drop(store);
        let mut storage =
            crate::storage::StorageWorker::start(Some(root.path().join("data"))).unwrap();
        assert!(cached_preview(&storage.client(), &location).is_none());
        storage.stop();
        let mut store = mail_storage::Store::open(&root.path().join("data")).unwrap();
        let mut file = store.part_temporary().unwrap();
        file.write_all(b"<p>cached preview</p><img src='https://tracker.invalid/a'>")
            .unwrap();
        drop(store.save_part(&location, &part, file).unwrap());
        drop(store);
        let mut storage =
            crate::storage::StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let preview = cached_preview(&storage.client(), &location).unwrap();
        assert!(preview.contains("cached preview"));
        storage.stop();
    }
}
