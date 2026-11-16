use super::*;
use mail_core::{BatchKind, FolderBatch, FolderCache, RemoteFolder};
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
            let flags = batch.flags.get(uid).copied().unwrap_or_default();
            tx.execute("INSERT INTO messages VALUES (?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET subject=excluded.subject,sender=excluded.sender,received_at=excluded.received_at",params![message_id,account.0,message.subject,message.sender,message.received_at])?;
            tx.execute(
                "INSERT INTO folder_messages(account_id,folder_id,uidvalidity,uid,message_id,seen,flagged) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(account_id,folder_id,uidvalidity,uid) DO UPDATE SET seen=excluded.seen,flagged=excluded.flagged",
                params![account.0, id, batch.uidvalidity, uid, message_id, flags.seen, flags.flagged],
            )?;
        }
        // 只有最近窗口的完整批次才删除远端已不存在的缓存；90天之前的缓存保留。
        if batch.kind == BatchKind::Recent && batch.complete && !batch.has_more {
            let cached: Vec<(u32, i64)> = tx
                .prepare("SELECT fm.uid,m.received_at FROM folder_messages fm JOIN messages m ON m.id=fm.message_id AND m.account_id=fm.account_id WHERE fm.account_id=?1 AND fm.folder_id=?2 AND fm.uidvalidity=?3")?
                .query_map(params![account.0, id, batch.uidvalidity], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            for (uid, received_at) in cached {
                if !uids.contains(&uid) && received_at >= batch.since {
                    tx.execute(
                        "DELETE FROM folder_messages WHERE account_id=?1 AND folder_id=?2 AND uidvalidity=?3 AND uid=?4",
                        params![account.0, id, batch.uidvalidity, uid],
                    )?;
                }
            }
        }
        if batch.kind == BatchKind::Recent {
            tx.execute("INSERT INTO folder_sync VALUES (?1,?2,?3,?4) ON CONFLICT(folder_id) DO UPDATE SET synced_at=excluded.synced_at,since=excluded.since,has_more=excluded.has_more",params![id,now,batch.since,batch.has_more])?;
        } else {
            tx.execute("INSERT INTO folder_sync VALUES (?1,?2,?3,?4) ON CONFLICT(folder_id) DO UPDATE SET synced_at=excluded.synced_at,has_more=excluded.has_more",params![id,now,batch.since,batch.has_more])?;
        }
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

    /// 远端标记操作成功后回写本地；失败时调用方不得改动缓存。
    pub fn set_flags(
        &mut self,
        account: &AccountId,
        name: &str,
        uidvalidity: u32,
        uids: &[u32],
        seen: Option<bool>,
        flagged: Option<bool>,
    ) -> Result<()> {
        let folder_id = self.folder_id(account, name)?;
        let tx = self.connection.transaction()?;
        for uid in uids {
            if let Some(seen) = seen {
                tx.execute(
                    "UPDATE folder_messages SET seen=?1 WHERE account_id=?2 AND folder_id=?3 AND uidvalidity=?4 AND uid=?5",
                    params![seen, account.0, folder_id, uidvalidity, uid],
                )?;
            }
            if let Some(flagged) = flagged {
                tx.execute(
                    "UPDATE folder_messages SET flagged=?1 WHERE account_id=?2 AND folder_id=?3 AND uidvalidity=?4 AND uid=?5",
                    params![flagged, account.0, folder_id, uidvalidity, uid],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 远端移动/删除成功后移除源文件夹缓存；目标文件夹等下次同步补齐。
    pub fn remove_cached(
        &mut self,
        account: &AccountId,
        name: &str,
        uidvalidity: u32,
        uids: &[u32],
    ) -> Result<()> {
        let folder_id = self.folder_id(account, name)?;
        let tx = self.connection.transaction()?;
        for uid in uids {
            tx.execute(
                "DELETE FROM folder_messages WHERE account_id=?1 AND folder_id=?2 AND uidvalidity=?3 AND uid=?4",
                params![account.0, folder_id, uidvalidity, uid],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn folder_id(&self, account: &AccountId, name: &str) -> Result<String> {
        self.connection
            .query_row(
                "SELECT id FROM folders WHERE account_id=?1 AND remote_name=?2",
                params![account.0, name],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::Invalid("文件夹尚未同步，请先刷新"))
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
            complete: true,
            kind: BatchKind::Recent,
            flags: Default::default(),
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
    fn flags_prune_and_remove_follow_remote_updates() {
        use mail_core::MessageFlags;
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let account = store.create_account(&settings()).unwrap();
        store
            .replace_remote_folders(&account, &catalog(), 1)
            .unwrap();
        let mut recent = batch(1, 4);
        recent.flags.insert(
            4,
            MessageFlags {
                seen: true,
                flagged: true,
            },
        );
        store.apply_folder_batch(&account, &recent, 2).unwrap();
        let cache = store
            .folder_cache(&account, "INBOX", None)
            .unwrap()
            .unwrap();
        assert!(cache.messages[0].seen);
        assert!(cache.messages[0].flagged);

        // 完整最近批次缺少的 UID 视为远端已移走/删除。
        let mut without = batch(1, 5);
        without.since = 10;
        store.apply_folder_batch(&account, &without, 3).unwrap();
        let cache = store
            .folder_cache(&account, "INBOX", None)
            .unwrap()
            .unwrap();
        assert_eq!(cache.messages.len(), 1);
        assert_eq!(cache.messages[0].location.uid, 5);

        // 90天之前的历史缓存不因最近窗口完整批次而删除。
        let mut history = batch(1, 2);
        history.kind = BatchKind::History;
        history.since = 10;
        history.messages[0].1.received_at = 1;
        store.apply_folder_batch(&account, &history, 4).unwrap();
        let mut latest = batch(1, 6);
        latest.since = 10;
        store.apply_folder_batch(&account, &latest, 5).unwrap();
        let cache = store
            .folder_cache(&account, "INBOX", None)
            .unwrap()
            .unwrap();
        assert!(cache.messages.iter().any(|m| m.location.uid == 2));
        assert!(!cache.messages.iter().any(|m| m.location.uid == 5));

        store
            .set_flags(&account, "INBOX", 1, &[6], Some(true), Some(true))
            .unwrap();
        let cache = store
            .folder_cache(&account, "INBOX", None)
            .unwrap()
            .unwrap();
        let message = cache.messages.iter().find(|m| m.location.uid == 6).unwrap();
        assert!(message.seen && message.flagged);
        store.remove_cached(&account, "INBOX", 1, &[6]).unwrap();
        let cache = store
            .folder_cache(&account, "INBOX", None)
            .unwrap()
            .unwrap();
        assert!(!cache.messages.iter().any(|m| m.location.uid == 6));
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
        store.connection.execute_batch("DROP TABLE message_bodies; DROP TABLE remote_folders; DROP TABLE folder_sync; DROP TABLE catalog_sync; ALTER TABLE send_tasks DROP COLUMN copy_folder; ALTER TABLE send_tasks DROP COLUMN copy_uidvalidity; ALTER TABLE send_tasks DROP COLUMN copy_uid; ALTER TABLE folder_messages DROP COLUMN seen; ALTER TABLE folder_messages DROP COLUMN flagged; PRAGMA user_version=3;").unwrap();
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
