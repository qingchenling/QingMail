//! IMAP草稿上传与SMTP完全分离；持久固定MIME，恢复只查证，绝不自动重发。
use crate::storage::{StorageClient, receive};
use mail_protocol::{Command, Reply};
use mail_service::{
    credentials::{CredentialVault, SystemVault},
    server_drafts::{self, UploadResult},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
pub(crate) struct DraftSync {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl DraftSync {
    pub fn start(
        store: StorageClient,
        enabled: bool,
        paused: Arc<AtomicBool>,
        revision: Arc<AtomicU64>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let thread = if enabled {
            Some(
                thread::Builder::new()
                    .name("qingmail-draft-sync".into())
                    .spawn(move || {
                        while !signal.load(Ordering::Acquire) {
                            if !paused.load(Ordering::Acquire)
                                && let Ok(Some(candidate)) = receive(store.next_draft_upload())
                            {
                                let account = candidate.draft.account.clone();
                                let id = candidate.draft.id.clone();
                                let _permit = crate::network_gate::acquire(&account, false);
                                if signal.load(Ordering::Acquire) {
                                    break;
                                }
                                if let Err((state, detail)) = execute(&store, candidate) {
                                    let _ = receive(store.fail_draft_sync(
                                        account,
                                        id,
                                        state.into(),
                                        detail,
                                    ));
                                }
                                revision.fetch_add(1, Ordering::Release);
                            } else if !paused.load(Ordering::Acquire)
                                && let Ok(Some(refresh)) = receive(store.next_draft_refresh())
                            {
                                let _permit =
                                    crate::network_gate::acquire(&refresh.draft.account, false);
                                if signal.load(Ordering::Acquire) {
                                    break;
                                }
                                if matches!(refresh_remote(&store, refresh), Ok(true)) {
                                    revision.fetch_add(1, Ordering::Release);
                                }
                            } else {
                                thread::park_timeout(Duration::from_millis(500));
                            }
                        }
                    })?,
            )
        } else {
            None
        };
        Ok(Self { stop, thread })
    }
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}
impl Drop for DraftSync {
    fn drop(&mut self) {
        self.stop();
    }
}
type Failure = (&'static str, String);
fn pending(e: impl std::fmt::Display) -> Failure {
    ("pending", e.to_string())
}
fn failed(e: impl std::fmt::Display) -> Failure {
    ("failed", e.to_string())
}
fn execute(
    store: &StorageClient,
    candidate: mail_storage::DraftUploadCandidate,
) -> Result<(), Failure> {
    let account = &candidate.draft.account.clone();
    let baseline = receive(store.account_version(account.clone())).map_err(pending)?;
    if matches!(store.control(Command::AccountOptions),Ok(Reply::AccountOptions{paused,..}) if paused.contains(account))
    {
        return Err(pending("账号已暂停"));
    }
    let credentials = SystemVault
        .load(&baseline.credential_key)
        .map_err(pending)?;
    if candidate.previous.as_ref().is_none_or(|u| !u.probe_only)
        && let Some(source) = &candidate.source
    {
        if source.folder != candidate.folder {
            return Err((
                "conflict",
                "草稿目录已变化；保留原件和本地副本，请核对".into(),
            ));
        }
        let raw = mail_service::sync::message(
            &baseline.settings.imap,
            &credentials.imap,
            &source.folder,
            source.uidvalidity,
            source.uid,
        )
        .map_err(|e| match e {
            mail_service::accounts::CheckError::EpochChanged
            | mail_service::accounts::CheckError::Protocol => (
                "conflict",
                format!("远端版本缺失或定位变化，保留本地副本：{e}"),
            ),
            _ => pending(e),
        })?;
        let expected = receive(store.server_draft_original(account.clone(), source.id.clone()))
            .map_err(failed)?;
        if raw != expected {
            receive(store.archive_server_draft(
                account.clone(),
                source.folder.clone(),
                source.uidvalidity,
                source.uid,
                raw,
            ))
            .map_err(failed)?;
            return Err((
                "conflict",
                "远端和本地版本不再共享基线，已保留两份原件，未上传".into(),
            ));
        }
    }
    let upload = match candidate.previous {
        Some(upload) => upload,
        None => {
            let id = mail_protocol::transport::request_id().map_err(failed)?;
            let mut snapshot =
                receive(store.draft_sync_snapshot(candidate.draft.clone())).map_err(pending)?;
            snapshot.message_id = format!("<{id}@qingmail.invalid>");
            let model = candidate.draft.document.body.to_json().map_err(failed)?;
            let raw = mail_service::outgoing::prepare_draft(&snapshot, &model).map_err(failed)?;
            if raw.len() > mail_content::MAX_MESSAGE_BYTES {
                return Err(failed(
                    "服务器草稿MIME超过当前8MiB原件边界，已保留本地草稿；不会截断上传",
                ));
            }
            receive(store.prepare_draft_upload(
                candidate.draft,
                candidate.folder,
                candidate.source.as_ref().map(|s| s.uidvalidity),
                id,
                raw,
            ))
            .map_err(pending)?
        }
    };
    if receive(store.account_version(account.clone())).map_err(pending)? != baseline {
        return Err(("conflict", "账号配置已变化，未上传旧服务器快照".into()));
    }
    let current = match store
        .control(Command::ServerDrafts {
            account: account.clone(),
        })
        .map_err(pending)?
    {
        Reply::ServerDrafts { folder, .. } => folder,
        _ => None,
    };
    if current.as_deref() != Some(&upload.folder) {
        return Err(("conflict", "草稿目录已变化，上传已暂停".into()));
    }
    let probe_only =
        receive(store.begin_draft_upload(account.clone(), upload.id.clone())).map_err(pending)?;
    let message_id = format!("<{}@qingmail.invalid>", upload.id);
    match server_drafts::upload(
        &baseline.settings.imap,
        &credentials.imap,
        &upload.folder,
        upload.expected_epoch,
        &message_id,
        &upload.raw,
        probe_only,
    ) {
        Ok(UploadResult::Verified { uidvalidity, uid }) => {
            if receive(store.account_version(account.clone())).map_err(pending)? != baseline {
                return Err((
                    "uncertain",
                    "账号配置已变化，请核对服务器；不会重新APPEND".into(),
                ));
            }
            receive(store.complete_draft_upload(upload, uidvalidity, uid))
                .map_err(|e| ("uncertain", e))?;
            Ok(())
        }
        Ok(UploadResult::EpochChanged) => Err((
            "conflict",
            "服务器草稿目录UIDVALIDITY已变化，保留本地与原件，未APPEND".into(),
        )),
        Ok(UploadResult::Uncertain) => Err((
            "uncertain",
            "上传结果尚未查证；只查询原标识，禁止自动重复APPEND".into(),
        )),
        Err(e) => {
            if !probe_only && !matches!(e, mail_service::sent::CopyError::ResultUnknown) {
                receive(store.defer_draft_upload(account.clone(), upload.id.clone()))
                    .map_err(pending)?;
                if e == mail_service::sent::CopyError::Authentication {
                    let _ = store.control(Command::PauseSync {
                        account: Some(account.clone()),
                        paused: true,
                    });
                }
                return Err(pending(format!(
                    "服务器尚未接受APPEND，保留上传意图稍后重试：{}",
                    e.code()
                )));
            }
            if e == mail_service::sent::CopyError::Authentication {
                let _ = store.control(Command::PauseSync {
                    account: Some(account.clone()),
                    paused: true,
                });
            }
            Err((
                "uncertain",
                format!(
                    "草稿上传未核验（{}）；保留固定快照，只查询不重复APPEND",
                    e.code()
                ),
            ))
        }
    }
}

/// 仅检查已知完整定位，不根据最新200条窗口推断删除，也不按Message-ID猜关联。
fn refresh_remote(
    store: &StorageClient,
    refresh: mail_storage::DraftRefresh,
) -> Result<bool, String> {
    let account = refresh.draft.account.clone();
    if matches!(store.control(Command::AccountOptions), Ok(Reply::AccountOptions {paused,..}) if paused.contains(&account))
    {
        return Ok(false);
    }
    let baseline = refresh.version.clone();
    let credentials = SystemVault
        .load(&baseline.credential_key)
        .map_err(|e| e.to_string())?;
    let source = &refresh.source;
    let raw = match mail_service::sync::message(
        &baseline.settings.imap,
        &credentials.imap,
        &source.folder,
        source.uidvalidity,
        source.uid,
    ) {
        Ok(raw) => Some(raw),
        Err(
            mail_service::accounts::CheckError::EpochChanged
            | mail_service::accounts::CheckError::Protocol,
        ) => None,
        Err(e) => {
            if e == mail_service::accounts::CheckError::Authentication {
                let _ = store.control(Command::PauseSync {
                    account: Some(account),
                    paused: true,
                });
            }
            return Err(e.to_string());
        }
    };
    if receive(store.account_version(account))? != baseline {
        return Ok(false);
    }
    receive(store.observe_draft_remote(refresh, raw))
}
