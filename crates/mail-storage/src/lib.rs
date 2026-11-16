//! P2 本地索引：后台单写者、XDG 路径、事务迁移和 IMAP 定位隔离。
//! 不保存密码/令牌；草稿与发送快照使用不可变对象和事务关联。
mod objects;
mod outbox;
mod paths;
pub use outbox::{
    AttachmentInfo, DraftDocument, OutgoingAttachment, OutgoingSnapshot, SendTask, SmtpAttempt,
    StoredDraft,
};
mod private_dir;

pub use paths::AppPaths;

use mail_core::{
    AccountId, AccountSettings, CachedMessage, ConnectionSecurity, Folder, FolderId, MailUid,
    MessageEnvelope, MessageId, ServerSettings,
};
use private_dir::PrivateDir;
use rusqlite::{Connection, MAIN_DB, OpenFlags, OptionalExtension, params};
use std::{fmt, fs::File, io, path::Path, time::Duration};

const SCHEMA_VERSION: u32 = 3;
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
        Ok(Self {
            objects,
            connection,
            _writer: writer,
            _directory: directory,
        })
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
        self.connection.execute("INSERT INTO local_editor VALUES (1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET draft_id=excluded.draft_id,account_id=excluded.account_id",params![id.0,account.0])?;
        Ok(())
    }

    pub fn create_account(&mut self, settings: &AccountSettings) -> Result<AccountId> {
        validate_settings(settings)?;
        let id = AccountId(new_id()?);
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
        Ok(id)
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
            "INSERT INTO messages VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET subject=excluded.subject,sender=excluded.sender,received_at=excluded.received_at",
            params![id,location.account.0,envelope.subject,envelope.sender,envelope.received_at],
        )?;
        tx.execute(
            "INSERT INTO folder_messages VALUES (?1,?2,?3,?4,?5) ON CONFLICT DO NOTHING",
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
            "SELECT m.id,fm.uid,m.subject,m.sender,m.received_at FROM folder_messages fm JOIN messages m ON m.id=fm.message_id AND m.account_id=fm.account_id WHERE fm.account_id=?1 AND fm.folder_id=?2 AND fm.uidvalidity=?3 AND (?4 IS NULL OR fm.uid < ?4) ORDER BY fm.uid DESC LIMIT ?5",
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
    tx.execute_batch(local_upgrade)?;
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
