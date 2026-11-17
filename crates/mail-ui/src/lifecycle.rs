//! 前台生命周期：正式入口协调 daemon 会话及草稿保存；隔离旧探针保留本地适配。
use crate::{
    composer::{self, Composer},
    reader,
};
use gpui_kit::{AnyWindowHandle, App, Global, WeakEntity, prelude::*};
use mail_daemon::{Background, Command, ControlServer, Instance, Tray};
use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    time::Duration,
};

pub(super) enum AccountBackend {
    Local(Arc<mail_daemon::account_gateway::AccountGateway>),
    Remote(
        mail_protocol::transport::Client,
        Arc<std::sync::atomic::AtomicBool>,
    ),
}
impl AccountBackend {
    pub fn saving(&self) -> bool {
        match self {
            Self::Local(backend) => backend.saving(),
            Self::Remote(_, active) => active.load(std::sync::atomic::Ordering::Acquire),
        }
    }
}
struct Editor {
    window: AnyWindowHandle,
    entity: WeakEntity<Composer>,
}
pub(super) struct Lifecycle {
    main: Option<AnyWindowHandle>,
    reader: Option<AnyWindowHandle>,
    mailbox: Option<AnyWindowHandle>,
    send_tasks: Option<AnyWindowHandle>,
    settings: Option<AnyWindowHandle>,
    accounts: Option<(AnyWindowHandle, WeakEntity<crate::accounts::Accounts>)>,
    editors: Vec<Editor>,
    worker: Option<Background>,
    account_backend: Arc<AccountBackend>,
    pending_account_save: Option<crate::accounts::PendingSave>,
    account_requests: Arc<mail_storage::ClientRequests>,
    storage: mail_daemon::storage::StorageWorker,
    sync: Option<mail_daemon::SyncEngine>,
    remote: Option<crate::remote::Remote>,
    shutdown: bool,
    finishing: bool,
    sending: mail_daemon::sending::SendCoordinator,
    _control: Option<ControlServer>,
    tray: Option<Tray>,
    queue: SyncSender<Command>,
    receiver: Receiver<Command>,
    status: Arc<Mutex<String>>,
    tray_status: String,
    quitting: bool,
    error: Option<String>,
    probe_step: u8,
}
impl Global for Lifecycle {}

pub(super) fn start(
    instance: Option<Instance>,
    frontend: Option<mail_daemon::launcher::Frontend>,
    cx: &mut App,
) -> Result<(), String> {
    let (queue, receiver) = mpsc::sync_channel(32);
    let status = Arc::new(Mutex::new(format!("pid={} starting", std::process::id())));
    let control = instance
        .map(|instance| ControlServer::start(instance, queue.clone(), status.clone()))
        .transpose()
        .map_err(|e| e.to_string())?;
    let storage_path = std::env::var_os("QINGMAIL_P0_DRAFT_ROOT").map(|p| {
        std::path::PathBuf::from(p).join(
            if std::env::args()
                .any(|a| matches!(a.as_str(), "--editor-probe" | "--editor-style-demo"))
            {
                "p2-editor-probe"
            } else {
                "p2-data"
            },
        )
    });
    let remote = frontend.map(crate::remote::Remote::start).transpose()?;
    let request_path = if remote.is_some() && std::env::var_os("QINGMAIL_IPC_ROOT").is_some() {
        std::path::PathBuf::from(std::env::var_os("QINGMAIL_IPC_ROOT").unwrap()).join("ui-requests")
    } else {
        match &storage_path {
            Some(path) => path.join("client-requests"),
            None => mail_storage::AppPaths::discover()
                .map_err(|e| e.to_string())?
                .config
                .join("client-requests"),
        }
    };
    let account_requests =
        Arc::new(mail_storage::ClientRequests::open(&request_path).map_err(|e| e.to_string())?);
    let pending_account_save = account_requests.load().map_err(|e| e.to_string())?;
    let probe = std::env::args().any(|arg| arg.ends_with("-probe"));
    let (storage, worker, sync, account_backend) = if let Some(remote) = &remote {
        let storage = mail_daemon::storage::StorageWorker::connect(remote.client.clone())
            .map_err(|e| e.to_string())?;
        (
            storage,
            None,
            None,
            Arc::new(AccountBackend::Remote(
                remote.client.clone(),
                remote.saving.clone(),
            )),
        )
    } else {
        let storage =
            mail_daemon::storage::StorageWorker::start(storage_path).map_err(|e| e.to_string())?;
        let worker = Some(Background::start().map_err(|e| e.to_string())?);
        let sync = Some(mail_daemon::SyncEngine::start_enabled(
            storage.client(),
            !probe,
        ));
        let backend = Arc::new(AccountBackend::Local(Arc::new(
            mail_daemon::account_gateway::AccountGateway::start(
                !probe,
                storage.client(),
                Default::default(),
            )
            .map_err(|e| e.to_string())?,
        )));
        (storage, worker, sync, backend)
    };
    let tray_enabled = remote.is_none()
        && !std::env::args().any(|arg| {
            matches!(
                arg.as_str(),
                "--no-tray" | "--editor-probe" | "--editor-style-demo"
            )
        })
        && std::env::var_os("QINGMAIL_P0_RUNTIME_ROOT").is_none();
    cx.set_global(Lifecycle {
        main: None,
        reader: None,
        mailbox: None,
        send_tasks: None,
        settings: None,
        accounts: None,
        editors: vec![],
        worker,
        account_backend,
        pending_account_save,
        account_requests,
        storage,
        sync,
        remote,
        shutdown: false,
        finishing: false,
        sending: Default::default(),
        _control: control,
        tray: None,
        queue: queue.clone(),
        receiver,
        status,
        tray_status: if tray_enabled {
            "正在检查托盘宿主".into()
        } else {
            "未启用托盘；再次启动可重开窗口，--quit 可退出".into()
        },
        quitting: false,
        error: None,
        probe_step: 0,
    });
    if tray_enabled {
        let task = cx
            .background_executor()
            .spawn(async move { Tray::start(queue) });
        cx.spawn(async move |cx| {
            let result = task.await;
            cx.update(|cx| {
                cx.update_global::<Lifecycle, _>(|state, _| match result {
                    Ok(tray) => {
                        state.tray = Some(tray);
                        state.tray_status = "托盘已注册，提供打开/保存并退出入口".into();
                    }
                    Err(e) => {
                        state.tray_status =
                            format!("托盘不可用：{e}；再次启动可重开，--quit 可退出")
                    }
                })
            });
        })
        .detach();
    }
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            cx.update(tick);
        }
    })
    .detach();
    Ok(())
}

fn live(window: AnyWindowHandle, cx: &App) -> bool {
    cx.windows().contains(&window)
}
fn activate(window: AnyWindowHandle, cx: &mut App) -> bool {
    window
        .update(cx, |_, window, cx| {
            window.activate_window();
            cx.activate(true);
        })
        .is_ok()
}
/// 正式入口是邮件主界面；P0 合成首页只保留为显式 `--home-demo` 诊断窗口。
pub(super) fn open_main(cx: &mut App) {
    crate::mailbox::open(cx);
}
pub(super) fn reuse_editor(account: Option<&mail_core::AccountId>, cx: &mut App) -> bool {
    let existing = cx
        .global::<Lifecycle>()
        .editors
        .iter()
        .find(|e| {
            live(e.window, cx)
                && e.entity
                    .upgrade()
                    .is_some_and(|entity| entity.read(cx).account_id() == account)
        })
        .map(|e| (e.window, e.entity.clone()));
    if let Some((window, entity)) = existing {
        let _ = entity.update(cx, |this, cx| this.explain_scope(account, cx));
        return activate(window, cx);
    }
    false
}
pub(super) fn register_editor(window: AnyWindowHandle, entity: WeakEntity<Composer>, cx: &mut App) {
    cx.global_mut::<Lifecycle>()
        .editors
        .push(Editor { window, entity });
}
pub(super) fn reuse_reader(cx: &mut App) -> bool {
    cx.global::<Lifecycle>()
        .reader
        .is_some_and(|window| live(window, cx))
        && {
            let window = cx.global::<Lifecycle>().reader.unwrap();
            activate(window, cx)
        }
}
pub(super) fn register_reader(window: AnyWindowHandle, cx: &mut App) {
    cx.global_mut::<Lifecycle>().reader = Some(window);
}

pub(super) fn reuse_accounts(cx: &mut App) -> bool {
    let window = cx.global::<Lifecycle>().accounts.as_ref().map(|(w, _)| *w);
    window.is_some_and(|w| activate(w, cx))
}
pub(super) fn register_accounts(
    window: AnyWindowHandle,
    entity: WeakEntity<crate::accounts::Accounts>,
    cx: &mut App,
) {
    cx.global_mut::<Lifecycle>().accounts = Some((window, entity));
}

pub(super) fn reuse_mailbox(cx: &mut App) -> bool {
    let window = cx.global::<Lifecycle>().mailbox;
    window.is_some_and(|window| activate(window, cx))
}
pub(super) fn register_mailbox(window: AnyWindowHandle, cx: &mut App) {
    if cx.global::<Lifecycle>().remote.is_some() {
        let _ = window.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, |_, cx| {
                let state = cx.global_mut::<Lifecycle>();
                state.quitting = true;
                state.shutdown = false;
                state.error = None;
                false
            })
        });
    }
    cx.global_mut::<Lifecycle>().mailbox = Some(window);
}

pub(super) fn reuse_settings(cx: &mut App) -> bool {
    let window = cx.global::<Lifecycle>().settings;
    window.is_some_and(|window| activate(window, cx))
}
pub(super) fn register_settings(window: AnyWindowHandle, cx: &mut App) {
    cx.global_mut::<Lifecycle>().settings = Some(window);
}
pub(super) fn reuse_send_tasks(cx: &mut App) -> bool {
    let window = cx.global::<Lifecycle>().send_tasks;
    window.is_some_and(|window| activate(window, cx))
}
pub(super) fn register_send_tasks(window: AnyWindowHandle, cx: &mut App) {
    cx.global_mut::<Lifecycle>().send_tasks = Some(window);
}

pub(super) fn storage(cx: &App) -> mail_daemon::storage::StorageClient {
    cx.global::<Lifecycle>().storage.client()
}
pub(super) fn default_sender(cx: &App) -> Option<(mail_core::AccountId, String)> {
    let remote = cx.global::<Lifecycle>().remote.as_ref()?;
    let view = remote.snapshot();
    view.snapshot
        .accounts
        .into_iter()
        .find(|(id, _)| Some(id) == view.snapshot.default_account.as_ref())
        .map(|(id, s)| (id, s.address))
}
pub(super) fn own_addresses(cx: &App) -> Vec<String> {
    cx.global::<Lifecycle>()
        .remote
        .as_ref()
        .map(|r| {
            r.snapshot()
                .snapshot
                .accounts
                .iter()
                .map(|(_, s)| s.address.clone())
                .collect()
        })
        .unwrap_or_default()
}
pub(super) fn account_backend(cx: &App) -> Arc<AccountBackend> {
    cx.global::<Lifecycle>().account_backend.clone()
}
pub(super) fn account_requests(cx: &App) -> Arc<mail_storage::ClientRequests> {
    cx.global::<Lifecycle>().account_requests.clone()
}
pub(super) fn pending_account_save(cx: &App) -> Option<crate::accounts::PendingSave> {
    cx.global::<Lifecycle>().pending_account_save.clone()
}
pub(super) fn set_pending_account_save(
    pending: Option<crate::accounts::PendingSave>,
    cx: &mut App,
) {
    cx.global_mut::<Lifecycle>().pending_account_save = pending;
}

pub(super) fn sync_status(cx: &App) -> String {
    let state = cx.global::<Lifecycle>();
    state
        .remote
        .as_ref()
        .map(|r| {
            r.snapshot()
                .error
                .unwrap_or_else(|| r.snapshot().snapshot.status.sync_status)
        })
        .unwrap_or_else(|| state.sync.as_ref().unwrap().status())
}
pub(super) fn account_sync_status(account: &mail_core::AccountId, cx: &App) -> String {
    let state = cx.global::<Lifecycle>();
    let states = state
        .remote
        .as_ref()
        .map(|r| r.snapshot().snapshot.status.account_sync)
        .unwrap_or_else(|| {
            state
                .sync
                .as_ref()
                .map(|s| s.account_states())
                .unwrap_or_default()
        });
    states
        .into_iter()
        .find(|s| &s.account == account)
        .map(|s| format!("{}{}", if s.paused { "已暂停 · " } else { "" }, s.status))
        .unwrap_or_default()
}
pub(super) fn sync_revision(cx: &App) -> u64 {
    let state = cx.global::<Lifecycle>();
    state
        .remote
        .as_ref()
        .map(|r| r.snapshot().snapshot.revision)
        .unwrap_or_else(|| state.sync.as_ref().unwrap().revision())
}
pub(super) fn sync_paused(cx: &App) -> bool {
    let state = cx.global::<Lifecycle>();
    state
        .remote
        .as_ref()
        .map(|r| r.snapshot().snapshot.status.sync_paused)
        .unwrap_or_else(|| state.sync.as_ref().unwrap().paused())
}
pub(super) fn toggle_sync(cx: &App) {
    let state = cx.global::<Lifecycle>();
    if let Some(remote) = &state.remote {
        remote.pause(!remote.snapshot().snapshot.status.sync_paused);
    } else {
        state.sync.as_ref().unwrap().toggle_paused();
    }
}
pub(super) fn sending(cx: &App) -> mail_daemon::sending::SendCoordinator {
    cx.global::<Lifecycle>().sending.clone()
}
pub(super) fn quit(cx: &mut App) {
    let queue = &cx.global::<Lifecycle>().queue;
    if let Err(e) = queue.try_send(Command::Quit) {
        eprintln!("退出请求未入队：{e}");
    }
}
pub(super) fn note(cx: &App) -> String {
    let state = cx.global::<Lifecycle>();
    if let Some(remote) = &state.remote {
        let view = remote.snapshot();
        return state.error.clone().or(view.error).unwrap_or_else(|| {
            format!(
                "{} · 后台 PID {} · {}",
                if state.quitting {
                    "正在保存关闭"
                } else {
                    "关闭前台后后台继续运行"
                },
                view.snapshot.status.pid,
                view.snapshot.status.tray_status
            )
        });
    }
    if let Some(error) = &state.error {
        return error.clone();
    }
    format!(
        "{} · 合成后台 {} 次 · PID {} · {}",
        if state.quitting {
            if state.sending.active() {
                "正在等待发送结果写盘后退出（网络阶段已有期限）"
            } else {
                "正在保存并退出"
            }
        } else {
            "关窗后后台继续运行，再次启动可重开"
        },
        state.worker.as_ref().map_or(0, |w| w.ticks()),
        std::process::id(),
        state.tray_status
    )
}
fn tick(cx: &mut App) {
    cx.update_global::<Lifecycle, _>(|state, cx| {
        if let Some(remote) = &state.remote {
            let view = remote.snapshot();
            if let Some(activation) = view.activation
                && activation.sequence > remote.ack.load(std::sync::atomic::Ordering::Acquire)
            {
                remote
                    .ack
                    .store(activation.sequence, std::sync::atomic::Ordering::Release);
                cx.defer(move |cx| match activation.target {
                    mail_protocol::WindowTarget::Mailbox => open_main(cx),
                    mail_protocol::WindowTarget::Composer => composer::open(cx),
                    mail_protocol::WindowTarget::Reader => reader::open(cx),
                    mail_protocol::WindowTarget::Accounts => crate::accounts::open(cx),
                    mail_protocol::WindowTarget::SendTasks => crate::send_tasks::open(cx),
                });
            }
            if view.shutdown {
                state.quitting = true;
                state.shutdown = true;
            }
            if let Some(result) = view.completion {
                remote
                    .view
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .completion = None;
                state.finishing = false;
                match result {
                    Ok(()) => cx.quit(),
                    Err(error) => {
                        state.quitting = false;
                        state.error = Some(format!("保存后退出未确认：{error}"));
                        cx.defer(open_main);
                    }
                }
            }
        }
        state.editors.retain(|editor| live(editor.window, cx));
        let commands: Vec<_> = state.receiver.try_iter().collect();
        for command in commands {
            match command {
                Command::Quit => {
                    state.quitting = true;
                    state.shutdown = true;
                    if let Some(remote) = &state.remote {
                        remote.shutdown();
                    }
                    state.error = None;
                }
                // 窗口打开必须在 update_global 外，否则 open()/render() 再读 global 会重入。
                other => {
                    let queue = state.queue.clone();
                    cx.defer(move |cx| match other {
                        Command::Open => open_main(cx),
                        Command::Editor => composer::open(cx),
                        Command::Reader => reader::open(cx),
                        Command::Accounts => crate::accounts::open(cx),
                        Command::Mailbox => crate::mailbox::open(cx),
                        Command::SendTasks => crate::send_tasks::open(cx),
                        _ => {
                            let _ = queue;
                        }
                    });
                }
            }
        }
        if state.remote.is_some()
            && std::env::var_os("QINGMAIL_IPC_TEST_EDITOR").is_some()
            && state.probe_step < 4
        {
            if state.probe_step < 3 {
                if let Some(editor) = state.editors.first()
                    && let Ok(Ok(true)) = editor.window.update(cx, |_, window, cx| {
                        editor.entity.update(cx, |editor, cx| {
                            editor.lifecycle_probe(state.probe_step, window, cx)
                        })
                    })
                {
                    state.probe_step += 1;
                }
            } else if state.editors.is_empty() {
                eprintln!("QINGMAIL_IPC_EDITOR: saved and closed through daemon");
                state.probe_step = 4;
            }
        }
        if state.probe_step < 4
            && std::env::args().any(|arg| {
                matches!(
                    arg.as_str(),
                    "--lifecycle-probe" | "--lifecycle-failure-probe"
                )
            })
        {
            if let Some(editor) = state.editors.first() {
                if let Ok(Ok(ready)) = editor.window.update(cx, |_, window, cx| {
                    editor.entity.update(cx, |editor, cx| {
                        editor.lifecycle_probe(state.probe_step, window, cx)
                    })
                }) && ready
                {
                    state.probe_step += 1;
                }
            } else if state.probe_step == 3 {
                for window in cx.windows() {
                    let _ = window.update(cx, |_, window, _| window.remove_window());
                }
                state.probe_step = 4;
            }
        }
        if state.quitting {
            for editor in &state.editors {
                let result = editor.window.update(cx, |_, window, cx| {
                    editor
                        .entity
                        .update(cx, |editor, cx| editor.prepare_close(window, cx))
                });
                if let Ok(Ok(Err(error))) = result {
                    state.error = Some(format!("退出已取消：{error}。请处理编辑窗口后重试。"));
                    state.quitting = false;
                    if let Some(remote) = &state.remote
                        && state.shutdown
                    {
                        remote.finish(false);
                    }
                    cx.defer(open_main);
                    break;
                }
            }
            let account_saving = state.accounts.as_ref().is_some_and(|(_, entity)| {
                entity
                    .upgrade()
                    .is_some_and(|entity| entity.read(cx).saving)
            });
            if state.quitting
                && state.editors.is_empty()
                && !account_saving
                && !state.account_backend.saving()
                && !state.sending.active()
            {
                if let Some(remote) = &state.remote {
                    if !state.finishing {
                        if !state.shutdown {
                            remote.detach();
                            state.finishing = true;
                        } else if remote.snapshot().shutdown {
                            remote.finish(true);
                            state.finishing = true;
                        }
                    }
                } else {
                    cx.quit();
                }
            }
        }
        let status = format!(
            "pid={} ticks={} elapsed_ms={} windows={} editors={} quitting={} probe_step={}",
            std::process::id(),
            state.worker.as_ref().map_or(0, |w| w.ticks()),
            state.worker.as_ref().map_or(0, |w| w.elapsed().as_millis()),
            cx.windows().len(),
            state.editors.len(),
            state.quitting,
            state.probe_step
        );
        *state.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
        if let Some(window) = state.main.filter(|window| live(*window, cx)) {
            let _ = window.update(cx, |_, window, _| {
                window.refresh();
            });
        }
    });
}

pub(super) fn operation_label(location: &mail_core::MailUid, cx: &App) -> String {
    let Some(remote) = cx.global::<Lifecycle>().remote.as_ref() else {
        return String::new();
    };
    let view = remote.view.lock().unwrap_or_else(|e| e.into_inner());
    view.operations
        .iter()
        .find(|o| &o.location == location && o.state != mail_protocol::OperationState::Applied)
        .map(|o| format!(" · {}", crate::operations::state_label(o.state)))
        .unwrap_or_default()
}
pub(super) fn operation_summary(cx: &App) -> String {
    let Some(remote) = cx.global::<Lifecycle>().remote.as_ref() else {
        return String::new();
    };
    let view = remote.view.lock().unwrap_or_else(|e| e.into_inner());
    let active = view
        .operations
        .iter()
        .filter(|o| o.state != mail_protocol::OperationState::Applied)
        .count();
    let issues = view
        .operations
        .iter()
        .filter(|o| {
            matches!(
                o.state,
                mail_protocol::OperationState::Failed
                    | mail_protocol::OperationState::Conflict
                    | mail_protocol::OperationState::Uncertain
            )
        })
        .count();
    if active == 0 {
        "待同步操作".into()
    } else {
        format!("待同步操作 {active} · 待处理 {issues}")
    }
}
