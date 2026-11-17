//! 各账号独立轮询与退避；主动 IMAP 公平限流，持久账号暂停。
use crate::storage::{StorageClient, receive};
use mail_service::{
    credentials::{CredentialVault, SystemVault},
    sync,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const CATALOG_EVERY: usize = 5;

pub struct SyncEngine {
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    revision: Arc<AtomicU64>,
    thread: Option<thread::JoinHandle<()>>,
    requested: Arc<Mutex<Vec<Option<mail_core::AccountId>>>>,
    enabled: bool,
    account_states: Arc<Mutex<Vec<mail_protocol::AccountSyncState>>>,
}
struct Worker {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    requested: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    thread: thread::JoinHandle<()>,
    baseline: Option<mail_storage::AccountVersion>,
}
impl SyncEngine {
    pub fn start(storage: StorageClient) -> Self {
        Self::start_enabled(storage, true)
    }
    pub fn start_enabled(storage: StorageClient, enabled: bool) -> Self {
        let paused = Arc::new(AtomicBool::new(!enabled));
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new("等待账号同步".into()));
        let revision = Arc::new(AtomicU64::new(0));
        let requested = Arc::new(Mutex::new(Vec::<Option<mail_core::AccountId>>::new()));
        let (pause, halt, state, rev, requests) = (
            paused.clone(),
            stop.clone(),
            status.clone(),
            revision.clone(),
            requested.clone(),
        );
        let account_states = Arc::new(Mutex::new(Vec::new()));
        let states = account_states.clone();
        let thread = thread::Builder::new()
            .name("qingmail-sync".into())
            .spawn(move || {
                let mut workers = std::collections::HashMap::<mail_core::AccountId, Worker>::new();
                while !halt.load(Ordering::Acquire) {
                    if let Ok(accounts) = receive(storage.accounts()) {
                        let account_ids: std::collections::HashSet<_> =
                            accounts.iter().map(|(id, _)| id.clone()).collect();
                        let removed: Vec<_> = workers
                            .keys()
                            .filter(|id| !account_ids.contains(*id))
                            .cloned()
                            .collect();
                        for id in removed {
                            if let Some(w) = workers.remove(&id) {
                                w.stop.store(true, Ordering::Release);
                                w.thread.thread().unpark();
                                let _ = w.thread.join();
                            }
                        }
                        let paused_ids =
                            match storage.control(mail_protocol::Command::AccountOptions) {
                                Ok(mail_protocol::Reply::AccountOptions { paused, .. }) => paused,
                                _ => accounts.iter().map(|(id, _)| id.clone()).collect(),
                            };
                        for (id, _) in accounts {
                            if enabled && !workers.contains_key(&id) {
                                let worker_stop = Arc::new(AtomicBool::new(false));
                                let worker_pause = Arc::new(AtomicBool::new(
                                    pause.load(Ordering::Acquire) || paused_ids.contains(&id),
                                ));
                                let worker_request = Arc::new(AtomicBool::new(false));
                                let worker_status = Arc::new(Mutex::new(String::new()));
                                let (st, pa, rq, ss, db, rv, account) = (
                                    worker_stop.clone(),
                                    worker_pause.clone(),
                                    worker_request.clone(),
                                    worker_status.clone(),
                                    storage.clone(),
                                    rev.clone(),
                                    id.clone(),
                                );
                                if let Ok(handle) = thread::Builder::new()
                                    .name("qingmail-account-sync".into())
                                    .spawn(move || {
                                        run_account(db, account, pa, st, ss, rq, rv);
                                    })
                                {
                                    workers.insert(
                                        id.clone(),
                                        Worker {
                                            stop: worker_stop,
                                            paused: worker_pause,
                                            requested: worker_request,
                                            status: worker_status,
                                            thread: handle,
                                            baseline: receive(storage.account_version(id.clone()))
                                                .ok(),
                                        },
                                    );
                                }
                            }
                            if let Some(w) = workers.get_mut(&id) {
                                if let Ok(version) = receive(storage.account_version(id.clone())) {
                                    if w.baseline
                                        .as_ref()
                                        .is_some_and(|baseline| baseline != &version)
                                    {
                                        w.requested.store(true, Ordering::Release);
                                        w.thread.thread().unpark();
                                    }
                                    w.baseline = Some(version);
                                }
                                w.paused.store(
                                    pause.load(Ordering::Acquire) || paused_ids.contains(&id),
                                    Ordering::Release,
                                );
                            }
                        }
                    }
                    let pending =
                        std::mem::take(&mut *requests.lock().unwrap_or_else(|e| e.into_inner()));
                    for (id, w) in &workers {
                        if pending
                            .iter()
                            .any(|target| target.as_ref().is_none_or(|target| target == id))
                        {
                            w.requested.store(true, Ordering::Release);
                            w.thread.thread().unpark();
                        }
                    }
                    let mut account_states: Vec<_> = workers
                        .iter()
                        .map(|(id, w)| mail_protocol::AccountSyncState {
                            account: id.clone(),
                            paused: w.paused.load(Ordering::Acquire),
                            status: w.status.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                        })
                        .collect();
                    account_states.sort_by(|a, b| a.account.0.cmp(&b.account.0));
                    *states.lock().unwrap_or_else(|e| e.into_inner()) = account_states;
                    *state.lock().unwrap_or_else(|e| e.into_inner()) = if !enabled {
                        "隔离后台：不联网".into()
                    } else if workers.is_empty() {
                        "尚未添加邮箱".into()
                    } else {
                        workers
                            .iter()
                            .map(|(id, w)| {
                                format!(
                                    "{}: {}{}",
                                    &id.0[..id.0.len().min(8)],
                                    if w.paused.load(Ordering::Acquire) {
                                        "已暂停 · "
                                    } else {
                                        ""
                                    },
                                    w.status.lock().unwrap_or_else(|e| e.into_inner())
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("；")
                    };
                    thread::park_timeout(Duration::from_millis(500));
                }
                for w in workers.values() {
                    w.stop.store(true, Ordering::Release);
                    w.thread.thread().unpark();
                }
                for (_, w) in workers {
                    let _ = w.thread.join();
                }
            })
            .ok();
        Self {
            paused,
            stop,
            status,
            revision,
            thread,
            requested,
            enabled,
            account_states,
        }
    }
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
    pub fn request_sync(&self) -> bool {
        self.request_account(None)
    }
    pub fn request_account(&self, account: Option<mail_core::AccountId>) -> bool {
        if !self.enabled {
            return false;
        }
        let mut requests = self.requested.lock().unwrap_or_else(|e| e.into_inner());
        if !requests.contains(&account) {
            requests.push(account);
        }
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
        true
    }
    pub fn set_paused(&self, paused: bool) {
        self.paused
            .store(paused || !self.enabled, Ordering::Release);
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
    pub fn toggle_paused(&self) {
        self.set_paused(!self.paused());
    }
    pub fn account_states(&self) -> Vec<mail_protocol::AccountSyncState> {
        self.account_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn status(&self) -> String {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}
impl Drop for SyncEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_account(
    storage: StorageClient,
    account: mail_core::AccountId,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    requested: Arc<AtomicBool>,
    revision: Arc<AtomicU64>,
) {
    run_loop(
        &account,
        paused,
        stop.clone(),
        status,
        requested,
        |refresh| poll_account(&storage, &account, refresh, &revision, &stop),
        || {
            let access = receive(storage.account_version(account.clone()))?;
            let credentials = SystemVault
                .load(&access.credential_key)
                .map_err(|e| e.to_string())?;
            mail_service::idle::watch(&access.settings.imap, &credentials.imap)
                .map_err(|e| e.to_string())
        },
    );
}
fn run_loop(
    account: &mail_core::AccountId,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    requested: Arc<AtomicBool>,
    mut poll: impl FnMut(bool) -> Result<usize, String>,
    mut watch: impl FnMut() -> Result<mail_service::idle::IdleResult, String>,
) {
    let mut next = Instant::now();
    let mut failures = 0u32;
    let mut cycle = 0usize;
    let mut idle_supported = true;
    let mut auth_failed = false;
    while !stop.load(Ordering::Acquire) {
        let manual = requested.swap(false, Ordering::AcqRel);
        if manual {
            auth_failed = false;
        }
        if !manual && (paused.load(Ordering::Acquire) || auth_failed) {
            thread::park_timeout(Duration::from_millis(500));
            continue;
        }
        if manual || Instant::now() >= next {
            cycle += 1;
            *status.lock().unwrap_or_else(|e| e.into_inner()) = "正在同步".into();
            match poll(refresh_on_cycle(cycle)) {
                Ok(count) => {
                    failures = 0;
                    next = Instant::now() + POLL_INTERVAL;
                    *status.lock().unwrap_or_else(|e| e.into_inner()) =
                        format!("已更新 {count} 个文件夹 · {}", sync::display_time(now()));
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    auth_failed = error.contains("认证失败");
                    next = Instant::now() + backoff(failures, account);
                    *status.lock().unwrap_or_else(|e| e.into_inner()) = format!(
                        "{error}{}",
                        if auth_failed {
                            "；认证失效，手动同步或更新凭据后重试"
                        } else {
                            "；独立退避后重试"
                        }
                    );
                }
            }
            if refresh_on_cycle(cycle) {
                idle_supported = true;
            }
        }
        if failures == 0
            && idle_supported
            && !paused.load(Ordering::Acquire)
            && !stop.load(Ordering::Acquire)
        {
            match watch() {
                Ok(mail_service::idle::IdleResult::Changed) => next = Instant::now(),
                Ok(mail_service::idle::IdleResult::Quiet) => (),
                _ => idle_supported = false,
            }
        } else {
            thread::park_timeout(Duration::from_millis(500));
        }
    }
}
fn backoff(failures: u32, account: &mail_core::AccountId) -> Duration {
    let base = match failures {
        0..=1 => 120,
        2 => 300,
        _ => 900,
    };
    // 稳定账号散列避免多个账号同时重连；不需要额外随机依赖。
    let jitter = account
        .0
        .bytes()
        .fold(0u64, |v, b| v.wrapping_mul(31).wrapping_add(u64::from(b)))
        % 16;
    Duration::from_secs(base + jitter)
}

#[cfg(test)]
fn run(
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    requested: Arc<AtomicBool>,
    mut poll: impl FnMut(bool) -> Result<usize, String>,
) {
    run_loop(
        &mail_core::AccountId("test".into()),
        paused,
        stop,
        status,
        requested,
        &mut poll,
        || Ok(mail_service::idle::IdleResult::Unsupported),
    );
}

fn refresh_on_cycle(cycle: usize) -> bool {
    cycle == 1 || cycle.is_multiple_of(CATALOG_EVERY)
}

/// 以远端目录为准，包括从未打开过、尚无 UIDVALIDITY 的文件夹。
fn sync_plan(
    catalog: &[mail_core::RemoteFolder],
    cached: &[mail_core::Folder],
) -> Vec<(String, Option<u32>)> {
    let mut plan: Vec<_> = catalog
        .iter()
        .filter(|folder| folder.selectable)
        .map(|folder| {
            let epoch = cached
                .iter()
                .find(|cached| cached.remote_name == folder.name)
                .map(|cached| cached.uidvalidity);
            (folder.name.clone(), epoch)
        })
        .collect();
    plan.sort_by_key(|(name, _)| !name.eq_ignore_ascii_case("INBOX"));
    plan
}

fn poll_account(
    storage: &StorageClient,
    account: &mail_core::AccountId,
    refresh_catalog: bool,
    revision: &AtomicU64,
    stop: &AtomicBool,
) -> Result<usize, String> {
    if stop.load(Ordering::Acquire) {
        return Ok(0);
    }
    let access = receive(storage.account_version(account.clone()))?;
    let settings = &access.settings;
    let credentials = SystemVault
        .load(&access.credential_key)
        .map_err(|error| error.to_string())?;
    let (mut catalog, _) = receive(storage.catalog(account.clone()))?;
    if refresh_catalog || catalog.is_empty() {
        let _permit = crate::network_gate::acquire(account, false);
        if stop.load(Ordering::Acquire) {
            return Ok(0);
        }
        if receive(storage.account_version(account.clone()))? != access {
            return Err("账号配置已更新，重新同步".into());
        }
        catalog =
            sync::folders(&settings.imap, &credentials.imap).map_err(|error| error.to_string())?;
        receive(storage.publish_catalog(account.clone(), catalog.clone(), now()))?;
        revision.fetch_add(1, Ordering::Release);
    }
    let cached = receive(storage.folders(account.clone()))?;
    let mut synced = 0;
    for (name, epoch) in sync_plan(&catalog, &cached)
        .into_iter()
        .filter(|(name, _)| refresh_catalog || name.eq_ignore_ascii_case("INBOX"))
    {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let _permit = crate::network_gate::acquire(account, false);
        if stop.load(Ordering::Acquire) {
            break;
        }
        if receive(storage.account_version(account.clone()))? != access {
            return Err("账号配置已更新，重新同步".into());
        }
        let batch = sync::folder(&settings.imap, &credentials.imap, &name, None, epoch)
            .map_err(|error| error.to_string())?;
        receive(storage.publish_folder(account.clone(), batch, now()))?;
        revision.fetch_add(1, Ordering::Release);
        synced += 1;
    }
    Ok(synced)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn five_account_loops_keep_progress_when_one_authentication_fails() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut workers = vec![];
        for id in 0..5 {
            let stop = Arc::new(AtomicBool::new(false));
            let halt = stop.clone();
            let tx = tx.clone();
            let handle = thread::spawn(move || {
                run_loop(
                    &mail_core::AccountId(format!("account-{id}")),
                    Arc::new(AtomicBool::new(false)),
                    halt.clone(),
                    Default::default(),
                    Default::default(),
                    |_| {
                        tx.send(id).unwrap();
                        if id == 0 {
                            Err("认证失败".into())
                        } else {
                            halt.store(true, Ordering::Release);
                            Ok(1)
                        }
                    },
                    || panic!("认证失败或已停止的账号不应进入 IDLE"),
                );
            });
            workers.push((stop, handle));
        }
        let mut completed = vec![];
        for _ in 0..5 {
            completed.push(rx.recv_timeout(Duration::from_secs(3)).unwrap());
        }
        completed.sort();
        assert_eq!(completed, vec![0, 1, 2, 3, 4]);
        for (stop, handle) in workers {
            stop.store(true, Ordering::Release);
            handle.thread().unpark();
            handle.join().unwrap();
        }
    }
    #[test]
    fn idle_change_schedules_immediate_inbox_refresh_and_backoff_is_per_account() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut polls = 0;
        run_loop(
            &mail_core::AccountId("a".into()),
            Default::default(),
            stop.clone(),
            Default::default(),
            Default::default(),
            |_| {
                polls += 1;
                if polls == 2 {
                    stop.store(true, Ordering::Release);
                }
                Ok(1)
            },
            || Ok(mail_service::idle::IdleResult::Changed),
        );
        assert_eq!(polls, 2);
        let a = mail_core::AccountId("a".into());
        let b = mail_core::AccountId("b".into());
        assert!((120..136).contains(&backoff(1, &a).as_secs()));
        assert!((300..316).contains(&backoff(2, &a).as_secs()));
        assert!((900..916).contains(&backoff(3, &a).as_secs()));
        assert_ne!(backoff(1, &a), backoff(1, &b));
    }
    #[test]
    fn worker_syncs_once_immediately_on_each_start() {
        for _ in 0..2 {
            let stop = Arc::new(AtomicBool::new(false));
            let status = Arc::new(Mutex::new(String::new()));
            let mut calls = 0;
            run(
                Arc::new(AtomicBool::new(false)),
                stop.clone(),
                status.clone(),
                Arc::new(AtomicBool::new(false)),
                |refresh| {
                    assert!(refresh);
                    calls += 1;
                    stop.store(true, Ordering::Release);
                    Ok(2)
                },
            );
            assert_eq!(calls, 1);
            assert!(status.lock().unwrap().contains("2 个文件夹"));
        }
    }
    #[test]
    fn startup_refreshes_before_periodic_catalog_interval() {
        assert!(refresh_on_cycle(1));
        assert!(!refresh_on_cycle(2));
        assert!(refresh_on_cycle(CATALOG_EVERY));
    }
    #[test]
    fn startup_requests_during_poll_coalesce_and_manual_sync_can_run_while_paused() {
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(AtomicBool::new(true));
        let mut calls = 0;
        run(
            Arc::new(AtomicBool::new(true)),
            stop.clone(),
            Default::default(),
            wake.clone(),
            |_| {
                calls += 1;
                if calls == 1 {
                    for _ in 0..20 {
                        wake.store(true, Ordering::Release);
                    }
                } else {
                    stop.store(true, Ordering::Release);
                }
                Ok(0)
            },
        );
        assert_eq!(calls, 2);
    }
    #[test]
    fn plan_includes_unopened_folders_and_skips_containers() {
        let catalog: Vec<_> = (0..7)
            .map(|i| mail_core::RemoteFolder {
                name: format!("folder-{i}"),
                display_name: format!("folder-{i}"),
                delimiter: Some("/".into()),
                selectable: i != 0,
            })
            .collect();
        let cached = vec![mail_core::Folder {
            id: mail_core::FolderId("local".into()),
            account: mail_core::AccountId("test".into()),
            remote_name: "folder-1".into(),
            uidvalidity: 42,
        }];
        let plan = sync_plan(&catalog, &cached);
        assert_eq!(plan.len(), 6); // 不再只同步前四个已打开文件夹
        assert_eq!(plan[0], ("folder-1".into(), Some(42)));
        assert_eq!(plan[5], ("folder-6".into(), None));
    }
}
