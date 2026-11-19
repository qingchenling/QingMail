//! 固定上传意图先落盘；重启/丢响应只对同一MIME查证，不重新APPEND。
use crate::*;
use mail_core::DraftId;
use mail_protocol::DraftSyncInfo;
#[derive(Debug)]
pub struct DraftUploadCandidate {
    pub draft: StoredDraft,
    pub source: Option<mail_protocol::ServerDraftSource>,
    pub folder: String,
    pub previous: Option<DraftUpload>,
    pub lineage: String,
}
#[derive(Debug, Clone)]
pub struct DraftUpload {
    pub id: String,
    pub account: AccountId,
    pub draft: DraftId,
    pub revision: u32,
    pub folder: String,
    pub expected_epoch: Option<u32>,
    pub raw: Vec<u8>,
    pub probe_only: bool,
}
impl Store {
    pub fn draft_sync_status(&self, account: &AccountId) -> Result<Vec<DraftSyncInfo>> {
        let mut q=self.connection.prepare("SELECT s.draft_id,d.subject,d.revision,s.base_revision,s.state,s.detail,s.base_source,s.changed_at FROM draft_sync s JOIN drafts d ON d.account_id=s.account_id AND d.id=s.draft_id WHERE s.account_id=?1 ORDER BY s.changed_at DESC,s.draft_id LIMIT 200")?;
        Ok(q.query_map([&account.0], |r| {
            Ok(DraftSyncInfo {
                updated_at: r.get(7)?,
                draft: DraftId(r.get(0)?),
                subject: r.get(1)?,
                revision: r.get(2)?,
                base_revision: r.get(3)?,
                state: r.get(4)?,
                detail: r.get(5)?,
                source: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?)
    }
    pub fn next_draft_upload(&mut self) -> Result<Option<DraftUploadCandidate>> {
        // 一个任务取完立即让出账号；失败后退避60秒，不阻塞其他账号。
        let value:Option<(String,String,Option<String>)>=self.connection.query_row("SELECT s.account_id,s.draft_id,s.base_source FROM draft_sync s JOIN drafts d ON d.account_id=s.account_id AND d.id=s.draft_id JOIN account_options a ON a.account_id=s.account_id AND a.paused=0 WHERE s.state IN ('pending','uncertain') AND d.revision>s.base_revision AND s.changed_at<=unixepoch()-5 AND s.attempted_at<=unixepoch()-60 AND NOT EXISTS(SELECT 1 FROM send_tasks t WHERE t.account_id=s.account_id AND t.source_draft=s.draft_id AND t.source_revision>=d.revision) ORDER BY s.attempted_at,s.changed_at,s.draft_id LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((account, id, source)) = value else {
            return Ok(None);
        };
        let account = AccountId(account);
        let id = DraftId(id);
        let Some(folder) = self.server_draft_folder(&account)? else {
            self.connection.execute("UPDATE draft_sync SET attempted_at=unixepoch(),detail='请选择服务器草稿目录' WHERE account_id=?1 AND draft_id=?2",params![account.0,id.0])?;
            return Ok(None);
        };
        let draft = self.draft(&account, &id)?;
        self.connection.execute(
            "UPDATE draft_sync SET attempted_at=unixepoch() WHERE account_id=?1 AND draft_id=?2",
            params![account.0, id.0],
        )?;
        if draft.document.body.text().is_empty()
            && draft.document.headers == mail_core::DraftHeaders::default()
            && draft.document.attachments.is_empty()
        {
            return Ok(None);
        }
        let upload:Option<String>=self.connection.query_row("SELECT id FROM draft_uploads WHERE account_id=?1 AND draft_id=?2 AND state IN ('prepared','in_flight','uncertain') ORDER BY created_at,id LIMIT 1",params![account.0,id.0],|r|r.get(0)).optional()?;
        let marker: Option<String> = self.connection.query_row(
            "SELECT lineage FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
            params![account.0, id.0],
            |r| r.get(0),
        )?;
        let lineage = if let Some(marker) = marker {
            marker
        } else if let Some(source) = &source {
            mail_content::server_draft_identity(&self.server_draft_original(&account, source)?)
                .map(|(_, v)| v)
                .unwrap_or_else(|| id.0.clone())
        } else {
            id.0.clone()
        };
        Ok(Some(DraftUploadCandidate {
            lineage,
            source: source
                .map(|s| self.server_draft_source(&account, &s))
                .transpose()?,
            draft,
            folder,
            previous: upload
                .map(|u| self.draft_upload(&account, &u))
                .transpose()?,
        }))
    }
    pub fn prepare_draft_upload(
        &mut self,
        draft: &StoredDraft,
        folder: &str,
        expected_epoch: Option<u32>,
        id: &str,
        raw: &[u8],
    ) -> Result<DraftUpload> {
        if !mail_protocol::valid_id(id)
            || raw.is_empty()
            || raw.len() > mail_content::MAX_SERVER_DRAFT_BYTES
            || self.draft(&draft.account, &draft.id)?.revision != draft.revision
            || self.server_draft_folder(&draft.account)?.as_deref() != Some(folder)
        {
            return Err(Error::Conflict);
        }
        let old: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM draft_uploads WHERE account_id=?1 AND draft_id=?2 AND revision=?3",
                params![draft.account.0, draft.id.0, draft.revision],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            return self.draft_upload(&draft.account, &old);
        }
        let object = objects::Object::write(
            &self.objects,
            &draft.account,
            "attachment",
            &mut &raw[..],
            mail_content::MAX_SERVER_DRAFT_BYTES as u64,
        )?;
        let result: Result<()> = (|| {
            let tx = self.connection.transaction()?;
            object.insert(&tx)?;
            tx.execute("INSERT INTO draft_uploads(id,account_id,draft_id,revision,remote_name,expected_epoch,raw_id,state) VALUES (?1,?2,?3,?4,?5,?6,?7,'prepared')",params![id,draft.account.0,draft.id.0,draft.revision,folder,expected_epoch,object.id])?;
            tx.commit()?;
            Ok(())
        })();
        if result.is_err() {
            self.cleanup_failed_object(&object.id);
        }
        result?;
        self.draft_upload(&draft.account, id)
    }
    pub fn draft_upload(&self, account: &AccountId, id: &str) -> Result<DraftUpload> {
        let (draft,revision,folder,epoch,raw,state):(String,u32,String,Option<u32>,String,String)=self.connection.query_row("SELECT draft_id,revision,remote_name,expected_epoch,raw_id,state FROM draft_uploads WHERE id=?1 AND account_id=?2",params![id,account.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        if !matches!(state.as_str(), "prepared" | "in_flight" | "uncertain") {
            return Err(Error::Conflict);
        }
        Ok(DraftUpload {
            id: id.into(),
            account: account.clone(),
            draft: DraftId(draft),
            revision,
            folder,
            expected_epoch: epoch,
            raw: objects::read(
                &self.objects,
                &self.connection,
                account,
                &raw,
                "attachment",
                mail_content::MAX_SERVER_DRAFT_BYTES as u64,
            )?,
            probe_only: state != "prepared",
        })
    }
    /// 必须在发出第一个APPEND字节前成功持久化；返回的bool表示只能查询。
    pub fn begin_draft_upload(&mut self, account: &AccountId, id: &str) -> Result<bool> {
        let current = self.draft_upload(account, id)?;
        let blocked: bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM draft_sync WHERE account_id=?1 AND draft_id=?2 AND state='stopped') OR EXISTS(SELECT 1 FROM account_options WHERE account_id=?1 AND paused=1) OR EXISTS(SELECT 1 FROM send_tasks WHERE account_id=?1 AND source_draft=?2 AND source_revision>=?3)",params![account.0,current.draft.0,current.revision],|r|r.get(0))?;
        if blocked {
            return Err(Error::Conflict);
        }
        let tx = self.connection.transaction()?;
        tx.execute("UPDATE draft_uploads SET state='in_flight' WHERE id=?1 AND account_id=?2 AND state='prepared'",params![id,account.0])?;
        tx.execute("UPDATE draft_sync SET state='uncertain',detail='上传执行中，等待服务器核验' WHERE account_id=?1 AND draft_id=?2",params![account.0,current.draft.0])?;
        tx.commit()?;
        Ok(current.probe_only)
    }

    pub fn fail_draft_sync_branch(
        &mut self,
        account: &AccountId,
        draft: &DraftId,
        lineage: &str,
        state: &str,
        detail: &str,
    ) -> Result<()> {
        let branch: Option<String> = self
            .connection
            .query_row(
                "SELECT lineage FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
                params![account.0, draft.0],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if branch.as_deref().is_some_and(|v| v != lineage) {
            return Ok(());
        }
        self.fail_draft_sync(account, draft, state, detail)
    }
    pub fn fail_draft_sync(
        &mut self,
        account: &AccountId,
        draft: &DraftId,
        state: &str,
        detail: &str,
    ) -> Result<()> {
        if !matches!(state, "pending" | "failed" | "conflict" | "uncertain") || detail.len() > 2048
        {
            return Err(Error::Invalid("草稿同步状态无效"));
        }
        let stopped: bool = self.connection.query_row(
            "SELECT state='stopped' FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
            params![account.0, draft.0],
            |r| r.get(0),
        )?;
        if stopped {
            return Ok(());
        }
        if state == "conflict" {
            let remote: Option<String> = self.connection.query_row("SELECT n.id FROM draft_sync d JOIN server_draft_sources b ON b.id=d.base_source JOIN server_draft_sources n ON n.account_id=b.account_id AND n.remote_name=b.remote_name AND n.uidvalidity=b.uidvalidity AND n.uid=b.uid AND n.id!=b.id WHERE d.account_id=?1 AND d.draft_id=?2 ORDER BY n.created_at DESC,n.rowid DESC LIMIT 1", params![account.0,draft.0],|r|r.get(0)).optional()?;
            return self.record_draft_conflict(account, draft, remote.as_deref(), detail);
        }
        if state == "uncertain" {
            self.record_draft_conflict(account, draft, None, detail)?;
        }
        self.connection.execute("UPDATE draft_sync SET state=?1,detail=?2,attempted_at=unixepoch() WHERE account_id=?3 AND draft_id=?4",params![state,detail,account.0,draft.0])?;
        Ok(())
    }
    pub fn complete_draft_upload(
        &mut self,
        upload: &DraftUpload,
        epoch: u32,
        uid: u32,
    ) -> Result<()> {
        let source =
            self.archive_server_draft(&upload.account, &upload.folder, epoch, uid, &upload.raw)?;
        let tx = self.connection.transaction()?;
        if tx.execute("UPDATE draft_uploads SET state='applied' WHERE id=?1 AND account_id=?2 AND state IN ('in_flight','uncertain')",params![upload.id,upload.account.0])?!=1{return Err(Error::Conflict);}
        tx.execute("UPDATE draft_conflicts SET state='upload_verified' WHERE account_id=?1 AND draft_id=?2 AND state='open'",params![upload.account.0,upload.draft.0])?;
        tx.execute(
            "UPDATE server_draft_sources SET converted=?1,baseline_revision=?2 WHERE id=?3",
            params![upload.draft.0, upload.revision, source.id],
        )?;
        tx.execute("UPDATE draft_sync SET base_source=?1,base_revision=?2,state=CASE WHEN (SELECT revision FROM drafts WHERE id=?3 AND account_id=?4)=?2 THEN 'synced' ELSE 'pending' END,detail='已核验服务器新版本；旧版本保留' WHERE draft_id=?3 AND account_id=?4",params![source.id,upload.revision,upload.draft.0,upload.account.0])?;
        tx.commit()?;
        Ok(())
    }
    /// 只有协议层证明未执行APPEND时可恢复prepared；未知结果没有此路径。
    pub fn defer_draft_upload(&mut self, account: &AccountId, id: &str) -> Result<()> {
        if self.connection.execute("UPDATE draft_uploads SET state='prepared' WHERE id=?1 AND account_id=?2 AND state='in_flight'",params![id,account.0])? != 1 {return Err(Error::Conflict);}
        Ok(())
    }
    pub fn flush_draft_sync(&mut self, account: &AccountId) -> Result<()> {
        self.connection.execute("UPDATE draft_sync SET changed_at=unixepoch()-5,attempted_at=0 WHERE account_id=?1 AND state='pending'",[&account.0])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(store: &mut Store) -> AccountId {
        let server = ServerSettings {
            host: "example.invalid".into(),
            port: 993,
            username: "fixture".into(),
            security: ConnectionSecurity::Tls,
        };
        let account = store
            .create_account(&AccountSettings {
                display_name: "drafts".into(),
                address: "me@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        store
            .replace_remote_folders(
                &account,
                &[mail_core::RemoteFolder {
                    name: "Drafts".into(),
                    display_name: "Drafts".into(),
                    delimiter: None,
                    selectable: true,
                }],
                0,
            )
            .unwrap();
        store.set_server_draft_folder(&account, "Drafts").unwrap();
        account
    }
    fn document(text: &str) -> DraftDocument {
        DraftDocument {
            headers: Default::default(),
            body: mail_content::draft::Draft::plain(text).unwrap(),
            attachments: vec![],
        }
    }
    #[test]
    fn frozen_upload_survives_new_edit_restart_and_recovery_never_regrants_append() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut s = Store::open(&data).unwrap();
        let a = setup(&mut s);
        let b = setup(&mut s);
        let d = s.create_draft(&a, &document("first")).unwrap();
        s.flush_draft_sync(&a).unwrap();
        let candidate = s.next_draft_upload().unwrap().unwrap();
        assert_eq!(candidate.draft, d);
        let id = "a".repeat(32);
        let raw = b"Subject: frozen\r\n\r\nfirst";
        let upload = s
            .prepare_draft_upload(&d, "Drafts", None, &id, raw)
            .unwrap();
        assert!(!upload.probe_only);
        assert!(s.begin_draft_upload(&b, &id).is_err());
        assert!(!s.begin_draft_upload(&a, &id).unwrap());
        let updated = s
            .save_draft(&a, &d.id, d.revision, &document("later local edit"))
            .unwrap();
        drop(s);
        let mut s = Store::open(&data).unwrap();
        let recovered = s.draft_upload(&a, &id).unwrap();
        assert!(recovered.probe_only);
        assert_eq!(recovered.raw, raw);
        assert_eq!(recovered.revision, d.revision);
        assert!(s.begin_draft_upload(&a, &id).unwrap());
        s.complete_draft_upload(&recovered, 42, 7).unwrap();
        let status = s.draft_sync_status(&a).unwrap();
        assert_eq!(status[0].state, "pending");
        assert_eq!(status[0].base_revision, d.revision);
        assert_eq!(s.draft(&a, &d.id).unwrap(), updated);
        assert!(s.draft_upload(&a, &id).is_err());
        assert!(s.complete_draft_upload(&recovered, 42, 7).is_err());
        let source = s.server_draft_sources(&a).unwrap();
        assert_eq!(s.server_draft_original(&a, &source[0].id).unwrap(), raw);
        s.remove_account(&b).unwrap();
    }
    #[test]
    fn proven_rejection_can_defer_but_uncertain_edits_cannot_escape_frozen_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let d = s.create_draft(&a, &document("pending")).unwrap();
        let id = "b".repeat(32);
        s.prepare_draft_upload(&d, "Drafts", Some(42), &id, b"Subject: fixed\r\n\r\nfixed")
            .unwrap();
        s.begin_draft_upload(&a, &id).unwrap();
        s.defer_draft_upload(&a, &id).unwrap();
        assert!(!s.draft_upload(&a, &id).unwrap().probe_only);
        s.begin_draft_upload(&a, &id).unwrap();
        s.fail_draft_sync(&a, &d.id, "uncertain", "lost response")
            .unwrap();
        s.save_draft(&a, &d.id, d.revision, &document("new edit"))
            .unwrap();
        s.connection
            .execute("UPDATE draft_sync SET changed_at=0,attempted_at=0", [])
            .unwrap();
        let next = s.next_draft_upload().unwrap().unwrap();
        assert_eq!(next.previous.unwrap().id, id);
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "uncertain");
        s.fail_draft_sync(&a, &d.id, "conflict", "epoch changed")
            .unwrap();
        s.flush_draft_sync(&a).unwrap();
        assert!(s.next_draft_upload().unwrap().is_none());
    }
    #[test]
    fn schema14_upgrade_links_converted_baseline_and_preserves_originals() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut s = Store::open(&data).unwrap();
        let a = setup(&mut s);
        let source = s
            .archive_server_draft(&a, "Drafts", 42, 7, b"Subject: remote\r\n\r\ncontent")
            .unwrap();
        let draft = s.convert_server_draft(&a, &source.id).unwrap();
        s.connection.execute_batch("DROP TABLE autostart_intent; DROP TABLE notification_candidates; DROP TABLE notification_pending; DROP TABLE notification_baselines; DROP TABLE desktop_settings; DROP VIEW protected_objects; DROP TRIGGER cache_body_insert; DROP TRIGGER cache_part_insert; DROP TABLE cache_access; DROP TABLE cache_settings; DROP TRIGGER stop_sent_draft_sync; DROP TRIGGER track_draft_scan_account; DROP TABLE draft_conflicts; DROP TABLE draft_scan_failures; DROP TABLE draft_scans; DROP TRIGGER track_server_draft_insert; DROP TRIGGER track_server_draft_update; DROP TABLE draft_uploads; DROP TABLE draft_sync; PRAGMA user_version=14;").unwrap();
        drop(s);
        let mut s = Store::open(&data).unwrap();
        let status = s.draft_sync_status(&a).unwrap();
        assert_eq!(status[0].base_revision, draft.revision);
        assert_eq!(status[0].state, "synced");
        assert_eq!(status[0].source, Some(source.id));
        assert!(s.next_draft_upload().unwrap().is_none());
        assert!(std::fs::read_dir(data).unwrap().any(|p| {
            p.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("migration-v14-")
        }));
    }
}
