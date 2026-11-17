//! 独立附件下载线程；网络流入私有匿名临时fd，完整解码/同步/关联后才发布Ready。
use super::{
    resources::{DownloadProgress, PartSpec, Resources},
    rpc_error,
};
use crate::storage::{StorageClient, receive};
use mail_protocol::*;
use mail_service::credentials::{CredentialVault, SystemVault};
use std::{
    fs::File,
    sync::{Arc, Mutex, atomic::Ordering, mpsc},
    thread,
};
type Work = (String, PartSpec, Arc<DownloadProgress>);
pub(super) struct Downloads {
    queue: Option<mpsc::SyncSender<Work>>,
    workers: Vec<thread::JoinHandle<()>>,
    resources: Arc<Mutex<Resources>>,
    enabled: bool,
}
impl Downloads {
    pub fn start(
        store: StorageClient,
        resources: Arc<Mutex<Resources>>,
        enabled: bool,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Work>(8);
        let rx = Arc::new(Mutex::new(rx));
        let mut workers = vec![];
        for _ in 0..2 {
            let (store, resources, rx) = (store.clone(), resources.clone(), rx.clone());
            workers.push(
                thread::Builder::new()
                    .name("qingmail-attachment".into())
                    .spawn(move || {
                        loop {
                            let work = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                            let Ok((id, spec, progress)) = work else {
                                break;
                            };
                            let _permit =
                                crate::network_gate::acquire(&spec.location.account, true);
                            let result = download(&store, &spec, &progress);
                            resources
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .finish_download_if_current(&id, &progress, result);
                        }
                    })?,
            );
        }
        Ok(Self {
            queue: Some(tx),
            workers,
            resources,
            enabled,
        })
    }
    pub fn start_download(&self, id: &str) -> Result<Reply, RpcError> {
        let mut resources = self.resources.lock().unwrap_or_else(|e| e.into_inner());
        let info = resources.download_status(id)?;
        if info.resource.state == ResourceState::Ready {
            return Ok(Reply::Download(info));
        }
        if !self.enabled {
            return Err(rpc_error(
                ErrorCode::Offline,
                "未缓存附件需要联网；隔离后台禁止访问邮箱",
            ));
        }
        if let Some((spec, progress)) = resources.begin_download(id)?
            && self
                .queue
                .as_ref()
                .is_none_or(|q| q.try_send((id.into(), spec, progress)).is_err())
        {
            resources.finish_download(id, Err("下载队列繁忙，请重试".into()));
        }
        Ok(Reply::Download(resources.download_status(id)?))
    }
    pub fn stop(&mut self) {
        self.resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel_downloads();
        self.queue.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
impl Drop for Downloads {
    fn drop(&mut self) {
        self.stop();
    }
}
fn download(
    store: &StorageClient,
    spec: &PartSpec,
    progress: &DownloadProgress,
) -> Result<File, String> {
    if progress.cancel.load(Ordering::Acquire) {
        return Err("下载已取消".into());
    }
    if let Some(file) = receive(store.part_file(spec.location.clone(), spec.part.clone()))? {
        return Ok(file);
    }
    let access = receive(store.account_version(spec.location.account.clone()))?;
    let credentials = SystemVault
        .load(&access.credential_key)
        .map_err(|e| e.to_string())?;
    let mut file = receive(store.part_temporary())?;
    mail_service::parts::fetch(
        &access.settings.imap,
        &credentials.imap,
        &spec.folder,
        spec.location.uidvalidity,
        spec.location.uid,
        &spec.part,
        &mut file,
        mail_service::parts::MAX_RECEIVED_PART,
        |n| {
            progress.received.store(n, Ordering::Release);
            !progress.cancel.load(Ordering::Acquire)
        },
    )
    .map_err(|e| e.to_string())?;
    file.sync_all()
        .map_err(|e| format!("下载写入失败，请检查空间：{e}"))?;
    if progress.cancel.load(Ordering::Acquire) {
        return Err("下载已取消，未标记已下载".into());
    }
    if receive(store.account_version(spec.location.account.clone()))? != access {
        return Err("账号配置已变化，下载未发布".into());
    }
    receive(store.save_part(spec.location.clone(), spec.part.clone(), file))?;
    receive(store.part_file(spec.location.clone(), spec.part.clone()))?
        .ok_or("下载完成但缓存未确认".into())
}
