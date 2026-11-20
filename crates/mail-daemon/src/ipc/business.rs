//! 有界后台业务队列。邮件网络、解析和安全副本只在此执行；不会恢复重放在线移动。
use super::{resources::Resources, rpc_error};
use crate::storage::{StorageClient, receive};
use mail_content::MailContent;
use mail_protocol::*;
use mail_service::{
    actions,
    credentials::{CredentialVault, SystemVault},
    outgoing, sync,
};
use mail_storage::MutationClaim;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
type ResultMap = Arc<Mutex<HashMap<String, (Request, Instant, Option<Result<Reply, RpcError>>)>>>;
struct Context {
    store: StorageClient,
    enabled: bool,
    cache: PathBuf,
    resources: Arc<Mutex<Resources>>,
    contents: HashMap<String, (Instant, MailContent)>,
    cancel: Arc<AtomicBool>,
}
enum Work {
    Run(Box<Request>),
    Stop,
}
pub(super) struct Business {
    queue: mpsc::SyncSender<Work>,
    results: ResultMap,
    cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    storage: StorageClient,
    worker: Option<thread::JoinHandle<()>>,
}
fn fail(e: impl ToString) -> RpcError {
    rpc_error(ErrorCode::Storage, e.to_string())
}
fn cancellable_read(command: &Command) -> bool {
    matches!(
        command,
        Command::Business {
            operation: BusinessOperation::Read { .. } | BusinessOperation::RemoteImages { .. }
        }
    )
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
impl Business {
    pub fn start(
        storage: StorageClient,
        enabled: bool,
        cache: PathBuf,
        resources: Arc<Mutex<Resources>>,
        revision: Arc<AtomicU64>,
    ) -> std::io::Result<Self> {
        let (queue, receiver) = mpsc::sync_channel(8);
        let results: ResultMap = Default::default();
        let replies = results.clone();
        let cancellations: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Default::default();
        let cancels = cancellations.clone();
        let store = storage.clone();
        let worker = thread::Builder::new()
            .name("qingmail-business".into())
            .spawn(move || {
                let mut context = Context {
                    store,
                    enabled,
                    cache,
                    resources,
                    contents: HashMap::new(),
                    cancel: Arc::new(AtomicBool::new(false)),
                };
                while let Ok(Work::Run(request)) = receiver.recv() {
                    let request = *request;
                    let Command::Business { operation } = &request.command else {
                        continue;
                    };
                    context.cancel = cancels
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&request.id)
                        .cloned()
                        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                    let mut result = context.execute(operation.clone());
                    cancels
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&request.id);
                    if operation.writes()
                        && !matches!(
                            receive(
                                context
                                    .store
                                    .finish_mutation(request.clone(), result.clone())
                            ),
                            Ok(Ok(()))
                        )
                    {
                        result = Err(rpc_error(
                            ErrorCode::Uncertain,
                            "在线操作结果未可靠记录，请同步核对；不会自动重放",
                        ));
                    }
                    if matches!(
                        operation,
                        BusinessOperation::ScanServerDrafts { .. }
                            | BusinessOperation::ImportServerDraft { .. }
                            | BusinessOperation::RefreshCatalog { .. }
                            | BusinessOperation::SyncFolder { .. }
                            | BusinessOperation::Flags { .. }
                            | BusinessOperation::Move { .. }
                            | BusinessOperation::RemoveAccount { .. }
                    ) {
                        revision.fetch_add(1, Ordering::Release);
                    }
                    if let Some(entry) = replies
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_mut(&request.id)
                    {
                        entry.1 = Instant::now();
                        entry.2 = Some(result);
                    }
                }
            })?;
        Ok(Self {
            queue,
            results,
            cancellations,
            storage,
            worker: Some(worker),
        })
    }
    pub fn cancel_read(&self, id: &str) {
        if let Some(cancel) = self
            .cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
        {
            cancel.store(true, Ordering::Release);
        }
    }
    pub fn active(&self) -> bool {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|(_, _, r)| r.is_none())
    }
    /// A foreground exit can abandon reads; writes and other business work must finish first.
    pub fn cancel_reads(&self) {
        let results = self.results.lock().unwrap_or_else(|e| e.into_inner());
        let cancellations = self.cancellations.lock().unwrap_or_else(|e| e.into_inner());
        for (id, (request, _, result)) in results.iter() {
            if result.is_none()
                && cancellable_read(&request.command)
                && let Some(cancel) = cancellations.get(id)
            {
                cancel.store(true, Ordering::Release);
            }
        }
    }
    pub fn has_non_read_work(&self) -> bool {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|(request, _, result)| result.is_none() && !cancellable_read(&request.command))
    }
    pub fn owns(&self, id: &str) -> bool {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .is_some_and(|(_, _, r)| r.is_none())
    }
    pub fn result(&self, id: &str) -> Result<Reply, RpcError> {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|(_, _, r)| Reply::Job {
                result: r.clone().map(Box::new),
            })
            .ok_or_else(|| {
                rpc_error(
                    ErrorCode::NotFound,
                    "任务已失效或后台已重启，请重新读取状态",
                )
            })
    }
    pub fn submit(&self, request: Request) -> Result<Reply, RpcError> {
        let Command::Business { operation } = &request.command else {
            return Err(rpc_error(ErrorCode::InvalidRequest, "业务命令无效"));
        };
        if matches!(
            operation,
            BusinessOperation::Flags { .. } | BusinessOperation::Move { .. }
        ) {
            return Err(rpc_error(
                ErrorCode::Unsupported,
                "邮件修改须使用持久 EnqueueOperation 队列",
            ));
        }
        let mut results = self.results.lock().unwrap_or_else(|e| e.into_inner());
        results.retain(|_, (_, at, r)| r.is_none() || at.elapsed() < Duration::from_secs(600));
        if let Some((original, _, _)) = results.get(&request.id) {
            if serde_json::to_vec(original).ok() != serde_json::to_vec(&request).ok() {
                return Err(rpc_error(ErrorCode::Conflict, "同一请求ID不能提交不同业务"));
            }
            return Ok(Reply::Pending { task: request.id });
        }
        if results.len() >= 64 {
            let oldest = results
                .iter()
                .filter(|(_, (_, _, r))| r.is_some())
                .min_by_key(|(_, (_, at, _))| *at)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                results.remove(&id);
            }
        }
        if results.len() >= 64 {
            return Err(rpc_error(ErrorCode::Busy, "后台任务结果已满，请稍后重试"));
        }
        if operation.writes() {
            match receive(self.storage.claim_mutation(request.clone())).map_err(fail)?? {
                MutationClaim::Acquired => (),
                MutationClaim::Existing(MutationStatus::Finished(result)) => return *result,
                MutationClaim::Existing(_) => {
                    return Err(rpc_error(
                        ErrorCode::Uncertain,
                        "在线操作曾被执行或中断；先同步核对，不会自动重放",
                    ));
                }
            }
        }
        if cancellable_read(&request.command) {
            self.cancellations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(request.id.clone(), Arc::new(AtomicBool::new(false)));
        }
        results.insert(request.id.clone(), (request.clone(), Instant::now(), None));
        if self
            .queue
            .try_send(Work::Run(Box::new(request.clone())))
            .is_err()
        {
            results.remove(&request.id);
            self.cancellations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&request.id);
            let result = Err(rpc_error(ErrorCode::Busy, "后台队列繁忙，请稍后重试"));
            if operation.writes() {
                let _ = receive(self.storage.finish_mutation(request, result.clone()));
            }
            return result;
        }
        Ok(Reply::Pending { task: request.id })
    }
    pub fn stop(&mut self) {
        for cancel in self
            .cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            cancel.store(true, Ordering::Release);
        }
        if let Some(worker) = self.worker.take() {
            let _ = self.queue.send(Work::Stop);
            let _ = worker.join();
        }
    }
}
impl Drop for Business {
    fn drop(&mut self) {
        self.stop();
    }
}
impl Context {
    fn credentials(
        &self,
        account: mail_core::AccountId,
    ) -> Result<
        (
            mail_core::AccountSettings,
            mail_service::credentials::Credentials,
        ),
        RpcError,
    > {
        if !self.enabled {
            return Err(rpc_error(
                ErrorCode::Offline,
                "隔离后台禁止邮件网络与密钥库访问",
            ));
        }
        let access = receive(self.store.account_version(account)).map_err(fail)?;
        let credentials = SystemVault.load(&access.credential_key).map_err(fail)?;
        Ok((access.settings, credentials))
    }
    fn execute(&mut self, operation: BusinessOperation) -> Result<Reply, RpcError> {
        use BusinessOperation::*;
        let account = match &operation {
            ScanServerDrafts { account }
            | ImportServerDraft { account, .. }
            | RefreshCatalog { account }
            | SyncFolder { account, .. }
            | Flags { account, .. }
            | Move { account, .. }
            | RemoveAccount { account } => Some(account),
            Read { location, .. } => Some(&location.account),
            _ => None,
        };
        let _permit = account.map(|id| crate::network_gate::acquire(id, true));
        if self.cancel.load(Ordering::Acquire) {
            return Err(rpc_error(ErrorCode::Offline, "阅读已取消"));
        }
        match operation {
            ReadServerDraft { account, source } => {
                let raw =
                    receive(self.store.server_draft_original(account, source)).map_err(fail)?;
                let content = mail_content::parse_server_draft(&raw).map_err(fail)?;
                self.publish(content, None)
            }

            ScanServerDrafts { account } => {
                let (settings, credentials) = self.credentials(account.clone())?;
                let found = mail_service::server_drafts::folders(&settings.imap, &credentials.imap)
                    .map_err(fail)?;
                receive(
                    self.store
                        .publish_catalog(account.clone(), found.folders.clone(), now()),
                )
                .map_err(fail)?;
                let folder = receive(self.store.discover_draft_folder(account, found.special))
                    .map_err(fail)?;
                let index = folder
                    .as_deref()
                    .map(|folder| {
                        mail_service::server_drafts::index(
                            &settings.imap,
                            &credentials.imap,
                            folder,
                        )
                    })
                    .transpose()
                    .map_err(fail)?;
                Ok(Reply::ServerDraftIndex {
                    folder,
                    uidvalidity: index.as_ref().map_or(0, |i| i.uidvalidity),
                    uids: index.as_ref().map_or_else(Vec::new, |i| i.uids.clone()),
                    has_more: index.is_some_and(|i| i.has_more),
                    choices: found.folders,
                })
            }
            ImportServerDraft {
                account,
                folder,
                uidvalidity,
                uid,
            } => {
                let configured = match self
                    .store
                    .control(Command::ServerDrafts {
                        account: account.clone(),
                    })
                    .map_err(fail)?
                {
                    Reply::ServerDrafts { folder, .. } => folder,
                    _ => return Err(fail("草稿目录状态无效")),
                };
                if configured.as_deref() != Some(&folder) {
                    return Err(rpc_error(ErrorCode::Conflict, "草稿目录已变化，请重新扫描"));
                }
                let baseline =
                    receive(self.store.account_version(account.clone())).map_err(fail)?;
                let (settings, credentials) = self.credentials(account.clone())?;
                let raw = mail_service::server_drafts::message(
                    &settings.imap,
                    &credentials.imap,
                    &folder,
                    uidvalidity,
                    uid,
                )
                .map_err(fail)?;
                if receive(self.store.account_version(account.clone())).map_err(fail)? != baseline {
                    return Err(rpc_error(
                        ErrorCode::Conflict,
                        "账号配置已变化，未关联旧服务器原件",
                    ));
                }
                let source = receive(self.store.archive_server_draft(
                    account,
                    folder,
                    uidvalidity,
                    uid,
                    raw,
                ))
                .map_err(fail)?;
                Ok(Reply::ServerDraftSource(source))
            }

            RefreshCatalog { account } => {
                let (settings, credentials) = self.credentials(account.clone())?;
                let folders = sync::folders(&settings.imap, &credentials.imap).map_err(fail)?;
                receive(self.store.publish_catalog(account, folders.clone(), now()))
                    .map_err(fail)?;
                Ok(Reply::Catalog {
                    folders,
                    synced_at: Some(now()),
                })
            }
            SyncFolder {
                account,
                name,
                before,
                history,
            } => {
                let cached = receive(self.store.folder_cache(account.clone(), name.clone(), None))
                    .map_err(fail)?;
                let epoch = cached.as_ref().map(|c| c.folder.uidvalidity);
                let (settings, credentials) = self.credentials(account.clone())?;
                let batch = if history {
                    sync::history(&settings.imap, &credentials.imap, &name, before, epoch)
                } else {
                    sync::folder(&settings.imap, &credentials.imap, &name, before, epoch)
                }
                .map_err(fail)?;
                let count = batch.messages.len() as u64;
                let has_more = batch.has_more;
                receive(self.store.publish_folder(account, batch, now())).map_err(fail)?;
                Ok(Reply::History { count, has_more })
            }
            Read { location, folder } => {
                // 文件夹名须来自完整身份，不能把另一文件夹的UID正文绑定到当前邮件。
                let folders =
                    receive(self.store.folders(location.account.clone())).map_err(fail)?;
                if !folders.iter().any(|f| {
                    f.id.0 == location.folder_id
                        && f.remote_name == folder
                        && f.uidvalidity == location.uidvalidity
                }) {
                    return Err(rpc_error(ErrorCode::Conflict, "邮件定位已变化，请重新同步"));
                }
                let content = self.read_sections(&location, &folder)?;
                self.publish(content, Some(location))
            }
            Flags {
                account,
                name,
                epoch,
                uids,
                seen,
                flagged,
            } => {
                let (settings, credentials) = self.credentials(account.clone())?;
                actions::store_flags(
                    &settings.imap,
                    &credentials.imap,
                    &name,
                    epoch,
                    &uids,
                    actions::FlagChange { seen, flagged },
                )
                .map_err(fail)?;
                let count = uids.len() as u64;
                receive(
                    self.store
                        .set_flags(account, name, epoch, uids, seen, flagged),
                )
                .map_err(fail)?;
                Ok(Reply::Count(count))
            }
            Move {
                account,
                name,
                epoch,
                uids,
                destination,
            } => {
                let (settings, credentials) = self.credentials(account.clone())?;
                let outcome = actions::move_messages(
                    &settings.imap,
                    &credentials.imap,
                    &name,
                    epoch,
                    &uids,
                    &destination,
                )
                .map_err(|e| {
                    rpc_error(
                        ErrorCode::Uncertain,
                        format!("移动结果未确认，先同步核对，勿重复移动：{e}"),
                    )
                })?;
                let count = uids.len() as u64;
                receive(self.store.remove_cached(account, name, epoch, uids)).map_err(fail)?;
                Ok(Reply::Moved {
                    count,
                    copied_only: matches!(outcome, actions::MoveOutcome::CopiedOnly),
                    destination,
                })
            }
            TaskContent { account, task } => {
                // A sent receipt and its read-only local record always describe
                // the frozen send snapshot, even when a server copy exists.
                // Opening this view must not require credentials or network.
                let snapshot = receive(self.store.task_snapshot(account, task)).map_err(fail)?;
                let raw = outgoing::prepare(&snapshot, true)
                    .map_err(fail)?
                    .raw()
                    .to_vec();
                self.publish(mail_content::parse(&raw).map_err(fail)?, None)
            }
            BrowserCopy { content } => {
                let mail = self.content(&content)?.clone();
                self.cache_dir()?;
                let path = mail_content::CopyStore::open(&self.cache)
                    .map_err(fail)?
                    .write(&mail)
                    .map_err(fail)?;
                // 返回由后台生成的受控文件URL，不接受前台任意路径。
                let url = mail_content::file_url(&path).map_err(fail)?;
                Ok(Reply::BrowserCopy { url })
            }
            RemoteImages { content } => {
                if !self.enabled {
                    return Err(rpc_error(ErrorCode::Offline, "隔离模式不加载远程图片"));
                }
                let mut mail = self.content(&content)?.clone();
                mail.load_remote_images_with_cancel(&self.cancel)
                    .map_err(fail)?;
                self.publish(mail, None)
            }
            ClearCopies => {
                self.cache_dir()?;
                Ok(Reply::Count(
                    mail_content::CopyStore::open(&self.cache)
                        .map_err(fail)?
                        .clear()
                        .map_err(fail)? as u64,
                ))
            }
            RemoveAccount { account } => {
                let (impact, keys) = receive(self.store.remove_local(account)).map_err(fail)?;
                if self.enabled {
                    for key in keys {
                        SystemVault
                            .delete(&key)
                            .map_err(|e| fail(format!("账号已移除，但密钥库清理失败：{e}")))?;
                    }
                }
                Ok(Reply::AccountImpact(impact))
            }
        }
    }
    fn cache_dir(&self) -> Result<(), RpcError> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.cache)
            .map_err(fail)
    }
    fn content(&mut self, id: &str) -> Result<&MailContent, RpcError> {
        self.contents
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
        let (at, content) = self
            .contents
            .get_mut(id)
            .ok_or_else(|| rpc_error(ErrorCode::NotFound, "阅读内容已失效，请重新选中邮件"))?;
        *at = Instant::now();
        Ok(content)
    }
    fn publish(
        &mut self,
        content: MailContent,
        location: Option<mail_core::MailUid>,
    ) -> Result<Reply, RpcError> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(fail("阅读已取消"));
        }
        let mut resources = self.resources.lock().unwrap_or_else(|e| e.into_inner());
        let mut inserted = vec![];
        let lazy_refs = content
            .attachments
            .iter()
            .map(|a| {
                a.remote
                    .as_ref()
                    .map(|id| resources.download_status(id).map(|s| s.resource))
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = (|| {
            let mut put = |bytes: Arc<[u8]>, kind: String| {
                let reference = resources.insert(bytes, kind)?;
                inserted.push(reference.id.clone());
                Ok::<_, RpcError>(reference)
            };
            let document = put(
                serde_json::to_vec(&content.document).map_err(fail)?.into(),
                "application/vnd.qingmail.document+json".into(),
            )?;
            let plain = put(content.plain.as_bytes().into(), "text/plain".into())?;
            let attachments = content
                .attachments
                .iter()
                .zip(lazy_refs)
                .map(|(a, lazy)| {
                    Ok(ReceivedAttachment {
                        name: a.name.clone(),
                        resource: match lazy {
                            Some(r) => r,
                            None => put(a.bytes.clone(), a.media_type.clone())?,
                        },
                    })
                })
                .collect::<Result<Vec<_>, RpcError>>()?;
            let images = content
                .images
                .iter()
                .map(|i| {
                    Ok(ReceivedImage {
                        key: i.key.clone(),
                        resource: put(i.png.clone(), "image/png".into())?,
                        width: i.width,
                        height: i.height,
                    })
                })
                .collect::<Result<Vec<_>, RpcError>>()?;
            Ok(Reply::Message(MessageView {
                location,
                subject: content.subject.clone(),
                sender: content.sender.clone(),
                from: content.from.clone(),
                to: content.to.clone(),
                cc: content.cc.clone(),
                reply_to: content.reply_to.clone(),
                date_unix: content.date_unix,
                date_text: content.date_text.clone(),
                document,
                plain,
                attachments,
                images,
                warnings: content.warnings.clone(),
                remote_images: content.remote_images.len() as u32,
                is_html: content.is_html,
            }))
        })();
        if result.is_err() {
            for id in inserted {
                resources.cancel(&id);
            }
        }
        if let Ok(Reply::Message(view)) = &result {
            if self.contents.len() >= 4
                && let Some(old) = self
                    .contents
                    .iter()
                    .min_by_key(|(_, v)| v.0)
                    .map(|(k, _)| k.clone())
            {
                self.contents.remove(&old);
            }
            self.contents
                .insert(view.document.id.clone(), (Instant::now(), content));
        }
        result
    }
}

impl Context {
    fn read_sections(
        &mut self,
        location: &mail_core::MailUid,
        folder: &str,
    ) -> Result<MailContent, RpcError> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(fail("阅读已取消"));
        }
        // 旧完整MIME缓存保持离线可读；新读取优先结构与分段。
        if let Some(raw) = receive(self.store.message_body(location.clone())).map_err(fail)? {
            let content = mail_content::parse(&raw).map_err(fail)?;
            let _ = receive(
                self.store
                    .mark_attachments(location.clone(), content.attachments.len() as u32),
            );
            return Ok(content);
        }
        let structure = match receive(self.store.mime_structure(location.clone())).map_err(fail)? {
            Some(value) => value,
            None => {
                let (settings, credentials) = self
                    .credentials(location.account.clone())
                    .map_err(|e| fail(format!("未缓存，需要联网下载：{}", e.message)))?;
                let size = mail_service::parts::size(
                    &settings.imap,
                    &credentials.imap,
                    folder,
                    location.uidvalidity,
                    location.uid,
                )
                .map_err(fail)?;
                match mail_service::parts::structure(
                    &settings.imap,
                    &credentials.imap,
                    folder,
                    location.uidvalidity,
                    location.uid,
                    size,
                ) {
                    Ok(value) => {
                        receive(self.store.save_structure(location.clone(), value.clone()))
                            .map_err(fail)?;
                        value
                    }
                    Err(error) => {
                        if size > mail_content::MAX_MESSAGE_BYTES as u32 {
                            return Err(fail(format!(
                                "MIME结构异常（{error}）；邮件超过8MiB，未完整下载。可稍后重试"
                            )));
                        }
                        let raw = sync::message(
                            &settings.imap,
                            &credentials.imap,
                            folder,
                            location.uidvalidity,
                            location.uid,
                        )
                        .map_err(fail)?;
                        receive(self.store.save_message_body(location.clone(), raw.clone()))
                            .map_err(fail)?;
                        let mut content = mail_content::parse(&raw).map_err(fail)?;
                        content.warnings.push(
                            "MIME结构不可用，已在8MiB限制内回退完整邮件；附件可能已随回退读取"
                                .into(),
                        );
                        return Ok(content);
                    }
                }
            }
        };
        if self.cancel.load(Ordering::Acquire) {
            return Err(fail("阅读已取消"));
        }
        let mut selected = vec![];
        let mut warnings = vec![];
        for media in ["text/plain", "text/html"] {
            if let Some(part) = structure
                .parts
                .iter()
                .find(|p| p.media_type == media && !p.attachment)
            {
                match self.read_part(location, folder, part, mail_content::MAX_BODY_BYTES as u64) {
                    Ok(bytes) => selected.push((part.clone(), bytes)),
                    Err(e) => warnings.push(format!("正文分段未获取：{}", e.message)),
                }
            }
        }
        // 仅正文引用到的唯一CID图片自动获取，不遍历下载所有内嵌附件。
        let body_text = selected
            .iter()
            .map(|(_, b)| String::from_utf8_lossy(b))
            .collect::<Vec<_>>()
            .join("\n");
        let mut total = selected.iter().map(|(_, b)| b.len()).sum::<usize>();
        let mut count = 0;
        for part in &structure.parts {
            let Some(cid) = part
                .cid
                .as_deref()
                .map(|s| s.trim().trim_matches(['<', '>']))
            else {
                continue;
            };
            if !part.media_type.starts_with("image/") || !body_text.contains(&format!("cid:{cid}"))
            {
                continue;
            }
            if structure
                .parts
                .iter()
                .filter(|p| {
                    p.cid.as_deref().map(|s| s.trim().trim_matches(['<', '>'])) == Some(cid)
                })
                .count()
                != 1
            {
                warnings.push("重复CID已阻止".into());
                continue;
            }
            if count >= mail_content::MAX_IMAGES
                || part.encoded_size > 3 * 1024 * 1024
                || total >= 5 * 1024 * 1024
            {
                warnings.push("内嵌资源超过安全限制，未自动获取".into());
                continue;
            }
            match self.read_part(location, folder, part, mail_content::MAX_IMAGE_BYTES as u64) {
                Ok(bytes) if total + bytes.len() <= 5 * 1024 * 1024 => {
                    total += bytes.len();
                    count += 1;
                    selected.push((part.clone(), bytes));
                }
                _ => warnings.push("CID图片无法获取或超过安全限制".into()),
            }
        }
        if self.cancel.load(Ordering::Acquire) {
            return Err(fail("阅读已取消"));
        }
        let inputs = selected
            .iter()
            .map(|(p, b)| mail_content::SectionInput {
                media_type: &p.media_type,
                charset: p.charset.as_deref(),
                cid: p.cid.as_deref().map(|s| s.trim().trim_matches(['<', '>'])),
                bytes: b,
            })
            .collect::<Vec<_>>();
        let mut content =
            mail_content::parse_sections(&structure.headers, &inputs).map_err(fail)?;
        content.attachments.clear();
        for part in &structure.parts {
            if matches!(part.media_type.as_str(), "text/plain" | "text/html") && !part.attachment {
                continue;
            }
            let file =
                receive(self.store.part_file(location.clone(), part.clone())).map_err(fail)?;
            let resource = self
                .resources
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .lazy(
                    super::resources::PartSpec {
                        location: location.clone(),
                        folder: folder.into(),
                        part: part.clone(),
                    },
                    file,
                )?;
            content.attachments.push(mail_content::Attachment {
                name: part.name.clone().unwrap_or_else(|| "未命名附件".into()),
                media_type: part.media_type.clone(),
                content_id: part.cid.clone(),
                bytes: Arc::from([]),
                remote: Some(resource.id),
                declared_size: u64::from(part.encoded_size),
            });
        }
        if selected.is_empty() {
            warnings.push("没有可安全获取的正文；附件可单独下载".into());
        }
        content.warnings.extend(warnings);
        let _ = receive(
            self.store
                .mark_attachments(location.clone(), content.attachments.len() as u32),
        );
        Ok(content)
    }
    fn read_part(
        &self,
        location: &mail_core::MailUid,
        folder: &str,
        part: &mail_core::MimePart,
        max: u64,
    ) -> Result<Vec<u8>, RpcError> {
        use std::io::{Read, Seek};
        if let Some(file) =
            receive(self.store.part_file(location.clone(), part.clone())).map_err(fail)?
        {
            if file.metadata().map_err(fail)?.len() > max {
                return Err(fail("正文或图片超过安全限制"));
            }
            let mut bytes = vec![];
            file.take(max + 1).read_to_end(&mut bytes).map_err(fail)?;
            return Ok(bytes);
        }
        if u64::from(part.encoded_size) > max * 3 {
            return Err(fail("正文或CID超过安全获取限制"));
        }
        let (settings, credentials) = self
            .credentials(location.account.clone())
            .map_err(|e| fail(format!("未缓存，需要联网下载：{}", e.message)))?;
        let mut file = receive(self.store.part_temporary()).map_err(fail)?;
        mail_service::parts::fetch(
            &settings.imap,
            &credentials.imap,
            folder,
            location.uidvalidity,
            location.uid,
            part,
            &mut file,
            max,
            |_| !self.cancel.load(Ordering::Acquire),
        )
        .map_err(fail)?;
        if self.cancel.load(Ordering::Acquire) {
            return Err(fail("阅读已取消，未发布资源"));
        }
        file.sync_all().map_err(fail)?;
        file.rewind().map_err(fail)?;
        let mut bytes = vec![];
        file.by_ref()
            .take(max + 1)
            .read_to_end(&mut bytes)
            .map_err(fail)?;
        receive(self.store.save_part(location.clone(), part.clone(), file)).map_err(fail)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod parts_tests {
    use super::*;
    #[test]
    fn shutdown_cancels_reads_but_still_waits_for_other_business_work() {
        let root = tempfile::tempdir().unwrap();
        let mut storage =
            crate::storage::StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let read_id = "a".repeat(32);
        let write_id = "b".repeat(32);
        let image_id = "c".repeat(32);
        let read = Request::new(
            read_id.clone(),
            Command::Business {
                operation: BusinessOperation::Read {
                    location: mail_core::MailUid {
                        account: mail_core::AccountId("fixture".into()),
                        folder_id: "inbox".into(),
                        uidvalidity: 42,
                        uid: 7,
                    },
                    folder: "INBOX".into(),
                },
            },
        );
        let write = Request::new(
            write_id.clone(),
            Command::Business {
                operation: BusinessOperation::RemoveAccount {
                    account: mail_core::AccountId("fixture".into()),
                },
            },
        );
        let image = Request::new(
            image_id.clone(),
            Command::Business {
                operation: BusinessOperation::RemoteImages {
                    content: "fixture".into(),
                },
            },
        );
        let image_cancel = Arc::new(AtomicBool::new(false));
        let read_cancel = Arc::new(AtomicBool::new(false));
        let write_cancel = Arc::new(AtomicBool::new(false));
        let business = Business {
            queue: mpsc::sync_channel(1).0,
            results: Arc::new(Mutex::new(HashMap::from([
                (read_id.clone(), (read, Instant::now(), None)),
                (write_id.clone(), (write, Instant::now(), None)),
                (image_id.clone(), (image, Instant::now(), None)),
            ]))),
            cancellations: Arc::new(Mutex::new(HashMap::from([
                (read_id, read_cancel.clone()),
                (write_id.clone(), write_cancel.clone()),
                (image_id, image_cancel.clone()),
            ]))),
            storage: storage.client(),
            worker: None,
        };

        assert!(business.active());
        assert!(business.has_non_read_work());
        business.cancel_reads();
        assert!(read_cancel.load(Ordering::Acquire));
        assert!(image_cancel.load(Ordering::Acquire));
        assert!(!write_cancel.load(Ordering::Acquire));
        business.results.lock().unwrap().remove(&write_id);
        assert!(!business.has_non_read_work(), "only cancelled reads remain");

        drop(business);
        storage.stop();
    }
    #[test]
    fn cancelled_read_never_enters_body_fetch_or_publishes_resources() {
        let root = tempfile::tempdir().unwrap();
        let mut worker =
            crate::storage::StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let mut context = Context {
            store: worker.client(),
            enabled: false,
            cache: root.path().join("cache"),
            resources: Default::default(),
            contents: HashMap::new(),
            cancel: Arc::new(AtomicBool::new(true)),
        };
        let result = context.execute(BusinessOperation::Read {
            location: mail_core::MailUid {
                account: mail_core::AccountId("fixture".into()),
                folder_id: "inbox".into(),
                uidvalidity: 42,
                uid: 7,
            },
            folder: "INBOX".into(),
        });
        assert!(result.unwrap_err().message.contains("取消"));
        assert!(context.contents.is_empty());
        let result = context.execute(BusinessOperation::RemoteImages {
            content: "fixture".into(),
        });
        assert!(result.unwrap_err().message.contains("取消"));
        assert!(context.contents.is_empty());
        drop(context);
        worker.stop();
    }
}
