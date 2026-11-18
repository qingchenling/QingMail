//! 引用驱动缓存淘汰：先提交引用移除，再unlink；索引和受保护内容不参与淘汰。
use crate::*;
impl Store {
    pub fn cache_status(&self) -> Result<mail_protocol::CacheInfo> {
        let budget =
            self.connection
                .query_row("SELECT budget FROM cache_settings WHERE id=1", [], |r| {
                    r.get::<_, i64>(0)
                })?;
        let (total,evictable):(i64,i64)=self.connection.query_row("SELECT coalesce(sum(size),0),coalesce(sum(CASE WHEN id IN (SELECT object_id FROM cache_access) AND id NOT IN (SELECT id FROM protected_objects) THEN size ELSE 0 END),0) FROM objects",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        Ok(mail_protocol::CacheInfo {
            budget_bytes: budget as u64,
            evictable_bytes: evictable as u64,
            protected_bytes: (total - evictable) as u64,
            downloading_bytes: 0,
        })
    }
    pub fn set_cache_budget(&mut self, bytes: u64) -> Result<()> {
        if bytes > 1024 * 1024 * 1024 * 1024 {
            return Err(Error::Invalid("缓存预算须在0到1TiB之间"));
        }
        self.connection.execute(
            "UPDATE cache_settings SET budget=?1 WHERE id=1",
            [bytes as i64],
        )?;
        self.trim_cache(false)
    }
    pub fn clear_cache(&mut self) -> Result<()> {
        self.trim_cache(true)
    }
    pub fn trim_cache(&mut self, clear: bool) -> Result<()> {
        let status = self.cache_status()?;
        let mut remaining = status.evictable_bytes + status.protected_bytes;
        let target = if clear { 0 } else { status.budget_bytes };
        if remaining <= target {
            return Ok(());
        }
        let mut q=self.connection.prepare("SELECT o.id,o.size FROM objects o JOIN cache_access c ON c.object_id=o.id WHERE o.id NOT IN (SELECT id FROM protected_objects) ORDER BY c.accessed_at,o.id")?;
        let candidates = q
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(q);
        let tx = self.connection.transaction()?;
        let mut removed = vec![];
        for (id, size) in candidates {
            if remaining <= target {
                break;
            }
            tx.execute("DELETE FROM message_bodies WHERE object_id=?1", [&id])?;
            tx.execute("DELETE FROM part_resources WHERE object_id=?1", [&id])?;
            tx.execute("DELETE FROM objects WHERE id=?1", [&id])?;
            removed.push(id);
            remaining = remaining.saturating_sub(size);
        }
        tx.commit()?;
        for id in removed {
            self.objects.remove(&id);
        }
        self.objects.0.sync_all()?;
        Ok(())
    }
    pub(super) fn recover_cache_files(&self) -> Result<()> {
        // 仅移除受控32位ID且无数据库记录的孤儿；任何关联提交前写盘的中间文件由单写者启动时回收。
        for entry in std::fs::read_dir(self._directory.path("objects"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str() else {
                continue;
            };
            if !mail_protocol::valid_id(id) {
                continue;
            }
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE id=?1)",
                [id],
                |r| r.get(0),
            )?;
            if !exists {
                self.objects.remove(id);
            }
        }
        self.objects.0.sync_all()?;
        Ok(())
    }
    pub(super) fn touch_cache(&self, id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE cache_access SET accessed_at=unixepoch() WHERE object_id=?1",
            [id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_protocol::{Command, MailOperation, Request};
    fn setup() -> (tempfile::TempDir, Store, MailUid, MailUid) {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let account = store.create_account(&crate::tests::settings()).unwrap();
        let f = store.upsert_folder(&account, "INBOX", 42).unwrap();
        let a = MailUid {
            account,
            folder_id: f.id.0,
            uidvalidity: 42,
            uid: 1,
        };
        let b = MailUid {
            uid: 2,
            ..a.clone()
        };
        for location in [&a, &b] {
            store
                .cache_message(
                    location,
                    &MessageEnvelope {
                        subject: "cache".into(),
                        sender: "fixture".into(),
                        received_at: 1,
                    },
                )
                .unwrap();
            store
                .save_message_body(location, b"Subject: cache\r\n\r\nbody")
                .unwrap();
        }
        (root, store, a, b)
    }
    #[test]
    fn cache_lru_keeps_index_and_budget_survives_restart() {
        let (root, mut store, a, b) = setup();
        store
            .connection
            .execute("UPDATE cache_access SET accessed_at=0", [])
            .unwrap();
        store.message_body(&a).unwrap().unwrap();
        let size = store.message_body(&a).unwrap().unwrap().len() as u64;
        store.set_cache_budget(size).unwrap();
        assert!(store.message_body(&a).unwrap().is_some());
        assert!(store.message_body(&b).unwrap().is_none());
        assert_eq!(
            store
                .list_messages(&store.folders(&a.account).unwrap()[0], None, 50)
                .unwrap()
                .len(),
            2
        );
        drop(store);
        let store = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(store.cache_status().unwrap().budget_bytes, size);
        assert!(store.message_body(&a).unwrap().is_some());
    }
    #[test]
    fn cache_clear_protects_pending_operations_and_user_attachments() {
        let (_root, mut store, a, b) = setup();
        let attachment = store
            .import_attachment(
                &a.account,
                "keep.txt",
                "text/plain",
                &mut b"user data".as_slice(),
            )
            .unwrap();
        let request = Request::new(
            new_id().unwrap(),
            Command::EnqueueOperation {
                locations: vec![a.clone()],
                operation: MailOperation::Flags {
                    seen: Some(true),
                    flagged: None,
                },
            },
        );
        store.enqueue_operations(&request).unwrap();
        store.set_cache_budget(0).unwrap();
        assert!(store.message_body(&a).unwrap().is_some());
        assert!(store.message_body(&b).unwrap().is_none());
        assert!(store.attachment(&a.account, &attachment.id).is_ok());
        let status = store.cache_status().unwrap();
        assert!(status.protected_bytes > status.budget_bytes);
        assert_eq!(status.evictable_bytes, 0);
        store
            .connection
            .execute("UPDATE mail_operations SET state='applied'", [])
            .unwrap();
        store.clear_cache().unwrap();
        assert!(store.message_body(&a).unwrap().is_none());
        assert!(store.attachment(&a.account, &attachment.id).is_ok());
    }
    #[test]
    fn cache_failed_transaction_preserves_files_and_startup_collects_only_orphans() {
        let (root, mut store, a, _) = setup();
        store.connection.execute_batch("CREATE TRIGGER fail_cache_delete BEFORE DELETE ON objects BEGIN SELECT RAISE(ABORT,'simulated disk full'); END;").unwrap();
        assert!(store.clear_cache().is_err());
        assert!(store.message_body(&a).unwrap().is_some());
        assert!(store.cache_status().unwrap().evictable_bytes > 0);
        store
            .connection
            .execute_batch("DROP TRIGGER fail_cache_delete;")
            .unwrap();
        let orphan = new_id().unwrap();
        drop(store.objects.file(&orphan, true).unwrap());
        let unrelated = root.path().join("data/objects/keep-unrecognized");
        std::fs::write(&unrelated, b"untouched").unwrap();
        drop(store);
        let store = Store::open(&root.path().join("data")).unwrap();
        assert!(store.message_body(&a).unwrap().is_some());
        assert!(!root.path().join("data/objects").join(orphan).exists());
        assert!(unrelated.exists());
    }
    #[test]
    fn cache_schema16_upgrade_preserves_objects_and_creates_backup() {
        let (root, store, a, _) = setup();
        store.connection.execute_batch("DROP TABLE notification_candidates; DROP TABLE notification_pending; DROP TABLE notification_baselines; DROP TABLE desktop_settings; DROP VIEW protected_objects; DROP TRIGGER cache_body_insert; DROP TRIGGER cache_part_insert; DROP TABLE cache_access; DROP TABLE cache_settings; PRAGMA user_version=16;").unwrap();
        drop(store);
        let store = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(store.cache_status().unwrap().budget_bytes, 1073741824);
        assert!(store.cache_status().unwrap().evictable_bytes > 0);
        assert!(store.message_body(&a).unwrap().is_some());
        assert!(
            std::fs::read_dir(root.path().join("data"))
                .unwrap()
                .any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("migration-v16-"))
        );
    }
}
