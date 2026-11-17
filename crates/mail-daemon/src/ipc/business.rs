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
        atomic::{AtomicU64, Ordering},
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
}
enum Work {
    Run(Box<Request>),
    Stop,
}
pub(super) struct Business {
    queue: mpsc::SyncSender<Work>,
    results: ResultMap,
    storage: StorageClient,
    worker: Option<thread::JoinHandle<()>>,
}
fn fail(e: impl ToString) -> RpcError {
    rpc_error(ErrorCode::Storage, e.to_string())
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
                };
                while let Ok(Work::Run(request)) = receiver.recv() {
                    let request = *request;
                    let Command::Business { operation } = &request.command else {
                        continue;
                    };
                    let mut result = context.execute(operation.clone());
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
                        BusinessOperation::RefreshCatalog { .. }
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
            storage,
            worker: Some(worker),
        })
    }
    pub fn active(&self) -> bool {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|(_, _, r)| r.is_none())
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
        results.insert(request.id.clone(), (request.clone(), Instant::now(), None));
        if self
            .queue
            .try_send(Work::Run(Box::new(request.clone())))
            .is_err()
        {
            results.remove(&request.id);
            let result = Err(rpc_error(ErrorCode::Busy, "后台队列繁忙，请稍后重试"));
            if operation.writes() {
                let _ = receive(self.storage.finish_mutation(request, result.clone()));
            }
            return result;
        }
        Ok(Reply::Pending { task: request.id })
    }
    pub fn stop(&mut self) {
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
        match operation {
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
                let raw = match receive(self.store.message_body(location.clone())).map_err(fail)? {
                    Some(raw) => raw,
                    None => {
                        let (settings, credentials) = self.credentials(location.account.clone())?;
                        let raw = sync::message(
                            &settings.imap,
                            &credentials.imap,
                            &folder,
                            location.uidvalidity,
                            location.uid,
                        )
                        .map_err(fail)?;
                        receive(self.store.save_message_body(location.clone(), raw))
                            .map_err(fail)?;
                        receive(self.store.message_body(location.clone()))
                            .map_err(fail)?
                            .ok_or_else(|| fail("正文缓存未确认"))?
                    }
                };
                let content = mail_content::parse(&raw).map_err(fail)?;
                let _ = receive(
                    self.store
                        .mark_attachments(location.clone(), content.attachments.len() as u32),
                );
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
                let record =
                    receive(self.store.task(account.clone(), task.clone())).map_err(fail)?;
                let mut remote = None;
                if self.enabled
                    && record.state == mail_core::SendState::Sent
                    && let Some(copy) = record.copy
                    && let Some(uid) = copy.uid
                    && let Ok((settings, credentials)) = self.credentials(account.clone())
                {
                    remote = sync::message(
                        &settings.imap,
                        &credentials.imap,
                        &copy.folder,
                        copy.uidvalidity,
                        uid,
                    )
                    .ok();
                }
                let raw = match remote {
                    Some(raw) => raw,
                    None => {
                        let snapshot =
                            receive(self.store.task_snapshot(account, task)).map_err(fail)?;
                        outgoing::prepare(&snapshot, true)
                            .map_err(fail)?
                            .raw()
                            .to_vec()
                    }
                };
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
                mail.load_remote_images_after_consent().map_err(fail)?;
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
        let mut resources = self.resources.lock().unwrap_or_else(|e| e.into_inner());
        let mut inserted = vec![];
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
                .map(|a| {
                    Ok(ReceivedAttachment {
                        name: a.name.clone(),
                        resource: put(a.bytes.clone(), a.media_type.clone())?,
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
