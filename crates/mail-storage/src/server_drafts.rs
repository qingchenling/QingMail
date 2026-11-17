//! 服务器草稿原件与显式转换副本；保留旧UID/旧版本，不把阅读派生MIME写回服务器。
use crate::*;
use mail_core::DraftId;
use mail_protocol::ServerDraftSource;
impl Store {
    pub fn server_draft_folder(&self, account: &AccountId) -> Result<Option<String>> {
        self.require_remote_draft_account(account)?;
        Ok(self
            .connection
            .query_row(
                "SELECT d.remote_name FROM server_draft_folders d JOIN remote_folders f ON f.account_id=d.account_id AND f.remote_name=d.remote_name WHERE d.account_id=?1 AND f.selectable=1",
                [&account.0],
                |r| r.get(0),
            )
            .optional()?)
    }
    fn require_remote_draft_account(&self, account: &AccountId) -> Result<()> {
        if self.account(account)?.is_none() {
            return Err(Error::Invalid("服务器草稿需要远端账号"));
        }
        Ok(())
    }
    /// 显式选择必须来自本账号最新LIST，不猜测创建文件夹。
    pub fn set_server_draft_folder(&mut self, account: &AccountId, folder: &str) -> Result<()> {
        self.require_remote_draft_account(account)?;
        if !self
            .remote_folders(account)?
            .iter()
            .any(|f| f.selectable && f.name == folder)
        {
            return Err(Error::Invalid("请选择本账号已列出的可选文件夹"));
        }
        self.connection.execute("INSERT INTO server_draft_folders VALUES (?1,?2,1) ON CONFLICT(account_id) DO UPDATE SET remote_name=excluded.remote_name,explicit=1", params![account.0,folder])?;
        Ok(())
    }
    /// 当前唯一特殊用途目录才自动选择；旧选择失效或特殊用途有歧义时返回None。
    pub fn discover_server_draft_folder(
        &mut self,
        account: &AccountId,
        special: &[String],
    ) -> Result<Option<String>> {
        self.require_remote_draft_account(account)?;
        let folders = self.remote_folders(account)?;
        let old: Option<(String, bool)> = self
            .connection
            .query_row(
                "SELECT remote_name,explicit FROM server_draft_folders WHERE account_id=?1",
                [&account.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((name, true)) = old {
            return Ok(folders
                .iter()
                .any(|f| f.selectable && f.name == name)
                .then_some(name));
        }
        let valid = special
            .iter()
            .filter(|n| folders.iter().any(|f| f.selectable && &f.name == *n))
            .collect::<std::collections::BTreeSet<_>>();
        self.connection.execute(
            "DELETE FROM server_draft_folders WHERE account_id=?1 AND explicit=0",
            [&account.0],
        )?;
        if valid.len() != 1 {
            return Ok(None);
        }
        let name = (*valid.first().expect("one")).clone();
        self.connection.execute(
            "INSERT INTO server_draft_folders VALUES (?1,?2,0)",
            params![account.0, name],
        )?;
        Ok(Some(name))
    }
    pub fn archive_server_draft(
        &mut self,
        account: &AccountId,
        folder: &str,
        epoch: u32,
        uid: u32,
        raw: &[u8],
    ) -> Result<ServerDraftSource> {
        use sha2::{Digest, Sha256};
        self.require_remote_draft_account(account)?;
        if self.server_draft_folder(account)?.as_deref() != Some(folder)
            || epoch == 0
            || uid == 0
            || raw.is_empty()
            || raw.len() > mail_content::MAX_MESSAGE_BYTES
        {
            return Err(Error::Invalid("草稿目录、定位或原件大小无效"));
        }
        let hash = format!("{:x}", Sha256::digest(raw));
        let old:Option<String>=self.connection.query_row("SELECT id FROM server_draft_sources WHERE account_id=?1 AND remote_name=?2 AND uidvalidity=?3 AND uid=?4 AND raw_hash=?5",params![account.0,folder,epoch,uid,hash],|r|r.get(0)).optional()?;
        if let Some(id) = old {
            return self.server_draft_source(account, &id);
        }
        let content = mail_content::parse(raw).map_err(|_| Error::Invalid("原件无法读取"))?;
        let object = objects::Object::write(
            &self.objects,
            account,
            "attachment",
            &mut &raw[..],
            mail_content::MAX_MESSAGE_BYTES as u64,
        )?;
        let id = new_id()?;
        let result: Result<()> = (|| {
            let tx = self.connection.transaction()?;
            object.insert(&tx)?;
            tx.execute("INSERT INTO server_draft_sources(id,account_id,remote_name,uidvalidity,uid,raw_id,raw_hash,subject) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![id,account.0,folder,epoch,uid,object.id,hash,content.subject])?;
            tx.commit()?;
            Ok(())
        })();
        if result.is_err() {
            self.cleanup_failed_object(&object.id);
        }
        result?;
        self.server_draft_source(account, &id)
    }
    pub fn server_draft_sources(&self, account: &AccountId) -> Result<Vec<ServerDraftSource>> {
        self.require_remote_draft_account(account)?;
        let mut query=self.connection.prepare("SELECT id FROM server_draft_sources WHERE account_id=?1 ORDER BY created_at DESC,rowid DESC LIMIT 200")?;
        let ids = query
            .query_map([&account.0], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.iter()
            .map(|id| self.server_draft_source(account, id))
            .collect()
    }
    pub fn server_draft_source(&self, account: &AccountId, id: &str) -> Result<ServerDraftSource> {
        Ok(self.connection.query_row("SELECT s.remote_name,s.uidvalidity,s.uid,s.subject,o.size,s.converted FROM server_draft_sources s JOIN objects o ON o.id=s.raw_id AND o.account_id=s.account_id WHERE s.id=?1 AND s.account_id=?2",params![id,account.0],|r|Ok(ServerDraftSource{id:id.into(),account:account.clone(),folder:r.get(0)?,uidvalidity:r.get(1)?,uid:r.get(2)?,subject:r.get(3)?,size:r.get(4)?,converted:r.get::<_,Option<String>>(5)?.map(DraftId)}))?)
    }
    pub fn server_draft_original(&self, account: &AccountId, id: &str) -> Result<Vec<u8>> {
        let raw: String = self.connection.query_row(
            "SELECT raw_id FROM server_draft_sources WHERE id=?1 AND account_id=?2",
            params![id, account.0],
            |r| r.get(0),
        )?;
        objects::read(
            &self.objects,
            &self.connection,
            account,
            &raw,
            "attachment",
            mail_content::MAX_MESSAGE_BYTES as u64,
        )
    }
    pub(super) fn server_copy_document(
        &mut self,
        account: &AccountId,
        copy: mail_content::EditableServerCopy,
    ) -> Result<DraftDocument> {
        let mut attachments = vec![];
        for attachment in copy.attachments {
            attachments.push(
                self.import_attachment(
                    account,
                    &attachment.name,
                    &attachment.media_type,
                    &mut attachment.bytes.as_ref(),
                )?
                .id,
            );
        }
        Ok(DraftDocument {
            headers: mail_core::DraftHeaders {
                subject: copy.subject,
                to: copy.to,
                cc: copy.cc,
                bcc: copy.bcc,
            },
            body: copy.body,
            attachments,
        })
    }
    /// 仅由操作者明确选择“转换副本”调用；保存附件、草稿和转换关联在同一savepoint。
    pub fn convert_server_draft(&mut self, account: &AccountId, id: &str) -> Result<StoredDraft> {
        let source = self.server_draft_source(account, id)?;
        if let Some(local) = source.converted {
            return self.draft(account, &local);
        }
        let raw = self.server_draft_original(account, id)?;
        let copy = mail_content::editable_server_copy(&raw)
            .map_err(|_| Error::Invalid("原件超出编辑模型或含无法转换内容，已保留原件"))?;
        self.connection
            .execute_batch("SAVEPOINT convert_server_draft")?;
        let result = (|| {
            let document = self.server_copy_document(account, copy)?;
            let draft = self.create_draft(account, &document)?;
            self.connection.execute("UPDATE server_draft_sources SET converted=?1,baseline_revision=?2 WHERE id=?3 AND account_id=?4 AND converted IS NULL",params![draft.id.0,draft.revision,id,account.0])?;
            self.connection.execute("UPDATE draft_sync SET base_source=?1,base_revision=?2,state='synced',detail='已转换，等待本地编辑' WHERE account_id=?3 AND draft_id=?4", params![id,draft.revision,account.0,draft.id.0])?;
            Ok(draft)
        })();
        if result.is_ok() {
            self.connection
                .execute_batch("RELEASE convert_server_draft")?;
        } else {
            self.connection
                .execute_batch("ROLLBACK TO convert_server_draft; RELEASE convert_server_draft")?;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(store: &mut Store) -> AccountId {
        let server = ServerSettings {
            host: "mail.example.invalid".into(),
            port: 993,
            username: "test".into(),
            security: ConnectionSecurity::Tls,
        };
        let a = store
            .create_account(&AccountSettings {
                display_name: "test".into(),
                address: "me@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        store
            .replace_remote_folders(
                &a,
                &[mail_core::RemoteFolder {
                    name: "Drafts".into(),
                    display_name: "Drafts".into(),
                    delimiter: Some("/".into()),
                    selectable: true,
                }],
                0,
            )
            .unwrap();
        a
    }
    #[test]
    fn folder_ambiguity_and_account_scoped_immutable_originals_survive_restart() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut s = Store::open(&data).unwrap();
        let a = setup(&mut s);
        let b = setup(&mut s);
        assert_eq!(s.discover_server_draft_folder(&a, &[]).unwrap(), None);
        assert_eq!(
            s.discover_server_draft_folder(&a, &["Drafts".into()])
                .unwrap(),
            Some("Drafts".into())
        );
        s.set_server_draft_folder(&b, "Drafts").unwrap();
        assert!(s.set_server_draft_folder(&b, "Unknown").is_err());
        let original = b"Subject: original\r\n\r\nhello";
        let first = s
            .archive_server_draft(&a, "Drafts", 42, 7, original)
            .unwrap();
        assert_eq!(
            s.archive_server_draft(&a, "Drafts", 42, 7, original)
                .unwrap()
                .id,
            first.id
        );
        let changed = s
            .archive_server_draft(&a, "Drafts", 42, 7, b"Subject: changed\r\n\r\nworld")
            .unwrap();
        let other = s
            .archive_server_draft(&b, "Drafts", 42, 7, original)
            .unwrap();
        assert_ne!(first.id, changed.id);
        assert_ne!(first.id, other.id);
        assert!(s.server_draft_original(&b, &first.id).is_err());
        drop(s);
        let mut s = Store::open(&data).unwrap();
        assert_eq!(s.server_draft_original(&a, &first.id).unwrap(), original);
        assert_eq!(s.server_draft_sources(&a).unwrap().len(), 2);
        let local = s.convert_server_draft(&a, &first.id).unwrap();
        assert_eq!(local.document.body.text(), "hello");
        let again = s.convert_server_draft(&a, &first.id).unwrap();
        assert_eq!(local.id, again.id);
        assert_eq!(s.server_draft_original(&a, &first.id).unwrap(), original);
        let updated = DraftDocument {
            headers: local.document.headers.clone(),
            body: mail_content::draft::Draft::plain("offline changed").unwrap(),
            attachments: vec![],
        };
        s.save_draft(&a, &local.id, local.revision, &updated)
            .unwrap();
        assert_eq!(s.server_draft_original(&a, &first.id).unwrap(), original);
        assert_eq!(
            s.convert_server_draft(&a, &first.id)
                .unwrap()
                .document
                .body
                .text(),
            "offline changed"
        );
        s.remove_account(&b).unwrap();
        assert_eq!(s.server_draft_sources(&a).unwrap().len(), 2);
    }
    #[test]
    fn conversion_keeps_bcc_and_attachments_and_rolls_back_failed_conversion() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        s.set_server_draft_folder(&a, "Drafts").unwrap();
        let raw=b"Subject: draft\r\nBcc: hidden@example.invalid\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nhello\r\n--x\r\nContent-Type: text/plain; name=ok.txt\r\nContent-Disposition: attachment; filename=ok.txt\r\n\r\nattachment\r\n--x--\r\n";
        let source = s.archive_server_draft(&a, "Drafts", 42, 1, raw).unwrap();
        let draft = s.convert_server_draft(&a, &source.id).unwrap();
        assert_eq!(draft.document.headers.bcc, vec!["hidden@example.invalid"]);
        assert_eq!(draft.document.attachments.len(), 1);
        let bad = String::from_utf8(raw.to_vec())
            .unwrap()
            .replace("--x--\r\n", "--x\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=../bad\r\n\r\nbad\r\n--x--\r\n");
        let source = s
            .archive_server_draft(&a, "Drafts", 42, 2, bad.as_bytes())
            .unwrap();
        let before = s.draft_ids(&a).unwrap().len();
        assert!(s.convert_server_draft(&a, &source.id).is_err());
        assert_eq!(s.draft_ids(&a).unwrap().len(), before);
        assert!(
            s.server_draft_source(&a, &source.id)
                .unwrap()
                .converted
                .is_none()
        );
        assert_eq!(
            s.server_draft_original(&a, &source.id).unwrap(),
            bad.as_bytes()
        );
        assert_eq!(
            s.connection
                .query_row(
                    "SELECT COUNT(*) FROM attachments WHERE account_id=?1",
                    [&a.0],
                    |r| r.get::<_, u32>(0)
                )
                .unwrap(),
            1
        );
    }
    #[test]
    fn schema13_upgrade_preserves_drafts_and_creates_backup() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut s = Store::open(&data).unwrap();
        let a = setup(&mut s);
        let draft = s
            .create_draft(
                &a,
                &DraftDocument {
                    headers: Default::default(),
                    body: mail_content::draft::Draft::plain("before migration").unwrap(),
                    attachments: vec![],
                },
            )
            .unwrap();
        s.connection.execute_batch("DROP TRIGGER track_server_draft_insert; DROP TRIGGER track_server_draft_update; DROP TABLE draft_uploads; DROP TABLE draft_sync; DROP TABLE server_draft_sources; DROP TABLE server_draft_folders; PRAGMA user_version=13;").unwrap();
        drop(s);
        let s = Store::open(&data).unwrap();
        assert_eq!(s.draft(&a, &draft.id).unwrap(), draft);
        assert_eq!(
            s.connection
                .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            15
        );
        assert!(std::fs::read_dir(&data).unwrap().any(|p| {
            p.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("migration-v13-")
        }));
    }
}
