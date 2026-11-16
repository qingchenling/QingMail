use super::*;
use mail_core::{FolderBatch, FolderCache, RemoteFolder};
use std::collections::HashSet;

impl Store {
    pub fn remote_folders(&self, account: &AccountId) -> Result<Vec<RemoteFolder>> {
        let mut query=self.connection.prepare("SELECT remote_name,display_name,delimiter,selectable FROM remote_folders WHERE account_id=?1 ORDER BY remote_name")?;
        Ok(query
            .query_map([&account.0], |r| {
                Ok(RemoteFolder {
                    name: r.get(0)?,
                    display_name: r.get(1)?,
                    delimiter: r.get(2)?,
                    selectable: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    /// 只有完整 LIST 成功才替换目录；旧文件夹缓存保留，不把目录移除当删除正文。
    pub fn replace_remote_folders(
        &mut self,
        account: &AccountId,
        folders: &[RemoteFolder],
        now: i64,
    ) -> Result<()> {
        if self.account(account)?.is_none() || folders.len() > 2048 {
            return Err(Error::Invalid("账号或目录数量无效"));
        }
        let mut names = HashSet::new();
        for folder in folders {
            bounded(&folder.name, 4096)?;
            bounded(&folder.display_name, 4096)?;
            if folder
                .delimiter
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_some_and(|d| folder.name.split(d).count() > 32)
                || !names.insert(&folder.name)
                || folder.name.chars().any(char::is_control)
                || folder
                    .delimiter
                    .as_ref()
                    .is_some_and(|s| s.len() > 32 || s.chars().any(char::is_control))
            {
                return Err(Error::Invalid("服务器文件夹目录无效"));
            }
        }
        let tx = self.connection.transaction()?;
        tx.execute(
            "DELETE FROM remote_folders WHERE account_id=?1",
            [&account.0],
        )?;
        for folder in folders {
            tx.execute(
                "INSERT INTO remote_folders VALUES (?1,?2,?3,?4,?5)",
                params![
                    account.0,
                    folder.name,
                    folder.display_name,
                    folder.delimiter,
                    folder.selectable
                ],
            )?;
        }
        tx.execute("INSERT INTO catalog_sync VALUES (?1,?2) ON CONFLICT(account_id) DO UPDATE SET synced_at=excluded.synced_at",params![account.0,now])?;
        tx.commit()?;
        Ok(())
    }
    pub fn catalog_synced_at(&self, account: &AccountId) -> Result<Option<i64>> {
        Ok(self
            .connection
            .query_row(
                "SELECT synced_at FROM catalog_sync WHERE account_id=?1",
                [&account.0],
                |r| r.get(0),
            )
            .optional()?)
    }
    /// 整批头部与 UIDVALIDITY 在同一事务发布，断网/缺项不产生半个新 epoch。
    pub fn apply_folder_batch(
        &mut self,
        account: &AccountId,
        batch: &FolderBatch,
        now: i64,
    ) -> Result<()> {
        if self.account(account)?.is_none() || batch.uidvalidity == 0 || batch.messages.len() > 200
        {
            return Err(Error::Invalid("同步批次无效"));
        }
        bounded(&batch.remote_name, 4096)?;
        let selectable: bool = self
            .connection
            .query_row(
                "SELECT selectable FROM remote_folders WHERE account_id=?1 AND remote_name=?2",
                params![account.0, batch.remote_name],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if !selectable {
            return Err(Error::Invalid("文件夹已移除或不可选择，请刷新目录"));
        }
        let mut uids = HashSet::new();
        for (uid, message) in &batch.messages {
            if *uid == 0
                || !uids.insert(uid)
                || message.subject.len() > 64 * 1024
                || message.sender.len() > 64 * 1024
            {
                return Err(Error::Invalid("同步邮件索引无效"));
            }
        }
        let tx = self.connection.transaction()?;
        let old: Option<(String, u32)> = tx
            .query_row(
                "SELECT id,uidvalidity FROM folders WHERE account_id=?1 AND remote_name=?2",
                params![account.0, batch.remote_name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let id = if let Some((id, epoch)) = old {
            if epoch != batch.uidvalidity {
                tx.execute(
                    "DELETE FROM folder_messages WHERE account_id=?1 AND folder_id=?2",
                    params![account.0, id],
                )?;
                tx.execute(
                    "UPDATE folders SET uidvalidity=?1 WHERE id=?2 AND account_id=?3",
                    params![batch.uidvalidity, id, account.0],
                )?;
            }
            id
        } else {
            let id = new_id()?;
            tx.execute(
                "INSERT INTO folders VALUES (?1,?2,?3,?4)",
                params![id, account.0, batch.remote_name, batch.uidvalidity],
            )?;
            id
        };
        for (uid, message) in &batch.messages {
            let existing:Option<String>=tx.query_row("SELECT message_id FROM folder_messages WHERE account_id=?1 AND folder_id=?2 AND uidvalidity=?3 AND uid=?4",params![account.0,id,batch.uidvalidity,uid],|r|r.get(0)).optional()?;
            let message_id = existing.map(Ok).unwrap_or_else(new_id)?;
            tx.execute("INSERT INTO messages VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET subject=excluded.subject,sender=excluded.sender,received_at=excluded.received_at",params![message_id,account.0,message.subject,message.sender,message.received_at])?;
            tx.execute(
                "INSERT INTO folder_messages VALUES (?1,?2,?3,?4,?5) ON CONFLICT DO NOTHING",
                params![account.0, id, batch.uidvalidity, uid, message_id],
            )?;
        }
        tx.execute("INSERT INTO folder_sync VALUES (?1,?2,?3,?4) ON CONFLICT(folder_id) DO UPDATE SET synced_at=excluded.synced_at,since=excluded.since,has_more=excluded.has_more",params![id,now,batch.since,batch.has_more])?;
        tx.commit()?;
        Ok(())
    }
    pub fn folder_cache(
        &self,
        account: &AccountId,
        name: &str,
        before: Option<u32>,
    ) -> Result<Option<FolderCache>> {
        let folder = self
            .folders(account)?
            .into_iter()
            .find(|folder| folder.remote_name == name);
        let Some(folder) = folder else {
            return Ok(None);
        };
        let synced: Option<(i64, i64, bool)> = self
            .connection
            .query_row(
                "SELECT synced_at,since,has_more FROM folder_sync WHERE folder_id=?1",
                [&folder.id.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((synced_at, since, has_more)) = synced else {
            return Ok(None);
        };
        Ok(Some(FolderCache {
            messages: self.list_messages(&folder, before, 200)?,
            folder,
            synced_at,
            since,
            has_more,
        }))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> AccountSettings {
        let server = ServerSettings {
            host: "example.invalid".into(),
            port: 993,
            username: "test".into(),
            security: ConnectionSecurity::Tls,
        };
        AccountSettings {
            display_name: "test".into(),
            address: "test@example.invalid".into(),
            imap: server.clone(),
            smtp: server,
        }
    }
    fn catalog() -> Vec<RemoteFolder> {
        let mut folders = vec![
            RemoteFolder {
                name: "INBOX".into(),
                display_name: "收件箱".into(),
                delimiter: Some("/".into()),
                selectable: true,
            },
            RemoteFolder {
                name: "Archive".into(),
                display_name: "归档".into(),
                delimiter: Some("/".into()),
                selectable: false,
            },
        ];
        folders.sort_by(|a, b| a.name.cmp(&b.name));
        folders
    }
    fn batch(epoch: u32, uid: u32) -> FolderBatch {
        FolderBatch {
            remote_name: "INBOX".into(),
            uidvalidity: epoch,
            since: 1,
            has_more: false,
            messages: vec![(
                uid,
                MessageEnvelope {
                    subject: "中文🙂".into(),
                    sender: "测试 <test@example.invalid>".into(),
                    received_at: 10,
                },
            )],
        }
    }
    #[test]
    fn atomic_batches_preserve_identity_and_scope_epoch_rebuild_to_one_folder() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut store = Store::open(&path).unwrap();
        let a = store.create_account(&settings()).unwrap();
        let b = store.create_account(&settings()).unwrap();
        for account in [&a, &b] {
            store
                .replace_remote_folders(account, &catalog(), 1)
                .unwrap();
            store.apply_folder_batch(account, &batch(1, 4), 2).unwrap();
        }
        let old = store.folder_cache(&a, "INBOX", None).unwrap().unwrap();
        store.apply_folder_batch(&a, &batch(1, 4), 3).unwrap();
        assert_eq!(
            store
                .folder_cache(&a, "INBOX", None)
                .unwrap()
                .unwrap()
                .messages,
            old.messages
        );
        let mut invalid = batch(2, 7);
        invalid.messages.push((0, invalid.messages[0].1.clone()));
        assert!(store.apply_folder_batch(&a, &invalid, 4).is_err());
        assert_eq!(
            store
                .folder_cache(&a, "INBOX", None)
                .unwrap()
                .unwrap()
                .folder
                .uidvalidity,
            1
        );
        store.apply_folder_batch(&a, &batch(2, 7), 5).unwrap();
        let new = store.folder_cache(&a, "INBOX", None).unwrap().unwrap();
        assert_eq!(old.folder.id, new.folder.id);
        assert_eq!(new.messages[0].location.uid, 7);
        assert_eq!(new.messages.len(), 1);
        assert_eq!(
            store
                .folder_cache(&b, "INBOX", None)
                .unwrap()
                .unwrap()
                .folder
                .uidvalidity,
            1
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.remote_folders(&a).unwrap(), catalog());
        assert_eq!(store.folder_cache(&a, "INBOX", None).unwrap().unwrap(), new);
    }
    #[test]
    fn invalid_catalog_and_failed_sql_keep_previous_cache() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let a = store.create_account(&settings()).unwrap();
        store.replace_remote_folders(&a, &catalog(), 1).unwrap();
        store.apply_folder_batch(&a, &batch(1, 4), 2).unwrap();
        let old = store.folder_cache(&a, "INBOX", None).unwrap();
        let mut duplicate = catalog();
        duplicate.push(duplicate[0].clone());
        assert!(store.replace_remote_folders(&a, &duplicate, 3).is_err());
        assert_eq!(store.remote_folders(&a).unwrap(), catalog());
        store.connection.execute_batch("CREATE TRIGGER fail_sync BEFORE INSERT ON folder_sync BEGIN SELECT RAISE(ABORT,'synthetic write failure'); END;").unwrap();
        assert!(store.apply_folder_batch(&a, &batch(2, 7), 4).is_err());
        assert_eq!(store.folder_cache(&a, "INBOX", None).unwrap(), old);
        store.replace_remote_folders(&a, &[], 5).unwrap();
        assert!(store.remote_folders(&a).unwrap().is_empty());
        assert!(store.folder_cache(&a, "INBOX", None).unwrap().is_some());
        assert!(store.apply_folder_batch(&a, &batch(1, 4), 6).is_err());
    }
    #[test]
    fn v3_migration_keeps_accounts_and_drafts_with_private_backup() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut store = Store::open(&path).unwrap();
        let a = store.create_account(&settings()).unwrap();
        store.connection.execute_batch("DROP TABLE remote_folders; DROP TABLE folder_sync; DROP TABLE catalog_sync; PRAGMA user_version=3;").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.account(&a).unwrap(), Some(settings()));
        assert!(store.remote_folders(&a).unwrap().is_empty());
        assert!(std::fs::read_dir(&path).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("migration-v3-")
        }));
    }
}
