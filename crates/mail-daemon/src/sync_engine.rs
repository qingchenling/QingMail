//! 单账号低频轮询收信：暂停、失败退避与目录周期刷新；不做 IDLE 或多账号调度。
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
}
impl SyncEngine {
    pub fn start(storage: StorageClient) -> Self {
        Self::start_enabled(storage, true)
    }
    /// 原生合成探针显式禁用网络；正常启动始终立即收信。
    pub fn start_enabled(storage: StorageClient, enabled: bool) -> Self {
        let revision = Arc::new(AtomicU64::new(0));
        let paused = Arc::new(AtomicBool::new(!enabled));
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new("自动收信：等待首次轮询".to_string()));
        let worker = (
            paused.clone(),
            stop.clone(),
            status.clone(),
            revision.clone(),
            storage,
        );
        let _ = thread::Builder::new()
            .name("qingmail-sync".into())
            .spawn(move || {
                let (paused, stop, status, revision, storage) = worker;
                run(paused, stop.clone(), status, |refresh| {
                    poll_once(&storage, refresh, &revision, &stop)
                });
            });
        Self {
            paused,
            stop,
            status,
            revision,
        }
    }
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
    pub fn toggle_paused(&self) {
        let now = !self.paused.load(Ordering::Acquire);
        self.paused.store(now, Ordering::Release);
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = if now {
            "自动收信已暂停；可手动同步".into()
        } else {
            "自动收信已恢复".into()
        };
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
        self.stop.store(true, Ordering::Release);
    }
}

fn run(
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    mut poll: impl FnMut(bool) -> Result<usize, String>,
) {
    let mut next = Instant::now();
    let mut failures = 0u32;
    let mut cycles = 0usize;
    while !stop.load(Ordering::Acquire) {
        if Instant::now() < next {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        if paused.load(Ordering::Acquire) {
            next = Instant::now() + Duration::from_secs(2);
            continue;
        }
        cycles += 1;
        *status.lock().unwrap_or_else(|e| e.into_inner()) = "正在同步邮箱…".into();
        let result = poll(refresh_on_cycle(cycles));
        let (message, interval) = match result {
            Ok(folders) => {
                failures = 0;
                (
                    format!(
                        "自动收信：{folders} 个文件夹 · {}",
                        mail_service::sync::display_time(now())
                    ),
                    POLL_INTERVAL,
                )
            }
            Err(error) => {
                failures += 1;
                let interval = match failures {
                    0..=1 => Duration::from_secs(120),
                    2 => Duration::from_secs(300),
                    _ => Duration::from_secs(900),
                };
                (
                    format!("自动收信失败（第{failures}次），稍后重试：{error}"),
                    interval,
                )
            }
        };
        *status.lock().unwrap_or_else(|e| e.into_inner()) = message;
        next = Instant::now() + interval;
    }
}

fn refresh_on_cycle(cycle: usize) -> bool {
    cycle == 1 || cycle.is_multiple_of(CATALOG_EVERY)
}

/// 以远端目录为准，包括从未打开过、尚无 UIDVALIDITY 的文件夹。
fn sync_plan(
    catalog: &[mail_core::RemoteFolder],
    cached: &[mail_core::Folder],
) -> Vec<(String, Option<u32>)> {
    catalog
        .iter()
        .filter(|folder| folder.selectable)
        .map(|folder| {
            let epoch = cached
                .iter()
                .find(|cached| cached.remote_name == folder.name)
                .map(|cached| cached.uidvalidity);
            (folder.name.clone(), epoch)
        })
        .collect()
}

fn poll_once(
    storage: &StorageClient,
    refresh_catalog: bool,
    revision: &AtomicU64,
    stop: &AtomicBool,
) -> Result<usize, String> {
    let accounts = receive(storage.accounts())?;
    let Some((account, settings)) = accounts.into_iter().next() else {
        return Ok(0);
    };
    let credentials = SystemVault
        .load(&account)
        .map_err(|error| error.to_string())?;
    let (mut catalog, _) = receive(storage.catalog(account.clone()))?;
    if refresh_catalog || catalog.is_empty() {
        catalog =
            sync::folders(&settings.imap, &credentials.imap).map_err(|error| error.to_string())?;
        receive(storage.publish_catalog(account.clone(), catalog.clone(), now()))?;
        revision.fetch_add(1, Ordering::Release);
    }
    let cached = receive(storage.folders(account.clone()))?;
    let mut synced = 0;
    for (name, epoch) in sync_plan(&catalog, &cached) {
        if stop.load(Ordering::Acquire) {
            break;
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
    fn worker_syncs_once_immediately_on_each_start() {
        for _ in 0..2 {
            let stop = Arc::new(AtomicBool::new(false));
            let status = Arc::new(Mutex::new(String::new()));
            let mut calls = 0;
            run(
                Arc::new(AtomicBool::new(false)),
                stop.clone(),
                status.clone(),
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
