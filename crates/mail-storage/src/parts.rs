//! 分段MIME清单与完整解码资源；未完成下载只持有已unlink的临时fd，不入缓存。
use crate::*;
use mail_core::{MimePart, MimeStructure};
use std::io::{Read, Seek, SeekFrom};
const MAX: u64 = 1024 * 1024 * 1024;
impl Store {
    pub fn mime_structure(&self, location: &MailUid) -> Result<Option<MimeStructure>> {
        self.check_message_location(location)?;
        let value:Option<String>=self.connection.query_row("SELECT manifest FROM message_structures WHERE account_id=?1 AND folder_id=?2 AND uidvalidity=?3 AND uid=?4",params![location.account.0,location.folder_id,location.uidvalidity,location.uid],|r|r.get(0)).optional()?;
        value
            .map(|s| serde_json::from_str(&s).map_err(|_| Error::Invalid("MIME清单损坏")))
            .transpose()
    }
    pub fn save_mime_structure(&mut self, location: &MailUid, value: &MimeStructure) -> Result<()> {
        self.check_message_location(location)?;
        if !value.valid() {
            return Err(Error::Invalid("MIME清单无效"));
        }
        if let Some(old) = self.mime_structure(location)? {
            return if old == *value {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        self.connection.execute(
            "INSERT INTO message_structures VALUES (?1,?2,?3,?4,?5)",
            params![
                location.account.0,
                location.folder_id,
                location.uidvalidity,
                location.uid,
                serde_json::to_string(value).map_err(|_| Error::Invalid("MIME清单无法编码"))?
            ],
        )?;
        Ok(())
    }
    fn check_part(&self, location: &MailUid, part: &MimePart) -> Result<()> {
        if !self
            .mime_structure(location)?
            .is_some_and(|m| m.parts.contains(part))
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }
    pub fn part_temporary(&self) -> Result<File> {
        let id = new_id()?;
        let file = self.objects.file(&id, true)?;
        self.objects.remove(&id);
        Ok(file)
    }
    pub fn save_part(
        &mut self,
        location: &MailUid,
        part: &MimePart,
        mut file: File,
    ) -> Result<File> {
        self.check_part(location, part)?;
        file.rewind()?;
        if let Some(existing) = self.part_file(location, part)? {
            return Ok(existing);
        }
        let object = objects::Object::write(
            &self.objects,
            &location.account,
            "attachment",
            &mut file,
            MAX,
        )?;
        let tx = self.connection.transaction()?;
        object.insert(&tx)?;
        tx.execute(
            "INSERT INTO part_resources VALUES (?1,?2,?3,?4,?5,?6,unixepoch())",
            params![
                location.account.0,
                location.folder_id,
                location.uidvalidity,
                location.uid,
                part.section,
                object.id
            ],
        )?;
        tx.commit()?;
        // 在淘汰前持有已校验的fd；即使预算为零，本次下载仍可完整导出。
        let ready = self.part_file(location, part)?.ok_or(Error::Conflict)?;
        self.trim_cache(false)?;
        Ok(ready)
    }
    /// 文件描述符只在后台内部传递；完整流校验后供有界IPC逐块读取。
    pub fn part_file(&self, location: &MailUid, part: &MimePart) -> Result<Option<File>> {
        use sha2::{Digest, Sha256};
        self.check_part(location, part)?;
        let info:Option<(String,u32,String)>=self.connection.query_row("SELECT o.id,o.size,o.sha256 FROM part_resources p JOIN objects o ON o.id=p.object_id AND o.account_id=p.account_id WHERE p.account_id=?1 AND p.folder_id=?2 AND p.uidvalidity=?3 AND p.uid=?4 AND p.section=?5",params![location.account.0,location.folder_id,location.uidvalidity,location.uid,part.section],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((id, size, hash)) = info else {
            return Ok(None);
        };
        if u64::from(size) > MAX || !mail_protocol::valid_id(&id) {
            return Err(Error::Invalid("资源大小或ID无效"));
        }
        let mut file = self.objects.read(&id)?;
        if file.metadata()?.len() != u64::from(size) {
            return Err(Error::Invalid("资源大小不符"));
        }
        let mut sha = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            sha.update(&buffer[..n]);
        }
        if format!("{:x}", sha.finalize()) != hash {
            return Err(Error::Invalid("资源完整性校验失败"));
        }
        file.seek(SeekFrom::Start(0))?;
        self.touch_cache(&id)?;
        self.connection.execute(
            "UPDATE part_resources SET accessed_at=unixepoch() WHERE object_id=?1",
            [id],
        )?;
        Ok(Some(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};
    fn fixture() -> (tempfile::TempDir, Store, MailUid, MimeStructure) {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let account = store.create_account(&crate::tests::settings()).unwrap();
        let f = store.upsert_folder(&account, "INBOX", 42).unwrap();
        let location = MailUid {
            account,
            folder_id: f.id.0,
            uidvalidity: 42,
            uid: 7,
        };
        store
            .cache_message(
                &location,
                &MessageEnvelope {
                    subject: "parts".into(),
                    sender: "fixture@example.invalid".into(),
                    received_at: 1,
                },
            )
            .unwrap();
        let manifest = MimeStructure {
            total_size: 50 * 1024 * 1024,
            headers: b"Subject: parts\r\n\r\n".to_vec(),
            parts: vec![
                MimePart {
                    section: "1".into(),
                    media_type: "text/plain".into(),
                    charset: Some("utf-8".into()),
                    encoding: "base64".into(),
                    encoded_size: 8,
                    name: None,
                    cid: None,
                    attachment: false,
                },
                MimePart {
                    section: "2".into(),
                    media_type: "application/octet-stream".into(),
                    charset: None,
                    encoding: "base64".into(),
                    encoded_size: 50 * 1024 * 1024,
                    name: Some("big.bin".into()),
                    cid: None,
                    attachment: true,
                },
            ],
        };
        (root, store, location, manifest)
    }
    #[test]
    fn cache_zero_budget_keeps_current_download_fd_readable_and_manifest() {
        let (_root, mut store, location, manifest) = fixture();
        store.save_mime_structure(&location, &manifest).unwrap();
        store.set_cache_budget(0).unwrap();
        let mut temporary = store.part_temporary().unwrap();
        temporary.write_all(b"complete attachment").unwrap();
        let mut ready = store
            .save_part(&location, &manifest.parts[1], temporary)
            .unwrap();
        let mut bytes = vec![];
        ready.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"complete attachment");
        assert!(
            store
                .part_file(&location, &manifest.parts[1])
                .unwrap()
                .is_none()
        );
        assert_eq!(store.mime_structure(&location).unwrap(), Some(manifest));
    }
    #[test]
    fn parts_manifest_and_complete_resource_restart_without_fetching_attachment() {
        let (root, mut store, location, manifest) = fixture();
        store.save_mime_structure(&location, &manifest).unwrap();
        assert!(
            store
                .part_file(&location, &manifest.parts[1])
                .unwrap()
                .is_none()
        );
        let mut file = store.part_temporary().unwrap();
        file.write_all(b"hello").unwrap();
        store
            .save_part(&location, &manifest.parts[0], file)
            .unwrap();
        drop(store);
        let store = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(
            store.mime_structure(&location).unwrap(),
            Some(manifest.clone())
        );
        let mut file = store
            .part_file(&location, &manifest.parts[0])
            .unwrap()
            .unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"hello");
        assert!(
            store
                .part_file(&location, &manifest.parts[1])
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn parts_partial_cancel_epoch_and_account_identity_never_publish_wrong_resource() {
        use std::os::unix::fs::MetadataExt;
        let (_root, mut store, location, manifest) = fixture();
        store.save_mime_structure(&location, &manifest).unwrap();
        let mut temporary = store.part_temporary().unwrap();
        assert_eq!(temporary.metadata().unwrap().nlink(), 0);
        temporary.write_all(b"partial").unwrap();
        drop(temporary);
        assert!(
            store
                .part_file(&location, &manifest.parts[1])
                .unwrap()
                .is_none()
        );
        let other = store.create_account(&crate::tests::settings()).unwrap();
        let mut wrong = location.clone();
        wrong.account = other;
        assert!(store.part_file(&wrong, &manifest.parts[0]).is_err());
        let mut altered = manifest.parts[0].clone();
        altered.section = "2.1".into();
        assert!(store.part_file(&location, &altered).is_err());
        let mut temporary = store.part_temporary().unwrap();
        temporary.write_all(b"old").unwrap();
        store.upsert_folder(&location.account, "INBOX", 43).unwrap();
        assert!(
            store
                .save_part(&location, &manifest.parts[0], temporary)
                .is_err()
        );
        assert!(store.mime_structure(&location).is_err());
    }
    #[test]
    fn parts_integrity_failure_is_not_treated_as_cache_miss_and_schema12_upgrade_is_backed_up() {
        let (root, store, location, manifest) = fixture();
        store
            .connection
            .execute_batch(
                "DROP TABLE notification_candidates; DROP TABLE notification_pending; DROP TABLE notification_baselines; DROP TABLE desktop_settings; DROP VIEW protected_objects; DROP TRIGGER cache_body_insert; DROP TRIGGER cache_part_insert; DROP TABLE cache_access; DROP TABLE cache_settings; DROP TRIGGER stop_sent_draft_sync; DROP TRIGGER track_draft_scan_account; DROP TABLE draft_conflicts; DROP TABLE draft_scan_failures; DROP TABLE draft_scans; DROP TRIGGER track_server_draft_insert; DROP TRIGGER track_server_draft_update; DROP TABLE draft_uploads; DROP TABLE draft_sync; DROP TABLE server_draft_sources; DROP TABLE server_draft_folders; DROP TABLE part_resources; DROP TABLE message_structures; PRAGMA user_version=12;",
            )
            .unwrap();
        drop(store);
        let mut store = Store::open(&root.path().join("data")).unwrap();
        assert!(
            std::fs::read_dir(root.path().join("data"))
                .unwrap()
                .any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("migration-v12-"))
        );
        store.save_mime_structure(&location, &manifest).unwrap();
        let mut file = store.part_temporary().unwrap();
        file.write_all(b"hello").unwrap();
        store
            .save_part(&location, &manifest.parts[0], file)
            .unwrap();
        let id: String = store
            .connection
            .query_row("SELECT object_id FROM part_resources", [], |r| r.get(0))
            .unwrap();
        let mut corrupted = store.objects.file(&id, false).unwrap();
        corrupted.rewind().unwrap();
        corrupted.write_all(b"xxxxx").unwrap();
        corrupted.sync_all().unwrap();
        assert!(store.part_file(&location, &manifest.parts[0]).is_err());
    }
}
