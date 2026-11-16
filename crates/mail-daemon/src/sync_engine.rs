//! 单账号低频轮询收信：暂停、失败退避与目录周期刷新；不做 IDLE 或多账号调度。
use crate::storage::{StorageClient, receive};
use mail_service::{
    credentials::{CredentialVault, SystemVault},
    sync,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
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
}
impl SyncEngine {
    pub fn start(storage: StorageClient) -> Self {
        let paused = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new("自动收信：等待首次轮询".to_string()));
        let worker = (paused.clone(), stop.clone(), status.clone(), storage);
        let _ = thread::Builder::new()
            .name("qingmail-sync".into())
            .spawn(move || {
                let (paused, stop, status, storage) = worker;
                run(storage, paused, stop, status);
            });
        Self {
            paused,
            stop,
            status,
        }
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
    storage: StorageClient,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
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
        let result = poll_once(&storage, cycles.is_multiple_of(CATALOG_EVERY));
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

fn poll_once(storage: &StorageClient, refresh_catalog: bool) -> Result<usize, String> {
    let accounts = receive(storage.accounts())?;
    let Some((account, settings)) = accounts.into_iter().next() else {
        return Ok(0);
    };
    let credentials = SystemVault
        .load(&account)
        .map_err(|error| error.to_string())?;
    if refresh_catalog {
        let folders =
            sync::folders(&settings.imap, &credentials.imap).map_err(|error| error.to_string())?;
        receive(storage.publish_catalog(account.clone(), folders, now()))?;
    }
    let folders = receive(storage.folders(account.clone()))?;
    let mut synced = 0;
    for folder in folders.iter().take(4) {
        let batch = sync::folder(
            &settings.imap,
            &credentials.imap,
            &folder.remote_name,
            None,
            Some(folder.uidvalidity),
        )
        .map_err(|error| error.to_string())?;
        receive(storage.publish_folder(account.clone(), batch, now()))?;
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
