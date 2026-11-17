use super::*;
const MAX: u64 = 8 * 1024 * 1024;
impl Store {
    fn check_message_location(&self, location: &MailUid) -> Result<()> {
        let valid:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM folder_messages WHERE account_id=?1 AND folder_id=?2 AND uidvalidity=?3 AND uid=?4)",params![location.account.0,location.folder_id,location.uidvalidity,location.uid],|r|r.get(0))?;
        if !valid {
            return Err(Error::Conflict);
        }
        Ok(())
    }
    pub fn message_body(&self, location: &MailUid) -> Result<Option<Vec<u8>>> {
        self.check_message_location(location)?;
        let id:Option<String>=self.connection.query_row("SELECT object_id FROM message_bodies WHERE account_id=?1 AND folder_id=?2 AND uidvalidity=?3 AND uid=?4",params![location.account.0,location.folder_id,location.uidvalidity,location.uid],|r|r.get(0)).optional()?;
        id.map(|id| {
            objects::read(
                &self.objects,
                &self.connection,
                &location.account,
                &id,
                "body",
                MAX,
            )
        })
        .transpose()
    }
    /// 下载完整且当前四元定位仍有效才关联；不覆盖已损坏的原对象。
    pub fn save_message_body(&mut self, location: &MailUid, raw: &[u8]) -> Result<()> {
        self.check_message_location(location)?;
        if raw.is_empty() || raw.len() as u64 > MAX {
            return Err(Error::Invalid("邮件为空或超过8MiB读取上限"));
        }
        if let Some(existing) = self.message_body(location)? {
            return if existing == raw {
                Ok(())
            } else {
                Err(Error::Invalid("相同UID内容发生变化，保留已缓存邮件"))
            };
        }
        let object = objects::Object::write(
            &self.objects,
            &location.account,
            "body",
            &mut std::io::Cursor::new(raw),
            MAX,
        )?;
        let result = (|| {
            let tx = self.connection.transaction()?;
            object.insert(&tx)?;
            tx.execute("INSERT INTO message_bodies(account_id,folder_id,uidvalidity,uid,object_id) VALUES (?1,?2,?3,?4,?5)",params![location.account.0,location.folder_id,location.uidvalidity,location.uid,object.id])?;
            tx.commit()?;
            Ok(())
        })();
        // 与既有对象存储一致，未知提交结果不擅自移除文件；孤立对象待统一回收。
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Store, MailUid) {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let server = ServerSettings {
            host: "example.invalid".into(),
            port: 993,
            username: "synthetic".into(),
            security: ConnectionSecurity::Tls,
        };
        let account = store
            .create_account(&AccountSettings {
                display_name: "synthetic".into(),
                address: "test@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        let folder = store.upsert_folder(&account, "INBOX", 42).unwrap();
        let location = MailUid {
            account,
            folder_id: folder.id.0,
            uidvalidity: 42,
            uid: 7,
        };
        store
            .cache_message(
                &location,
                &MessageEnvelope {
                    subject: "test".into(),
                    sender: "test".into(),
                    received_at: 0,
                },
            )
            .unwrap();
        (root, store, location)
    }
    #[test]
    fn v4_upgrade_preserves_cached_index_and_can_attach_body_after_backup() {
        let (root, store, location) = setup();
        store
            .connection
            .execute_batch("DROP TABLE mutation_results; DROP TABLE message_bodies; ALTER TABLE send_tasks DROP COLUMN copy_folder; ALTER TABLE send_tasks DROP COLUMN copy_uidvalidity; ALTER TABLE send_tasks DROP COLUMN copy_uid; ALTER TABLE folder_messages DROP COLUMN seen; ALTER TABLE folder_messages DROP COLUMN flagged; ALTER TABLE messages DROP COLUMN has_attachments; PRAGMA user_version=4;")
            .unwrap();
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert!(store.message_body(&location).unwrap().is_none());
        store
            .save_message_body(&location, b"Subject: restored\r\n\r\nhello")
            .unwrap();
        assert!(
            std::fs::read_dir(root.path().join("data"))
                .unwrap()
                .any(|item| item
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("migration-v4-"))
        );
    }
    #[test]
    fn raw_bytes_roundtrip_reopen_and_stale_epoch_cannot_alias_body() {
        let (root, mut store, location) = setup();
        let raw = b"Subject: test\r\n\r\nhello\0raw";
        assert!(store.message_body(&location).unwrap().is_none());
        store.save_message_body(&location, raw).unwrap();
        store.save_message_body(&location, raw).unwrap();
        assert!(store.save_message_body(&location, b"changed").is_err());
        assert_eq!(store.message_body(&location).unwrap().unwrap(), raw);
        let mut wrong = location.clone();
        wrong.account = AccountId("different".into());
        assert!(store.message_body(&wrong).is_err());
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(store.message_body(&location).unwrap().unwrap(), raw);
        store.upsert_folder(&location.account, "INBOX", 43).unwrap();
        assert!(store.message_body(&location).is_err());
        assert!(store.save_message_body(&location, raw).is_err());
    }
    #[test]
    fn corrupt_object_and_failed_link_are_preserved_without_false_success() {
        let (root, mut store, location) = setup();
        store.connection.execute_batch("CREATE TRIGGER fail_body BEFORE INSERT ON message_bodies BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap();
        assert!(
            store
                .save_message_body(&location, b"complete MIME")
                .is_err()
        );
        assert!(store.message_body(&location).unwrap().is_none());
        store
            .connection
            .execute_batch("DROP TRIGGER fail_body")
            .unwrap();
        store
            .save_message_body(&location, b"complete MIME")
            .unwrap();
        let id: String = store
            .connection
            .query_row("SELECT object_id FROM message_bodies", [], |r| r.get(0))
            .unwrap();
        std::fs::write(root.path().join("data/objects").join(id), b"tampered MIME").unwrap();
        assert!(store.message_body(&location).is_err());
        assert!(
            store
                .save_message_body(&location, b"complete MIME")
                .is_err()
        );
    }
}
