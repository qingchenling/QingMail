//! 进程内事件驱动的存储线程；SQLite 连接仅在此线程建立和使用。
use mail_content::draft::Draft;
use mail_core::{AccountId, DraftHeaders};
use mail_storage::{AppPaths, DraftDocument, Store, StoredDraft};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};

pub type StorageResult<T> = Result<T, String>;
pub type AccountList = Vec<(AccountId, mail_core::AccountSettings)>;

#[derive(Clone)]
pub struct StorageClient {
    queue: SyncSender<Request>,
}

pub struct StorageWorker {
    client: StorageClient,
    thread: Option<thread::JoinHandle<()>>,
}

enum Request {
    Open {
        initial: Draft,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    Save {
        previous: Box<StoredDraft>,
        body: Draft,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    New {
        body: Draft,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    List {
        reply: SyncSender<StorageResult<Vec<StoredDraft>>>,
    },
    Select {
        id: mail_core::DraftId,
        reply: SyncSender<StorageResult<StoredDraft>>,
    },
    AddAccount {
        settings: Box<mail_core::AccountSettings>,
        credentials: mail_service::credentials::Credentials,
        reply: SyncSender<StorageResult<AccountId>>,
    },
    Accounts {
        reply: SyncSender<StorageResult<AccountList>>,
    },
    Stop,
}

impl StorageWorker {
    /// None 使用正式 XDG data；显式路径供隔离验证，不从 UI 请求接收路径。
    pub fn start(path: Option<PathBuf>) -> std::io::Result<Self> {
        let (queue, receiver) = mpsc::sync_channel(16);
        let thread = thread::Builder::new()
            .name("qingmail-storage".into())
            .spawn(move || run(path, receiver))?;
        Ok(Self {
            client: StorageClient { queue },
            thread: Some(thread),
        })
    }
    pub fn client(&self) -> StorageClient {
        self.client.clone()
    }
    pub fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.client.queue.send(Request::Stop);
            let _ = thread.join();
        }
    }
}
impl Drop for StorageWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

impl StorageClient {
    fn request<T>(
        &self,
        build: impl FnOnce(SyncSender<StorageResult<T>>) -> Request,
    ) -> StorageResult<Receiver<StorageResult<T>>> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.queue
            .try_send(build(reply))
            .map_err(|_| "存储队列繁忙或后台已停止，请重试".to_owned())?;
        Ok(receive)
    }
    pub fn add_account(
        &self,
        settings: mail_core::AccountSettings,
        credentials: mail_service::credentials::Credentials,
    ) -> StorageResult<Receiver<StorageResult<AccountId>>> {
        self.request(|reply| Request::AddAccount {
            settings: Box::new(settings),
            credentials,
            reply,
        })
    }
    pub fn accounts(&self) -> StorageResult<Receiver<StorageResult<AccountList>>> {
        self.request(|reply| Request::Accounts { reply })
    }
    pub fn open_editor(
        &self,
        initial: Draft,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::Open { initial, reply })
    }
    pub fn save(
        &self,
        previous: StoredDraft,
        body: Draft,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::Save {
            previous: Box::new(previous),
            body,
            reply,
        })
    }
    pub fn new_draft(&self, body: Draft) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::New { body, reply })
    }
    pub fn list(&self) -> StorageResult<Receiver<StorageResult<Vec<StoredDraft>>>> {
        self.request(|reply| Request::List { reply })
    }
    pub fn select(
        &self,
        id: mail_core::DraftId,
    ) -> StorageResult<Receiver<StorageResult<StoredDraft>>> {
        self.request(|reply| Request::Select { id, reply })
    }
}

/// 在已有后台 future 中等待，不在 GPUI listener 内调用。
pub fn receive<T>(reply: StorageResult<Receiver<StorageResult<T>>>) -> StorageResult<T> {
    reply?
        .recv()
        .map_err(|_| "存储线程已停止，尚未收到保存确认".to_owned())?
}

fn run(path: Option<PathBuf>, receiver: Receiver<Request>) {
    let mut store: Option<Store> = None;
    for request in receiver {
        if matches!(request, Request::Stop) {
            break;
        }
        // 打开失败可以通过后续明确重试恢复；不自动丢弃坏库或覆盖原文件。
        let opened = (|| {
            if store.is_none() {
                let path = path
                    .clone()
                    .map(Ok)
                    .unwrap_or_else(|| AppPaths::discover().map(|p| p.data))
                    .map_err(|e| e.to_string())?;
                store = Some(Store::open(&path).map_err(|e| e.to_string())?);
            }
            Ok::<_, String>(())
        })();
        let opened = opened.map(|()| store.as_mut().expect("opened"));
        match request {
            Request::Open { initial, reply } => {
                let _ = reply.send(opened.and_then(|s| open(s, initial)));
            }
            Request::Save {
                previous,
                body,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    let mut document = previous.document;
                    document.body = body;
                    s.save_draft(
                        &previous.account,
                        &previous.id,
                        previous.revision,
                        &document,
                    )
                    .map_err(|e| e.to_string())
                }));
            }
            Request::New { body, reply } => {
                let _ = reply.send(opened.and_then(|s| new(s, body)));
            }
            Request::List { reply } => {
                let _ = reply.send(opened.and_then(|s| {
                    let account = s.local_draft_account().map_err(|e| e.to_string())?;
                    s.draft_ids(&account)
                        .map_err(|e| e.to_string())?
                        .iter()
                        .map(|id| s.draft(&account, id).map_err(|e| e.to_string()))
                        .collect()
                }));
            }
            Request::Select { id, reply } => {
                let _ = reply.send(opened.and_then(|s| {
                    let account = s.local_draft_account().map_err(|e| e.to_string())?;
                    let draft = s.draft(&account, &id).map_err(|e| e.to_string())?;
                    s.select_local_draft(&account, &id)
                        .map_err(|e| e.to_string())?;
                    Ok(draft)
                }));
            }
            Request::Accounts { reply } => {
                let _ = reply.send(opened.and_then(|s| s.accounts().map_err(|e| e.to_string())));
            }
            Request::AddAccount {
                settings,
                credentials,
                reply,
            } => {
                let _ = reply.send(opened.and_then(|s| {
                    persist_account(
                        s,
                        &mail_service::credentials::SystemVault,
                        &settings,
                        &credentials,
                    )
                }));
            }
            Request::Stop => break,
        }
    }
}
fn open(store: &mut Store, initial: Draft) -> StorageResult<StoredDraft> {
    let account = store.local_draft_account().map_err(|e| e.to_string())?;
    if let Some(id) = store
        .selected_local_draft(&account)
        .map_err(|e| e.to_string())?
    {
        return store.draft(&account, &id).map_err(|e| e.to_string());
    }
    // 上次可能在创建草稿后、选中前退出；优先恢复已存在草稿，不反复创建。
    if let Some(id) = store
        .draft_ids(&account)
        .map_err(|e| e.to_string())?
        .first()
    {
        let draft = store.draft(&account, id).map_err(|e| e.to_string())?;
        store
            .select_local_draft(&account, id)
            .map_err(|e| e.to_string())?;
        return Ok(draft);
    }
    create(store, &account, initial)
}
fn new(store: &mut Store, body: Draft) -> StorageResult<StoredDraft> {
    let account = store.local_draft_account().map_err(|e| e.to_string())?;
    create(store, &account, body)
}
fn create(store: &mut Store, account: &AccountId, body: Draft) -> StorageResult<StoredDraft> {
    let draft = store
        .create_draft(
            account,
            &DraftDocument {
                headers: DraftHeaders::default(),
                body,
                attachments: vec![],
            },
        )
        .map_err(|e| e.to_string())?;
    store
        .select_local_draft(account, &draft.id)
        .map_err(|e| e.to_string())?;
    Ok(draft)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_preserves_revision_order_switches_and_reopens_selected_draft() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        let client = worker.client();
        let first = receive(client.open_editor(Draft::plain("first🙂").unwrap())).unwrap();
        let update = receive(client.save(first.clone(), Draft::plain("saved🙂").unwrap())).unwrap();
        assert!(receive(client.save(first.clone(), Draft::plain("stale").unwrap())).is_err());
        let second = receive(client.new_draft(Draft::plain("second").unwrap())).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(receive(client.list()).unwrap().len(), 2);
        assert_eq!(receive(client.select(first.id)).unwrap(), update);
        worker.stop();
        assert!(client.list().is_err());
        let mut worker = StorageWorker::start(Some(path)).unwrap();
        assert_eq!(
            receive(
                worker
                    .client()
                    .open_editor(Draft::plain("must not replace").unwrap())
            )
            .unwrap(),
            update
        );
        worker.stop();
    }

    #[test]
    fn dropped_reply_does_not_block_worker_or_lose_accepted_save() {
        let root = tempfile::tempdir().unwrap();
        let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let client = worker.client();
        let draft = receive(client.open_editor(Draft::plain("old").unwrap())).unwrap();
        drop(client.save(draft, Draft::plain("new").unwrap()));
        assert_eq!(
            receive(client.list()).unwrap()[0].document.body.text(),
            "new"
        );
        worker.stop();
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    #[test]
    fn failed_open_can_retry_without_discarding_database() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let mut worker = StorageWorker::start(Some(path.clone())).unwrap();
        let client = worker.client();
        assert!(receive(client.open_editor(Draft::plain("initial").unwrap())).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let draft = receive(client.open_editor(Draft::plain("initial").unwrap())).unwrap();
        assert_eq!(draft.document.body.text(), "initial");
        worker.stop();
    }
}

fn persist_account(
    store: &mut Store,
    vault: &impl mail_service::credentials::CredentialVault,
    settings: &mail_core::AccountSettings,
    credentials: &mail_service::credentials::Credentials,
) -> StorageResult<AccountId> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| "无法生成账号 ID".to_owned())?;
    let id = AccountId(format!("{:032x}", u128::from_be_bytes(random)));
    persist_account_with_id(store, vault, settings, credentials, id)
}
fn persist_account_with_id(
    store: &mut Store,
    vault: &impl mail_service::credentials::CredentialVault,
    settings: &mail_core::AccountSettings,
    credentials: &mail_service::credentials::Credentials,
    id: AccountId,
) -> StorageResult<AccountId> {
    mail_service::accounts::validate(settings).map_err(|e| e.to_string())?;
    if !store.accounts().map_err(|e| e.to_string())?.is_empty() {
        return Err("P2 当前仅支持一个远程账号".into());
    }
    vault.save(&id, credentials).map_err(|e| match e {
        mail_service::credentials::VaultError::OperationFailed
        | mail_service::credentials::VaultError::Timeout => {
            format!("{e}；账号未写入，密钥库可能留有凭据项")
        }
        _ => e.to_string(),
    })?;
    if let Err(e) = store.insert_account(&id, settings) {
        // 提交结果可能不确定；只有确认账号不存在才删密钥。
        match store.account(&id) {
            Ok(None) => {
                if vault.delete(&id).is_err() {
                    return Err("账号保存失败，密钥库可能保留孤立凭据，请检查后重试".into());
                }
            }
            _ => return Err("账号保存结果需重新检查，凭据已保留，请刷新账号列表".into()),
        }
        return Err(e.to_string());
    }
    Ok(id)
}

#[cfg(test)]
mod account_tests {
    use super::*;
    use mail_core::{AccountSettings, ConnectionSecurity, ServerSettings};
    use mail_service::credentials::{CredentialVault, Credentials, VaultError};
    use std::cell::Cell;
    struct Vault {
        error: Option<VaultError>,
        saves: Cell<usize>,
        deletes: Cell<usize>,
    }
    impl CredentialVault for Vault {
        fn status(&self) -> Result<(), VaultError> {
            self.error.map_or(Ok(()), Err)
        }
        fn save(&self, _: &AccountId, _: &Credentials) -> Result<(), VaultError> {
            self.saves.set(self.saves.get() + 1);
            self.status()
        }
        fn load(&self, _: &AccountId) -> Result<Credentials, VaultError> {
            Err(VaultError::Missing)
        }
        fn delete(&self, _: &AccountId) -> Result<(), VaultError> {
            self.deletes.set(self.deletes.get() + 1);
            Ok(())
        }
    }
    fn settings() -> AccountSettings {
        let server = ServerSettings {
            host: "example.invalid".into(),
            port: 993,
            username: "synthetic".into(),
            security: ConnectionSecurity::Tls,
        };
        AccountSettings {
            display_name: "测试".into(),
            address: "test@example.invalid".into(),
            imap: server.clone(),
            smtp: server,
        }
    }
    #[test]
    fn unavailable_locked_and_unknown_vault_results_never_insert_account() {
        for error in [
            VaultError::Unavailable,
            VaultError::Locked,
            VaultError::Missing,
            VaultError::OperationFailed,
            VaultError::Timeout,
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut store = Store::open(&root.path().join("data")).unwrap();
            let vault = Vault {
                error: Some(error),
                saves: Cell::new(0),
                deletes: Cell::new(0),
            };
            assert!(
                persist_account(
                    &mut store,
                    &vault,
                    &settings(),
                    &Credentials::new("synthetic-imap".into(), "synthetic-smtp".into()).unwrap()
                )
                .is_err()
            );
            assert!(store.accounts().unwrap().is_empty());
            assert_eq!(vault.saves.get(), 1);
            assert_eq!(vault.deletes.get(), 0);
        }
    }
    #[test]
    fn confirmed_vault_save_precedes_account_and_second_account_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let vault = Vault {
            error: None,
            saves: Cell::new(0),
            deletes: Cell::new(0),
        };
        let c = Credentials::new("synthetic-imap".into(), "synthetic-smtp".into()).unwrap();
        let id = persist_account(&mut store, &vault, &settings(), &c).unwrap();
        assert_eq!(store.account(&id).unwrap(), Some(settings()));
        assert!(persist_account(&mut store, &vault, &settings(), &c).is_err());
        assert_eq!(vault.saves.get(), 1);
        drop(store);
        let store = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(store.accounts().unwrap().len(), 1);
        // 真实 SQLite 字节不能含任何密码。
        let bytes = std::fs::read(root.path().join("data/state.sqlite3")).unwrap();
        assert!(!bytes.windows(c.imap.len()).any(|w| w == c.imap.as_bytes()));
        assert!(!bytes.windows(c.smtp.len()).any(|w| w == c.smtp.as_bytes()));
    }
    #[test]
    fn failed_database_insert_cleans_only_confirmed_absent_remote_account() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let id = store.local_draft_account().unwrap();
        let vault = Vault {
            error: None,
            saves: Cell::new(0),
            deletes: Cell::new(0),
        };
        assert!(
            persist_account_with_id(
                &mut store,
                &vault,
                &settings(),
                &Credentials::new("imap".into(), "smtp".into()).unwrap(),
                id
            )
            .is_err()
        );
        assert!(store.accounts().unwrap().is_empty());
        assert_eq!(vault.saves.get(), 1);
        assert_eq!(vault.deletes.get(), 1);
    }
}
