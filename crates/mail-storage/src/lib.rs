//! 本地索引与 P3 IPC 幂等记录：后台单写者、XDG 路径、事务迁移和 IMAP 定位隔离。
//! 不保存密码/令牌；草稿与发送快照使用不可变对象和事务关联。
mod account_credentials;
mod cache;
mod desktop;
pub use desktop::NotificationBatch;
mod client_requests;
pub use client_requests::{ClientRequests, PendingAccountRequest};
mod draft_reconcile;
mod draft_refresh;
mod draft_uploads;
mod message_body;
mod multi_account;
mod mutations;
mod operations;
mod parts;
mod server_drafts;
pub use account_credentials::AccountVersion;
pub use draft_refresh::DraftRefresh;
pub use draft_uploads::{DraftUpload, DraftUploadCandidate};
pub use mutations::MutationClaim;
mod objects;
mod outbox;
mod paths;
mod sync;
pub use outbox::{
    AttachmentInfo, CopyOutcome, CopyReady, DraftDocument, MAX_ATTACHMENT_BYTES,
    OutgoingAttachment, OutgoingSnapshot, SendTask, SentCopy, SmtpAttempt, StoredDraft,
};
mod private_dir;

pub use paths::AppPaths;

pub use mail_protocol::{AccountImpact, SearchHit, SearchQuery};

use mail_core::{
    AccountId, AccountSettings, CachedMessage, ConnectionSecurity, Folder, FolderId, MailUid,
    MessageEnvelope, MessageId, ServerSettings,
};
use private_dir::PrivateDir;
use rusqlite::{Connection, MAIN_DB, OpenFlags, OptionalExtension, params};
use std::{fmt, fs::File, io, path::Path, time::Duration};

const SCHEMA_VERSION: u32 = 18;
const APPLICATION_ID: u32 = 0x514d4149;
const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_V2: &str = include_str!("schema-v2.sql");
const SCHEMA_V3: &str = include_str!("schema-v3.sql");

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Sql(rusqlite::Error),
    Invalid(&'static str),
    FutureSchema(u32),
    Conflict,
}
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "本地存储 I/O 失败：{e}"),
            Self::Sql(e) => write!(f, "本地数据库失败：{e}"),
            Self::Invalid(message) => f.write_str(message),
            Self::Conflict => f.write_str("版本或任务状态已变化，请重新加载"),
            Self::FutureSchema(version) => {
                write!(f, "数据库版本 {version} 较新，请使用新版 QingMail")
            }
        }
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        Self::Io(e.into())
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

/// 由后台模块持有。每个数据目录只有一个写者，不向 UI 暴露数据库连接。
/// SQLite 在连接销毁后才释放目录和写锁。
pub struct Store {
    connection: Connection,
    _writer: File,
    _directory: PrivateDir,
    objects: PrivateDir,
}

impl Store {
    pub fn open_default() -> Result<Self> {
        Self::open(&AppPaths::discover()?.data)
    }

    /// 显式目录便于隔离验证；不得从不可信前台请求直接接收任意路径。
    pub fn open(data_directory: &Path) -> Result<Self> {
        let directory = PrivateDir::open(data_directory)?;
        let writer = directory.lock()?;
        directory.check_sidecars()?;
        let file = directory.file("state.sqlite3", false)?;
        let existed = file.metadata()?.len() != 0;
        let mut connection = Connection::open_with_flags(
            directory.path("state.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(2))?;
        validate_schema(&connection)?;
        let check: String = connection.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(Error::Invalid("数据库完整性检查失败，已保留原文件"));
        }
        connection.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA trusted_schema=OFF;",
        )?;
        migrate(
            &mut connection,
            &directory,
            existed,
            SCHEMA,
            SCHEMA_V2,
            SCHEMA_V3,
        )?;
        let mut foreign_keys = connection.prepare("PRAGMA foreign_key_check")?;
        if foreign_keys.query([])?.next()?.is_some() {
            return Err(Error::Invalid("数据库关联检查失败，已保留原文件"));
        }
        drop(foreign_keys);
        file.sync_all()?;
        directory.0.sync_all()?;
        let objects = directory.child("objects")?;
        outbox::recover_interrupted(&mut connection)?;
        mutations::recover(&connection)?;
        operations::recover(&connection)?;
        let mut store = Self {
            objects,
            connection,
            _writer: writer,
            _directory: directory,
        };
        store.recover_cache_files()?;
        store.trim_cache(false)?;
        Ok(store)
    }

    /// 本地草稿空间不含可用服务器或凭据；account()/accounts() 不暴露为真实账号。
    pub fn local_draft_account(&mut self) -> Result<AccountId> {
        if let Some(id) = self
            .connection
            .query_row("SELECT id FROM accounts WHERE kind='local'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        {
            return Ok(AccountId(id));
        }
        let id = AccountId(new_id()?);
        self.connection.execute(
            "INSERT INTO accounts VALUES (?1,'本地草稿','','',1,'','tls','',1,'','tls','local')",
            [&id.0],
        )?;
        Ok(id)
    }

    pub fn selected_local_draft(&self, account: &AccountId) -> Result<Option<mail_core::DraftId>> {
        Ok(self
            .connection
            .query_row(
                "SELECT draft_id FROM local_editor WHERE account_id=?1",
                [&account.0],
                |r| Ok(mail_core::DraftId(r.get(0)?)),
            )
            .optional()?)
    }

    pub fn select_local_draft(
        &mut self,
        account: &AccountId,
        id: &mail_core::DraftId,
    ) -> Result<()> {
        let local: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE id=?1 AND kind='local')",
            [&account.0],
            |r| r.get(0),
        )?;
        if !local {
            return Err(Error::Invalid("只能在本地草稿空间中选择"));
        }
        self.select_editor_draft(account, id)
    }

    /// P2单编辑窗口记忆当前草稿；复用外键保证账号与草稿一致。
    pub fn select_editor_draft(
        &mut self,
        account: &AccountId,
        id: &mail_core::DraftId,
    ) -> Result<()> {
        self.connection.execute("INSERT INTO local_editor VALUES (1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET draft_id=excluded.draft_id,account_id=excluded.account_id",params![id.0,account.0])?;
        Ok(())
    }

    pub fn create_account(&mut self, settings: &AccountSettings) -> Result<AccountId> {
        let id = AccountId(new_id()?);
        self.insert_account(&id, settings)?;
        Ok(id)
    }

    pub fn insert_account(&mut self, id: &AccountId, settings: &AccountSettings) -> Result<()> {
        self.insert_account_row(id, settings)
    }
    fn insert_account_row(&self, id: &AccountId, settings: &AccountSettings) -> Result<()> {
        validate_settings(settings)?;
        if id.0.len() != 32 || !id.0.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Invalid("无效账号 ID"));
        }
        self.connection.execute(
            "INSERT INTO accounts(id,display_name,address,imap_host,imap_port,imap_username,imap_security,smtp_host,smtp_port,smtp_username,smtp_security) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                id.0,
                settings.display_name,
                settings.address,
                settings.imap.host,
                settings.imap.port,
                settings.imap.username,
                security_name(settings.imap.security),
                settings.smtp.host,
                settings.smtp.port,
                settings.smtp.username,
                security_name(settings.smtp.security),
            ],
        )?;
        Ok(())
    }

    pub fn account(&self, id: &AccountId) -> Result<Option<AccountSettings>> {
        Ok(self.connection.query_row(
            "SELECT display_name,address,imap_host,imap_port,imap_username,imap_security,smtp_host,smtp_port,smtp_username,smtp_security FROM accounts WHERE id=?1 AND kind='remote'",
            [&id.0],
            |r| Ok(AccountSettings {
                display_name: r.get(0)?, address: r.get(1)?,
                imap: ServerSettings { host:r.get(2)?, port:r.get(3)?, username:r.get(4)?, security:read_security(r.get(5)?)? },
                smtp: ServerSettings { host:r.get(6)?, port:r.get(7)?, username:r.get(8)?, security:read_security(r.get(9)?)? },
            }),
        ).optional()?)
    }

    /// 修改同一账号的连接配置；账号 ID 与本地数据保持不变。
    pub fn update_account(&mut self, id: &AccountId, settings: &AccountSettings) -> Result<()> {
        self.update_account_row(id, settings)
    }
    fn update_account_row(&self, id: &AccountId, settings: &AccountSettings) -> Result<()> {
        validate_settings(settings)?;
        let changed = self.connection.execute(
            "UPDATE accounts SET display_name=?1,address=?2,imap_host=?3,imap_port=?4,imap_username=?5,imap_security=?6,smtp_host=?7,smtp_port=?8,smtp_username=?9,smtp_security=?10 WHERE id=?11 AND kind='remote'",
            params![
                settings.display_name,
                settings.address,
                settings.imap.host,
                settings.imap.port,
                settings.imap.username,
                security_name(settings.imap.security),
                settings.smtp.host,
                settings.smtp.port,
                settings.smtp.username,
                security_name(settings.smtp.security),
                id.0,
            ],
        )?;
        if changed != 1 {
            return Err(Error::Invalid("账号不存在或不是远端账号"));
        }
        Ok(())
    }

    /// 统计本地移除会影响的数据；远端邮件不在统计内。
    pub fn account_impact(&self, id: &AccountId) -> Result<AccountImpact> {
        let count = |sql: &str| -> Result<i64> {
            Ok(self.connection.query_row(sql, [&id.0], |row| row.get(0))?)
        };
        Ok(AccountImpact {
            cached_messages: count("SELECT COUNT(*) FROM folder_messages WHERE account_id=?1")?
                as u64,
            drafts: count("SELECT COUNT(*) FROM drafts WHERE account_id=?1")? as u64,
            tasks: count("SELECT COUNT(*) FROM send_tasks WHERE account_id=?1")? as u64,
            pending_tasks: count(
                "SELECT COUNT(*) FROM send_tasks WHERE account_id=?1 AND state IN ('pending','sending','copy_pending')",
            )? as u64,
            object_bytes: count("SELECT COALESCE(SUM(size),0) FROM objects WHERE account_id=?1")?
                as u64,
        })
    }

    /// 只移除本地账号、缓存、草稿与任务；不连接远端、不删除远端邮件。
    /// 对象文件在事务提交后删除，提交失败不会先丢文件。
    pub fn remove_account(&mut self, id: &AccountId) -> Result<AccountImpact> {
        if self.account(id)?.is_none() {
            return Err(Error::Invalid("账号不存在或不是远端账号"));
        }
        let impact = self.account_impact(id)?;
        let objects = {
            let mut query = self
                .connection
                .prepare("SELECT id FROM objects WHERE account_id=?1")?;
            query
                .query_map([&id.0], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let tx = self.connection.transaction()?;
        for sql in [
            "DELETE FROM task_attachments WHERE account_id=?1",
            "DELETE FROM send_tasks WHERE account_id=?1",
            "DELETE FROM draft_attachments WHERE account_id=?1",
            "DELETE FROM local_editor WHERE account_id=?1",
            "DELETE FROM draft_conflicts WHERE account_id=?1",
            "DELETE FROM draft_scan_failures WHERE account_id=?1",
            "DELETE FROM draft_scans WHERE account_id=?1",
            "DELETE FROM draft_uploads WHERE account_id=?1",
            "DELETE FROM draft_sync WHERE account_id=?1",
            "DELETE FROM server_draft_sources WHERE account_id=?1",
            "DELETE FROM server_draft_folders WHERE account_id=?1",
            "DELETE FROM drafts WHERE account_id=?1",
            "DELETE FROM part_resources WHERE account_id=?1",
            "DELETE FROM message_structures WHERE account_id=?1",
            "DELETE FROM message_bodies WHERE account_id=?1",
            "DELETE FROM folder_messages WHERE account_id=?1",
            "DELETE FROM folder_sync WHERE folder_id IN (SELECT id FROM folders WHERE account_id=?1)",
            "DELETE FROM folders WHERE account_id=?1",
            "DELETE FROM messages WHERE account_id=?1",
            "DELETE FROM attachments WHERE account_id=?1",
            "DELETE FROM objects WHERE account_id=?1",
            "DELETE FROM remote_folders WHERE account_id=?1",
            "DELETE FROM catalog_sync WHERE account_id=?1",
            "DELETE FROM accounts WHERE id=?1",
        ] {
            tx.execute(sql, [&id.0])?;
        }
        tx.execute("UPDATE account_options SET is_default=1 WHERE account_id=(SELECT id FROM accounts WHERE kind='remote' ORDER BY id LIMIT 1) AND NOT EXISTS(SELECT 1 FROM account_options WHERE is_default=1)", [])?;
        tx.commit()?;
        for object in objects {
            self.objects.remove(&object);
        }
        Ok(impact)
    }

    /// 启动时枚举持久化账号，不依赖调用者记住上次进程中的 ID。
    pub fn accounts(&self) -> Result<Vec<(AccountId, AccountSettings)>> {
        let mut query = self
            .connection
            .prepare("SELECT id FROM accounts WHERE kind='remote' ORDER BY id")?;
        let ids = query
            .query_map([], |r| Ok(AccountId(r.get(0)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                let settings = self.account(&id)?.ok_or(Error::Invalid("账号索引不完整"))?;
                Ok((id, settings))
            })
            .collect()
    }

    /// UIDVALIDITY 改变时在同一事务中清空该文件夹关联；其他文件夹不受影响。
    /// 保留旧邮件对象用于将来的草稿/任务引用清理策略，不误当有效 UID 继续操作。
    pub fn upsert_folder(
        &mut self,
        account: &AccountId,
        remote_name: &str,
        uidvalidity: u32,
    ) -> Result<Folder> {
        bounded(remote_name, 4096)?;
        if uidvalidity == 0 {
            return Err(Error::Invalid("UIDVALIDITY 必须非零"));
        }
        let tx = self.connection.transaction()?;
        let existing: Option<(String, u32)> = tx
            .query_row(
                "SELECT id,uidvalidity FROM folders WHERE account_id=?1 AND remote_name=?2",
                params![account.0, remote_name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let id = if let Some((id, epoch)) = existing {
            if epoch != uidvalidity {
                tx.execute(
                    "DELETE FROM folder_messages WHERE account_id=?1 AND folder_id=?2",
                    params![account.0, id],
                )?;
                tx.execute(
                    "UPDATE folders SET uidvalidity=?1 WHERE id=?2",
                    params![uidvalidity, id],
                )?;
            }
            id
        } else {
            let id = new_id()?;
            tx.execute(
                "INSERT INTO folders VALUES (?1,?2,?3,?4)",
                params![id, account.0, remote_name, uidvalidity],
            )?;
            id
        };
        tx.commit()?;
        Ok(Folder {
            id: FolderId(id),
            account: account.clone(),
            remote_name: remote_name.into(),
            uidvalidity,
        })
    }

    pub fn folders(&self, account: &AccountId) -> Result<Vec<Folder>> {
        let mut query = self.connection.prepare("SELECT id,remote_name,uidvalidity FROM folders WHERE account_id=?1 ORDER BY remote_name")?;
        Ok(query
            .query_map([&account.0], |r| {
                Ok(Folder {
                    id: FolderId(r.get(0)?),
                    account: account.clone(),
                    remote_name: r.get(1)?,
                    uidvalidity: r.get(2)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?)
    }

    /// 重复同步同一四元定位更新已有对象，不按 Message-ID 跨账号去重。
    pub fn cache_message(
        &mut self,
        location: &MailUid,
        envelope: &MessageEnvelope,
    ) -> Result<MessageId> {
        if location.uid == 0 || location.uidvalidity == 0 {
            return Err(Error::Invalid("UID 与 UIDVALIDITY 必须非零"));
        }
        if envelope.subject.len() > 64 * 1024 || envelope.sender.len() > 64 * 1024 {
            return Err(Error::Invalid("邮件索引字段超过上限"));
        }
        let tx = self.connection.transaction()?;
        let existing: Option<String> = tx.query_row(
            "SELECT message_id FROM folder_messages WHERE account_id=?1 AND folder_id=?2 AND uidvalidity=?3 AND uid=?4",
            params![location.account.0,location.folder_id,location.uidvalidity,location.uid], |r| r.get(0),
        ).optional()?;
        let id = match existing {
            Some(id) => id,
            None => new_id()?,
        };
        tx.execute(
            "INSERT INTO messages(id,account_id,subject,sender,received_at) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET subject=excluded.subject,sender=excluded.sender,received_at=excluded.received_at",
            params![id,location.account.0,envelope.subject,envelope.sender,envelope.received_at],
        )?;
        tx.execute(
            "INSERT INTO folder_messages(account_id,folder_id,uidvalidity,uid,message_id,seen,flagged) VALUES (?1,?2,?3,?4,?5,0,0) ON CONFLICT DO NOTHING",
            params![
                location.account.0,
                location.folder_id,
                location.uidvalidity,
                location.uid,
                id
            ],
        )?;
        tx.commit()?;
        Ok(MessageId(id))
    }

    /// 已下载正文的附件数量；搜索“有附件”只覆盖已标记的邮件。
    pub fn mark_attachments(&mut self, location: &MailUid, count: u32) -> Result<()> {
        if count > 10_000 {
            return Err(Error::Invalid("附件数量异常"));
        }
        self.connection.execute(
            "UPDATE messages SET has_attachments=?1 WHERE id=(SELECT message_id FROM folder_messages WHERE account_id=?2 AND folder_id=?3 AND uidvalidity=?4 AND uid=?5)",
            params![
                count,
                location.account.0,
                location.folder_id,
                location.uidvalidity,
                location.uid
            ],
        )?;
        Ok(())
    }

    /// 已缓存内容的本地搜索；不发网络请求。has_attachments 仅对有正文标记的邮件生效。
    pub fn search(&self, account: &AccountId, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        self.search_scope(Some(account), query)
    }

    pub fn search_scope(
        &self,
        account: Option<&AccountId>,
        query: &SearchQuery,
    ) -> Result<Vec<SearchHit>> {
        if query.text.len() > 512 || query.sender.len() > 320 {
            return Err(Error::Invalid("搜索条件过长"));
        }
        let like = |value: &str| format!("%{}%", escape_like(value));
        let text = like(&query.text);
        let sender = like(&query.sender);
        let mut statement = self.connection.prepare(
            "SELECT m.id,fm.uid,fm.uidvalidity,m.subject,m.sender,m.received_at,fm.seen,fm.flagged,f.remote_name,f.id,fm.account_id \
             FROM visible_folder_messages fm \
             JOIN messages m ON m.id=fm.message_id AND m.account_id=fm.account_id \
             JOIN folders f ON f.id=fm.folder_id AND f.account_id=fm.account_id \
             WHERE (?1 IS NULL OR fm.account_id=?1) \
               AND (?2 IS NULL OR f.remote_name=?2) \
               AND (?3='' OR m.subject LIKE ?4 ESCAPE '\\' OR m.sender LIKE ?4 ESCAPE '\\') \
               AND (?5='' OR m.sender LIKE ?6 ESCAPE '\\') \
               AND (?7=0 OR COALESCE(m.has_attachments,0) > 0) \
             ORDER BY m.received_at DESC, fm.account_id DESC, m.id DESC LIMIT 200",
        )?;
        Ok(statement
            .query_map(
                params![
                    account.map(|id| &id.0),
                    query.folder,
                    query.text,
                    text,
                    query.sender,
                    sender,
                    i64::from(query.has_attachments),
                ],
                |row| {
                    Ok(SearchHit {
                        message: CachedMessage {
                            id: MessageId(row.get(0)?),
                            location: MailUid {
                                account: AccountId(row.get(10)?),
                                folder_id: row.get(9)?,
                                uidvalidity: row.get(2)?,
                                uid: row.get(1)?,
                            },
                            envelope: MessageEnvelope {
                                subject: row.get(3)?,
                                sender: row.get(4)?,
                                received_at: row.get(5)?,
                            },
                            seen: row.get(6)?,
                            flagged: row.get(7)?,
                        },
                        folder: row.get(8)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 第一版按 UID 倒序稳定游标分页；不冒充按邮件日期排序或完整历史搜索。
    pub fn list_messages(
        &self,
        folder: &Folder,
        before_uid: Option<u32>,
        limit: u32,
    ) -> Result<Vec<CachedMessage>> {
        if !(1..=200).contains(&limit) {
            return Err(Error::Invalid("分页数量须为 1–200"));
        }
        let mut query = self.connection.prepare(
            "SELECT m.id,fm.uid,m.subject,m.sender,m.received_at,fm.seen,fm.flagged FROM visible_folder_messages fm JOIN messages m ON m.id=fm.message_id AND m.account_id=fm.account_id WHERE fm.account_id=?1 AND fm.folder_id=?2 AND fm.uidvalidity=?3 AND (?4 IS NULL OR fm.uid < ?4) ORDER BY fm.uid DESC LIMIT ?5",
        )?;
        Ok(query
            .query_map(
                params![
                    folder.account.0,
                    folder.id.0,
                    folder.uidvalidity,
                    before_uid,
                    limit
                ],
                |r| {
                    Ok(CachedMessage {
                        id: MessageId(r.get(0)?),
                        location: MailUid {
                            account: folder.account.clone(),
                            folder_id: folder.id.0.clone(),
                            uidvalidity: folder.uidvalidity,
                            uid: r.get(1)?,
                        },
                        envelope: MessageEnvelope {
                            subject: r.get(2)?,
                            sender: r.get(3)?,
                            received_at: r.get(4)?,
                        },
                        seen: r.get(5)?,
                        flagged: r.get(6)?,
                    })
                },
            )?
            .collect::<std::result::Result<_, _>>()?)
    }
}

fn validate_schema(connection: &Connection) -> Result<u32> {
    let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(Error::FutureSchema(version));
    }
    let app: u32 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if (version == 0 && app != 0) || (version != 0 && app != APPLICATION_ID) {
        return Err(Error::Invalid("非 QingMail 数据库，已保留原文件"));
    }
    if version == 0 {
        let tables: u32 = connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if tables != 0 {
            return Err(Error::Invalid("未知未版本化数据库，拒绝覆盖"));
        }
    }
    Ok(version)
}

fn migrate(
    connection: &mut Connection,
    directory: &PrivateDir,
    existed: bool,
    schema: &str,
    upgrade: &str,
    local_upgrade: &str,
) -> Result<()> {
    let version = validate_schema(connection)?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    // 已有库在升级前备份；所有步骤和版本号在一个事务中提交。
    if existed {
        let name = format!("migration-v{version}-{}.sqlite3", new_id()?);
        let backup = directory.file(&name, true)?;
        connection.backup(
            MAIN_DB,
            directory.path(&name),
            rusqlite::backup::NO_PROGRESS,
        )?;
        backup.sync_all()?;
        directory.0.sync_all()?;
    }
    let tx = connection.transaction()?;
    if version == 0 {
        tx.execute_batch(schema)?;
    }
    if version < 2 {
        tx.execute_batch(upgrade)?;
    }
    if version < 3 {
        tx.execute_batch(local_upgrade)?;
    }
    if version < 4 {
        tx.execute_batch(include_str!("schema-v4.sql"))?;
    }
    if version < 5 {
        tx.execute_batch(include_str!("schema-v5.sql"))?;
    }
    if version < 6 {
        tx.execute_batch(include_str!("schema-v6.sql"))?;
    }
    if version < 7 {
        tx.execute_batch(include_str!("schema-v7.sql"))?;
    }
    if version < 8 {
        tx.execute_batch(include_str!("schema-v8.sql"))?;
    }
    if version < 9 {
        tx.execute_batch(include_str!("schema-v9.sql"))?;
    }
    if version < 10 {
        tx.execute_batch(include_str!("schema-v10.sql"))?;
    }
    if version < 11 {
        tx.execute_batch(include_str!("schema-v11.sql"))?;
    }
    if version < 12 {
        tx.execute_batch(include_str!("schema-v12.sql"))?;
    }
    if version < 13 {
        tx.execute_batch(include_str!("schema-v13.sql"))?;
    }
    if version < 14 {
        tx.execute_batch(include_str!("schema-v14.sql"))?;
    }
    if version < 15 {
        tx.execute_batch(include_str!("schema-v15.sql"))?;
    }
    if version < 16 {
        tx.execute_batch(include_str!("schema-v16.sql"))?;
    }
    if version < 17 {
        tx.execute_batch(include_str!("schema-v17.sql"))?;
    }
    if version < 18 {
        tx.execute_batch(include_str!("schema-v18.sql"))?;
    }
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn new_id() -> Result<String> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| Error::Invalid("无法生成稳定 ID"))?;
    Ok(format!("{:032x}", u128::from_be_bytes(random)))
}
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
fn bounded(value: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max || value.contains('\0') {
        return Err(Error::Invalid("配置字段为空、过长或含 NUL"));
    }
    Ok(())
}
fn validate_settings(settings: &AccountSettings) -> Result<()> {
    bounded(&settings.display_name, 1024)?;
    bounded(&settings.address, 320)?;
    for server in [&settings.imap, &settings.smtp] {
        bounded(&server.host, 253)?;
        bounded(&server.username, 1024)?;
        if server.port == 0
            || server.host.chars().any(char::is_whitespace)
            || server.host.contains(['/', '@', '?', '#'])
        {
            return Err(Error::Invalid("服务器须为主机名或 IP，端口须非零"));
        }
    }
    Ok(())
}
fn security_name(security: ConnectionSecurity) -> &'static str {
    match security {
        ConnectionSecurity::Tls => "tls",
        ConnectionSecurity::StartTls => "starttls",
    }
}
fn read_security(value: String) -> rusqlite::Result<ConnectionSecurity> {
    match value.as_str() {
        "tls" => Ok(ConnectionSecurity::Tls),
        "starttls" => Ok(ConnectionSecurity::StartTls),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

#[cfg(test)]
mod tests;
