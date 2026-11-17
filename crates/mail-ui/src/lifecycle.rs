//! 过渡期进程内生命周期：窗口可销毁，后台/实例锁由 App 持有；P3 逐步迁往独立后台。
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
    worker: Background,
    account_backend: Arc<mail_daemon::account_gateway::AccountGateway>,
    pending_account_save: Option<crate::accounts::PendingSave>,
    account_requests: Arc<mail_storage::ClientRequests>,
    storage: mail_daemon::storage::StorageWorker,
    sync: mail_daemon::SyncEngine,
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

pub(super) fn start(instance: Option<Instance>, cx: &mut App) -> Result<(), String> {
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
    let request_path = match &storage_path {
        Some(path) => path.join("client-requests"),
        None => mail_storage::AppPaths::discover()
            .map_err(|e| e.to_string())?
            .config
            .join("client-requests"),
    };
    let account_requests =
        Arc::new(mail_storage::ClientRequests::open(&request_path).map_err(|e| e.to_string())?);
    let pending_account_save = account_requests.load().map_err(|e| e.to_string())?;
    let storage =
        mail_daemon::storage::StorageWorker::start(storage_path).map_err(|e| e.to_string())?;
    let worker = Background::start().map_err(|e| e.to_string())?;
    let probe = std::env::args().any(|arg| arg.ends_with("-probe"));
    let sync = mail_daemon::SyncEngine::start_enabled(storage.client(), !probe);
    let account_backend = Arc::new(
        mail_daemon::account_gateway::AccountGateway::start(
            !probe,
            storage.client(),
            Default::default(),
        )
        .map_err(|e| e.to_string())?,
    );
    let tray_enabled = !std::env::args().any(|arg| {
        matches!(
            arg.as_str(),
            "--no-tray" | "--editor-probe" | "--editor-style-demo"
        )
    }) && std::env::var_os("QINGMAIL_P0_RUNTIME_ROOT").is_none();
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
        .find(|e| live(e.window, cx))
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
pub(super) fn account_backend(cx: &App) -> Arc<mail_daemon::account_gateway::AccountGateway> {
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
    cx.global::<Lifecycle>().sync.status()
}
pub(super) fn sync_revision(cx: &App) -> u64 {
    cx.global::<Lifecycle>().sync.revision()
}
pub(super) fn sync_paused(cx: &App) -> bool {
    cx.global::<Lifecycle>().sync.paused()
}
pub(super) fn toggle_sync(cx: &App) {
    cx.global::<Lifecycle>().sync.toggle_paused();
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
        state.worker.ticks(),
        std::process::id(),
        state.tray_status
    )
}
fn tick(cx: &mut App) {
    cx.update_global::<Lifecycle, _>(|state, cx| {
        state.editors.retain(|editor| live(editor.window, cx));
        let commands: Vec<_> = state.receiver.try_iter().collect();
        for command in commands {
            match command {
                Command::Quit => {
                    state.quitting = true;
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
                cx.quit();
            }
        }
        let status = format!(
            "pid={} ticks={} elapsed_ms={} windows={} editors={} quitting={} probe_step={}",
            std::process::id(),
            state.worker.ticks(),
            state.worker.elapsed().as_millis(),
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
