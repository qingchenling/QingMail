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
    pub deleting: bool,
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
        let mut q=self.connection.prepare("SELECT s.draft_id,d.subject,d.revision,s.base_revision,CASE WHEN d.deleted_at IS NOT NULL THEN 'deleting' WHEN s.local_only=1 THEN 'stopped' WHEN EXISTS(SELECT 1 FROM draft_uploads u WHERE u.account_id=s.account_id AND u.draft_id=s.draft_id AND u.state IN ('in_flight','uncertain')) THEN 'uncertain' ELSE s.state END,s.detail,s.base_source,s.changed_at FROM draft_sync s JOIN drafts d ON d.account_id=s.account_id AND d.id=s.draft_id WHERE s.account_id=?1 ORDER BY s.changed_at DESC,s.draft_id LIMIT 200")?;
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
        let value:Option<(String,String,Option<String>,bool)>=self.connection.query_row(
            "SELECT s.account_id,s.draft_id,s.base_source,d.deleted_at IS NOT NULL FROM draft_sync s
             JOIN drafts d ON d.account_id=s.account_id AND d.id=s.draft_id
             JOIN account_options a ON a.account_id=s.account_id AND a.paused=0
             WHERE (s.local_only=0 OR d.deleted_at IS NOT NULL) AND s.changed_at<=unixepoch()-5 AND s.attempted_at<=unixepoch()-60 AND (
               (s.state IN ('pending','uncertain') AND d.revision>s.base_revision
                AND NOT EXISTS(SELECT 1 FROM send_tasks t WHERE t.account_id=s.account_id AND t.source_draft=s.draft_id AND t.source_revision>=d.revision))
               OR EXISTS(SELECT 1 FROM draft_uploads u WHERE u.account_id=s.account_id AND u.draft_id=s.draft_id AND u.state IN ('in_flight','uncertain')) OR d.deleted_at IS NOT NULL
             ) ORDER BY s.attempted_at,s.changed_at,s.draft_id LIMIT 1",
            [],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let Some((account, id, source, deleting)) = value else {
            return Ok(None);
        };
        let account = AccountId(account);
        let id = DraftId(id);
        let folder = self.server_draft_folder(&account)?.unwrap_or_default();
        if folder.is_empty() && !deleting {
            self.connection.execute("UPDATE draft_sync SET attempted_at=unixepoch(),detail='请选择服务器草稿目录' WHERE account_id=?1 AND draft_id=?2",params![account.0,id.0])?;
            return Ok(None);
        }
        let draft = self.draft_including_deleted(&account, &id)?;
        self.connection.execute(
            "UPDATE draft_sync SET attempted_at=unixepoch() WHERE account_id=?1 AND draft_id=?2",
            params![account.0, id.0],
        )?;
        // Resolve a frozen upload before considering the current editor contents.
        // A newer empty revision must not strand an older unknown APPEND.
        let upload:Option<String>=self.connection.query_row("SELECT id FROM draft_uploads WHERE account_id=?1 AND draft_id=?2 AND state IN ('prepared','in_flight','uncertain') ORDER BY CASE WHEN state='prepared' THEN 1 ELSE 0 END,created_at,id LIMIT 1",params![account.0,id.0],|r|r.get(0)).optional()?;
        if !deleting
            && upload.is_none()
            && draft.document.body.text().is_empty()
            && draft.document.headers == mail_core::DraftHeaders::default()
            && draft.document.attachments.is_empty()
        {
            return Ok(None);
        }
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
        self.connection.execute("UPDATE draft_sync SET lineage=COALESCE(lineage,?1) WHERE account_id=?2 AND draft_id=?3",params![lineage,account.0,id.0])?;
        Ok(Some(DraftUploadCandidate {
            deleting,
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
        let blocked: bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM account_options WHERE account_id=?1 AND paused=1) OR EXISTS(SELECT 1 FROM draft_sync WHERE account_id=?1 AND draft_id=?2 AND local_only=1) OR (?4=0 AND (EXISTS(SELECT 1 FROM draft_sync WHERE account_id=?1 AND draft_id=?2 AND state='stopped') OR EXISTS(SELECT 1 FROM send_tasks WHERE account_id=?1 AND source_draft=?2 AND source_revision>=?3)))",params![account.0,current.draft.0,current.revision,current.probe_only],|r|r.get(0))?;
        if blocked {
            return Err(Error::Conflict);
        }
        let tx = self.connection.transaction()?;
        tx.execute("UPDATE draft_uploads SET state='in_flight' WHERE id=?1 AND account_id=?2 AND state='prepared'",params![id,account.0])?;
        tx.execute("UPDATE draft_sync SET state=CASE WHEN state='stopped' THEN state ELSE 'uncertain' END,detail='正在核对草稿上传结果' WHERE account_id=?1 AND draft_id=?2",params![account.0,current.draft.0])?;
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
        let deleted: bool = self.connection.query_row(
            "SELECT deleted_at IS NOT NULL FROM drafts WHERE account_id=?1 AND id=?2",
            params![account.0, draft.0],
            |r| r.get(0),
        )?;
        let local_only: bool = self.connection.query_row(
            "SELECT local_only FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
            params![account.0, draft.0],
            |r| r.get(0),
        )?;
        if local_only && !deleted {
            return Ok(());
        }
        let stopped: bool = self.connection.query_row(
            "SELECT state='stopped' FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
            params![account.0, draft.0],
            |r| r.get(0),
        )?;
        if stopped {
            if !deleted && matches!(state, "conflict" | "uncertain") {
                self.record_draft_conflict(account, draft, None, detail)?;
            }
            self.connection.execute(
                "UPDATE draft_sync SET state='stopped',detail=?1,attempted_at=unixepoch() WHERE account_id=?2 AND draft_id=?3",
                params![detail, account.0, draft.0],
            )?;
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
        trash: Option<&str>,
    ) -> Result<()> {
        let source =
            self.archive_server_draft(&upload.account, &upload.folder, epoch, uid, &upload.raw)?;
        let tx = self.connection.transaction()?;
        let deleted: bool = tx.query_row(
            "SELECT deleted_at IS NOT NULL FROM drafts WHERE account_id=?1 AND id=?2",
            params![upload.account.0, upload.draft.0],
            |r| r.get(0),
        )?;
        if deleted {
            if tx.execute("UPDATE draft_uploads SET state='applied' WHERE id=?1 AND account_id=?2 AND state IN ('in_flight','uncertain')",params![upload.id,upload.account.0])? != 1 { return Err(Error::Conflict); }
            tx.execute("UPDATE server_draft_sources SET converted=?1,baseline_revision=?2 WHERE id=?3 AND account_id=?4",params![upload.draft.0,upload.revision,source.id,upload.account.0])?;
            crate::outbox::finish_draft_deletion_on(&tx, &upload.account, &upload.draft, trash)?;
            tx.commit()?;
            return Ok(());
        }
        let stopped: bool = tx.query_row(
            "SELECT state='stopped' FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
            params![upload.account.0, upload.draft.0],
            |r| r.get(0),
        )?;
        if stopped {
            if tx.execute("UPDATE draft_uploads SET state='applied' WHERE id=?1 AND account_id=?2 AND state IN ('in_flight','uncertain')", params![upload.id, upload.account.0])? != 1 {
                return Err(Error::Conflict);
            }
            tx.execute("UPDATE server_draft_sources SET converted=?1,baseline_revision=?2 WHERE id=?3 AND account_id=?4", params![upload.draft.0,upload.revision,source.id,upload.account.0])?;
            let sent: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM send_tasks WHERE account_id=?1 AND source_draft=?2 AND state IN ('sent','copy_pending') AND EXISTS(SELECT 1 FROM draft_sync WHERE account_id=?1 AND draft_id=?2 AND local_only=0))",
                params![upload.account.0,upload.draft.0], |r|r.get(0),
            )?;
            if sent {
                crate::outbox::queue_sent_draft_cleanup(
                    &tx,
                    &upload.account,
                    &upload.draft,
                    trash,
                )?;
                tx.execute("DELETE FROM drafts WHERE account_id=?1 AND id=?2 AND EXISTS(SELECT 1 FROM send_tasks t WHERE t.account_id=?1 AND t.source_draft=?2 AND t.source_revision=drafts.revision AND t.state IN ('sent','copy_pending')) AND NOT EXISTS(SELECT 1 FROM draft_uploads u WHERE u.account_id=?1 AND u.draft_id=?2 AND u.state IN ('in_flight','uncertain'))",params![upload.account.0,upload.draft.0])?;
            }
            tx.execute("UPDATE draft_conflicts SET state='upload_verified' WHERE account_id=?1 AND draft_id=?2 AND state='open' AND (remote_source IS NULL OR remote_source=?3)",params![upload.account.0,upload.draft.0,source.id])?;
            tx.execute("UPDATE draft_sync SET base_source=?1,base_revision=?2,detail='旧上传已核验；保持停止上传状态' WHERE account_id=?3 AND draft_id=?4",params![source.id,upload.revision,upload.account.0,upload.draft.0])?;
            tx.commit()?;
            return Ok(());
        }
        // Verifying our frozen upload does not resolve a different remote edit.
        // Preserve the common baseline and all originals until the user decides.
        let divergent: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM draft_conflicts WHERE account_id=?1 AND draft_id=?2 AND state='open' AND remote_source IS NOT NULL AND remote_source!=?3)",params![upload.account.0,upload.draft.0,source.id],|r|r.get(0))?;
        if divergent {
            if tx.execute("UPDATE draft_uploads SET state='applied' WHERE id=?1 AND account_id=?2 AND state IN ('in_flight','uncertain')",params![upload.id,upload.account.0])? != 1 { return Err(Error::Conflict); }
            tx.execute("UPDATE server_draft_sources SET converted=?1,baseline_revision=?2 WHERE id=?3 AND account_id=?4",params![upload.draft.0,upload.revision,source.id,upload.account.0])?;
            tx.execute("UPDATE draft_sync SET state='conflict',detail='此前上传已确认；另一份云端修改仍需选择，所有原件保留' WHERE account_id=?1 AND draft_id=?2",params![upload.account.0,upload.draft.0])?;
            tx.commit()?;
            return Ok(());
        }
        let previous: Vec<(String, u32, u32)> = tx
            .prepare("SELECT remote_name,uidvalidity,uid FROM server_draft_sources WHERE account_id=?1 AND converted=?2 AND id!=?3")?
            .query_map(params![upload.account.0, upload.draft.0, source.id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut moved = 0usize;
        if !previous.is_empty()
            && let Some(trash) = trash
        {
            let valid: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM remote_folders WHERE account_id=?1 AND remote_name=?2 AND selectable=1)",
                params![upload.account.0, trash],
                |row| row.get(0),
            )?;
            let pending: usize = tx.query_row(
                "SELECT COUNT(*) FROM mail_operations WHERE state!='applied'",
                [],
                |row| row.get::<_, u32>(0),
            )? as usize;
            if valid && pending + previous.len() <= 200 {
                for (name, old_epoch, old_uid) in &previous {
                    let folder: Option<(String, u32)> = tx
                    .query_row(
                        "SELECT id,uidvalidity FROM folders WHERE account_id=?1 AND remote_name=?2",
                        params![upload.account.0, name],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                    let folder = match folder {
                        Some((id, current)) if current == *old_epoch => id,
                        Some(_) => continue,
                        None => {
                            let id = new_id()?;
                            tx.execute(
                                "INSERT INTO folders VALUES (?1,?2,?3,?4)",
                                params![id, upload.account.0, name, old_epoch],
                            )?;
                            id
                        }
                    };
                    let operation = if name == trash {
                        mail_protocol::MailOperation::Delete
                    } else {
                        mail_protocol::MailOperation::Trash {
                            destination: trash.to_owned(),
                        }
                    };
                    tx.execute(
                    "INSERT INTO mail_operations(id,account_id,folder_id,uidvalidity,uid,operation,state) VALUES (?1,?2,?3,?4,?5,?6,'pending')",
                    params![new_id()?, upload.account.0, folder, old_epoch, old_uid, serde_json::to_string(&operation).expect("static operation")],
                )?;
                    moved += 1;
                }
            }
        }
        if tx.execute("UPDATE draft_uploads SET state='applied' WHERE id=?1 AND account_id=?2 AND state IN ('in_flight','uncertain')",params![upload.id,upload.account.0])?!=1{return Err(Error::Conflict);}
        tx.execute("UPDATE draft_conflicts SET state='upload_verified' WHERE account_id=?1 AND draft_id=?2 AND state='open' AND (remote_source IS NULL OR remote_source=?3)",params![upload.account.0,upload.draft.0,source.id])?;
        tx.execute(
            "UPDATE server_draft_sources SET converted=?1,baseline_revision=?2 WHERE id=?3",
            params![upload.draft.0, upload.revision, source.id],
        )?;
        let detail = if previous.is_empty() {
            "已核验服务器新版本"
        } else if moved == previous.len() {
            "已核验新版本；旧版本已排队移入垃圾箱"
        } else if moved > 0 {
            "已核验新版本；部分旧版本已排队移入垃圾箱，其余原件保留"
        } else {
            "已核验新版本；旧版本保留（未找到垃圾箱或待处理队列已满）"
        };
        tx.execute("UPDATE draft_sync SET base_source=?1,base_revision=?2,state=CASE WHEN EXISTS(SELECT 1 FROM draft_conflicts WHERE draft_id=?3 AND account_id=?4 AND state='open') THEN 'conflict' WHEN (SELECT revision FROM drafts WHERE id=?3 AND account_id=?4)=?2 THEN 'synced' ELSE 'pending' END,detail=?5 WHERE draft_id=?3 AND account_id=?4",params![source.id,upload.revision,upload.draft.0,upload.account.0,detail])?;
        tx.commit()?;
        Ok(())
    }
    /// 只有协议层证明未执行APPEND时可恢复prepared；未知结果没有此路径。
    pub fn defer_draft_upload(&mut self, account: &AccountId, id: &str) -> Result<()> {
        if self.connection.execute("UPDATE draft_uploads SET state='prepared' WHERE id=?1 AND account_id=?2 AND state='in_flight'",params![id,account.0])? != 1 {return Err(Error::Conflict);}
        Ok(())
    }
    pub fn flush_draft_sync(&mut self, account: &AccountId) -> Result<()> {
        self.connection.execute("UPDATE draft_sync SET changed_at=unixepoch()-5,attempted_at=0 WHERE account_id=?1 AND local_only=0 AND (state='pending' OR EXISTS(SELECT 1 FROM draft_uploads u WHERE u.account_id=draft_sync.account_id AND u.draft_id=draft_sync.draft_id AND u.state IN ('in_flight','uncertain')))",[&account.0])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn add_trash(store: &mut Store, account: &AccountId) {
        let mut folders = store.remote_folders(account).unwrap();
        folders.push(mail_core::RemoteFolder {
            name: "Trash".into(),
            display_name: "Trash".into(),
            delimiter: None,
            selectable: true,
        });
        store.replace_remote_folders(account, &folders, 0).unwrap();
    }

    #[test]
    fn deletion_of_unknown_upload_survives_restart_and_never_resurrects_editor() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut s = Store::open(&path).unwrap();
        let a = setup(&mut s);
        add_trash(&mut s, &a);
        let d = s.create_draft(&a, &document("delete me")).unwrap();
        let u = s
            .prepare_draft_upload(
                &d,
                "Drafts",
                Some(42),
                &"a".repeat(32),
                b"Subject: delete me\r\n\r\ndelete me",
            )
            .unwrap();
        s.begin_draft_upload(&a, &u.id).unwrap();
        s.fail_draft_sync(&a, &d.id, "uncertain", "lost response")
            .unwrap();
        assert!(
            s.delete_draft(&a, &d.id, d.revision + 1, Some("Trash"))
                .is_err()
        );
        assert_eq!(s.draft(&a, &d.id).unwrap(), d);
        assert_eq!(s.draft_conflicts(Some(&a)).unwrap().len(), 1);
        s.delete_draft(&a, &d.id, d.revision, Some("Trash"))
            .unwrap();
        assert!(s.draft_ids(&a).unwrap().is_empty());
        assert!(s.draft_conflicts(Some(&a)).unwrap().is_empty());
        assert!(
            s.save_draft(&a, &d.id, d.revision, &document("late save"))
                .is_err()
        );
        assert!(s.select_editor_draft(&a, &d.id).is_err());
        assert!(s.enqueue_draft(&a, &d.id, d.revision).is_err());
        s.fail_draft_sync(&a, &d.id, "uncertain", "late response")
            .unwrap();
        assert!(s.draft_conflicts(Some(&a)).unwrap().is_empty());
        drop(s);
        let mut s = Store::open(&path).unwrap();
        s.connection
            .execute("UPDATE draft_sync SET attempted_at=0", [])
            .unwrap();
        let candidate = s.next_draft_upload().unwrap().unwrap();
        assert!(candidate.deleting);
        let frozen = candidate.previous.unwrap();
        assert!(frozen.probe_only);
        assert_eq!(frozen.raw, u.raw);
        assert!(s.begin_draft_upload(&a, &frozen.id).unwrap());
        s.complete_draft_upload(&frozen, 42, 7, Some("Trash"))
            .unwrap();
        assert!(s.draft_ids(&a).unwrap().is_empty());
        assert!(s.draft_sync_status(&a).unwrap().is_empty());
        assert!(s.next_draft_upload().unwrap().is_none());
        let operations = s.operations(Some(&a)).unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].location.uid, 7);
        assert!(s.task_list(&a).unwrap().is_empty());
    }

    #[test]
    fn local_only_resolution_does_not_copy_and_late_failures_cannot_reopen_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut s = Store::open(&path).unwrap();
        let a = setup(&mut s);
        let d = s.create_draft(&a, &document("local")).unwrap();
        let u = s
            .prepare_draft_upload(
                &d,
                "Drafts",
                Some(42),
                &"b".repeat(32),
                b"Subject: local\r\n\r\nlocal",
            )
            .unwrap();
        s.begin_draft_upload(&a, &u.id).unwrap();
        s.fail_draft_sync(&a, &d.id, "uncertain", "lost response")
            .unwrap();
        let conflict = s.draft_conflicts(Some(&a)).unwrap().remove(0);
        s.resolve_draft_conflict(&a, &conflict.id, mail_protocol::DraftResolution::KeepLocal)
            .unwrap();
        assert_eq!(s.draft(&a, &d.id).unwrap(), d);
        assert_eq!(s.draft_ids(&a).unwrap(), vec![d.id.clone()]);
        s.fail_draft_sync(&a, &d.id, "uncertain", "late failure")
            .unwrap();
        s.record_draft_conflict(&a, &d.id, None, "late scan")
            .unwrap();
        assert!(s.draft_conflicts(Some(&a)).unwrap().is_empty());
        let edited = s
            .save_draft(&a, &d.id, d.revision, &document("new local edit"))
            .unwrap();
        s.flush_draft_sync(&a).unwrap();
        assert!(s.next_draft_upload().unwrap().is_none());
        assert!(s.begin_draft_upload(&a, &u.id).is_err());
        drop(s);
        let mut s = Store::open(&path).unwrap();
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "stopped");
        assert_eq!(s.draft(&a, &d.id).unwrap(), edited);
        assert!(s.next_draft_upload().unwrap().is_none());
        // Deleting a local-only draft re-enables only verification/cleanup of
        // the old APPEND, never upload of its newer local revision.
        s.delete_draft(&a, &d.id, edited.revision, None).unwrap();
        let candidate = s.next_draft_upload().unwrap().unwrap();
        assert!(candidate.deleting);
        assert!(candidate.previous.unwrap().probe_only);
    }

    #[test]
    fn late_verified_upload_preserves_local_only_choice_and_newer_edits() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        add_trash(&mut s, &a);
        let d = s.create_draft(&a, &document("local")).unwrap();
        let u = s
            .prepare_draft_upload(
                &d,
                "Drafts",
                Some(42),
                &"c".repeat(32),
                b"Subject: local\r\n\r\nlocal",
            )
            .unwrap();
        s.begin_draft_upload(&a, &u.id).unwrap();
        s.fail_draft_sync(&a, &d.id, "uncertain", "lost response")
            .unwrap();
        let conflict = s.draft_conflicts(Some(&a)).unwrap().remove(0);
        s.resolve_draft_conflict(&a, &conflict.id, mail_protocol::DraftResolution::KeepLocal)
            .unwrap();
        let edited = s
            .save_draft(&a, &d.id, d.revision, &document("newer local"))
            .unwrap();
        s.complete_draft_upload(&u, 42, 7, Some("Trash")).unwrap();
        assert_eq!(s.draft(&a, &d.id).unwrap(), edited);
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "stopped");
        assert!(s.operations(Some(&a)).unwrap().is_empty());
        assert!(s.next_draft_upload().unwrap().is_none());
    }

    #[test]
    fn verifying_own_upload_does_not_resolve_or_clean_up_a_distinct_remote_conflict() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        add_trash(&mut s, &a);
        let original = s
            .archive_server_draft(&a, "Drafts", 42, 5, b"Subject: base\r\n\r\nbase")
            .unwrap();
        let d = s.convert_server_draft(&a, &original.id).unwrap();
        let d = s
            .save_draft(&a, &d.id, d.revision, &document("local edit"))
            .unwrap();
        let u = s
            .prepare_draft_upload(
                &d,
                "Drafts",
                Some(42),
                &"d".repeat(32),
                b"Subject: local\r\n\r\nlocal edit",
            )
            .unwrap();
        s.begin_draft_upload(&a, &u.id).unwrap();
        let remote = s
            .archive_server_draft(&a, "Drafts", 42, 9, b"Subject: remote\r\n\r\nremote edit")
            .unwrap();
        s.record_draft_conflict(&a, &d.id, Some(&remote.id), "both edited")
            .unwrap();
        s.complete_draft_upload(&u, 42, 7, Some("Trash")).unwrap();
        assert_eq!(
            s.draft_conflicts(Some(&a)).unwrap()[0].original.as_ref(),
            Some(&remote.id)
        );
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "conflict");
        assert_eq!(
            s.draft_sync_status(&a).unwrap()[0].source.as_ref(),
            Some(&original.id)
        );
        assert!(s.operations(Some(&a)).unwrap().is_empty());
        assert_eq!(s.draft(&a, &d.id).unwrap(), d);
    }
    fn setup(store: &mut Store) -> AccountId {
        let server = ServerSettings {
            authentication: Default::default(),
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
    fn deleting_local_draft_cancels_prepared_upload_without_waiting_for_network() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let mut store = Store::open(&data).unwrap();
        let account = setup(&mut store);
        let draft = store
            .create_draft(&account, &document("local only"))
            .unwrap();
        let upload = store
            .prepare_draft_upload(
                &draft,
                "Drafts",
                None,
                &"a".repeat(32),
                b"Subject: local\r\n\r\nlocal only",
            )
            .unwrap();
        assert!(
            store
                .delete_draft(&account, &draft.id, draft.revision + 1, None)
                .is_err()
        );
        assert!(!store.draft_upload(&account, &upload.id).unwrap().probe_only);
        store
            .delete_draft(&account, &draft.id, draft.revision, None)
            .unwrap();
        assert!(store.begin_draft_upload(&account, &upload.id).is_err());
        assert!(store.operations(None).unwrap().is_empty());
        drop(store);
        let mut store = Store::open(&data).unwrap();
        assert!(store.draft_ids(&account).unwrap().is_empty());
        assert!(store.next_draft_upload().unwrap().is_none());
    }

    #[test]
    fn sent_draft_recovers_unknown_append_after_restart_without_uploading_again() {
        for legacy in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            let mut store = Store::open(&data).unwrap();
            let account = setup(&mut store);
            let mut folders = store.remote_folders(&account).unwrap();
            folders.push(mail_core::RemoteFolder {
                name: "Trash".into(),
                display_name: "Trash".into(),
                delimiter: None,
                selectable: true,
            });
            store.replace_remote_folders(&account, &folders, 0).unwrap();
            let mut doc = document("sent draft");
            doc.headers.to = vec!["recipient@example.invalid".into()];
            let initial = store.create_draft(&account, &doc).unwrap();
            let upload = store
                .prepare_draft_upload(
                    &initial,
                    "Drafts",
                    None,
                    &"a".repeat(32),
                    b"Subject: sent draft\r\n\r\nsent draft",
                )
                .unwrap();
            assert!(!store.begin_draft_upload(&account, &upload.id).unwrap());
            let draft = store
                .save_draft(&account, &initial.id, initial.revision, &doc)
                .unwrap();
            let task = store
                .enqueue_draft(&account, &draft.id, draft.revision)
                .unwrap();
            let attempt = store.begin_smtp(&account, &task).unwrap();
            if legacy {
                // Older versions left both the sent draft and its in-flight APPEND.
                store
                    .connection
                    .execute("UPDATE send_tasks SET state='sent' WHERE id=?1", [&task.0])
                    .unwrap();
                drop(attempt);
            } else {
                store
                    .finish_smtp(attempt, mail_core::SmtpOutcome::Accepted, Some("Trash"))
                    .unwrap();
            }
            assert_eq!(store.draft(&account, &draft.id).unwrap(), draft);
            store
                .connection
                .execute("UPDATE draft_sync SET changed_at=0,attempted_at=0", [])
                .unwrap();
            drop(store);

            let mut store = Store::open(&data).unwrap();
            let candidate = store.next_draft_upload().unwrap().unwrap();
            let recovered = candidate.previous.unwrap();
            assert!(recovered.probe_only);
            assert_eq!(recovered.raw, upload.raw);
            assert!(store.begin_draft_upload(&account, &recovered.id).unwrap());
            store
                .fail_draft_sync(
                    &account,
                    &draft.id,
                    "uncertain",
                    "waiting for server verification",
                )
                .unwrap();
            assert_eq!(
                store.draft_sync_status(&account).unwrap()[0].state,
                "uncertain"
            );
            assert_eq!(store.draft_conflicts(Some(&account)).unwrap().len(), 1);
            let state: String = store
                .connection
                .query_row(
                    "SELECT state FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
                    params![account.0, draft.id.0],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(state, "stopped");
            store
                .complete_draft_upload(&recovered, 42, 7, Some("Trash"))
                .unwrap();
            assert!(store.draft_ids(&account).unwrap().is_empty());
            assert!(store.draft_conflicts(Some(&account)).unwrap().is_empty());
            assert!(store.next_draft_upload().unwrap().is_none());
            assert_eq!(store.operations(Some(&account)).unwrap().len(), 1);
            assert_eq!(
                store.task(&account, &task).unwrap().state,
                mail_core::SendState::Sent
            );
            assert!(store.begin_smtp(&account, &task).is_err());
            assert_eq!(
                store.task_snapshot(&account, &task).unwrap().plain,
                "sent draft"
            );
        }
    }

    #[test]
    fn recovery_keeps_new_edits_after_a_successful_send() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let account = setup(&mut store);
        let mut doc = document("sent");
        doc.headers.to = vec!["recipient@example.invalid".into()];
        let draft = store.create_draft(&account, &doc).unwrap();
        let upload = store
            .prepare_draft_upload(
                &draft,
                "Drafts",
                None,
                &"a".repeat(32),
                b"Subject: sent\r\n\r\nsent",
            )
            .unwrap();
        store.begin_draft_upload(&account, &upload.id).unwrap();
        let task = store
            .enqueue_draft(&account, &draft.id, draft.revision)
            .unwrap();
        let attempt = store.begin_smtp(&account, &task).unwrap();
        store
            .finish_smtp(attempt, mail_core::SmtpOutcome::Accepted, None)
            .unwrap();
        let edited = store
            .save_draft(
                &account,
                &draft.id,
                draft.revision,
                &document("new unsent edits"),
            )
            .unwrap();
        store.flush_draft_sync(&account).unwrap();
        let recovery = store
            .next_draft_upload()
            .unwrap()
            .unwrap()
            .previous
            .unwrap();
        assert!(recovery.probe_only);
        store.complete_draft_upload(&recovery, 42, 7, None).unwrap();
        assert_eq!(store.draft(&account, &draft.id).unwrap(), edited);
        assert_eq!(
            store.draft_sync_status(&account).unwrap()[0].state,
            "pending"
        );
        assert!(store.operations(None).unwrap().is_empty());
        assert_eq!(store.task_snapshot(&account, &task).unwrap().plain, "sent");
        assert!(store.begin_smtp(&account, &task).is_err());
    }

    #[test]
    fn recovery_prioritizes_unknown_upload_even_after_the_editor_is_cleared() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let account = setup(&mut store);
        let first = store.create_draft(&account, &document("prepared")).unwrap();
        store
            .prepare_draft_upload(
                &first,
                "Drafts",
                None,
                &"a".repeat(32),
                b"Subject: prepared\r\n\r\nprepared",
            )
            .unwrap();
        let second = store
            .save_draft(&account, &first.id, first.revision, &document("unknown"))
            .unwrap();
        let upload = store
            .prepare_draft_upload(
                &second,
                "Drafts",
                None,
                &"b".repeat(32),
                b"Subject: unknown\r\n\r\nunknown",
            )
            .unwrap();
        store.begin_draft_upload(&account, &upload.id).unwrap();
        let cleared = store
            .save_draft(&account, &second.id, second.revision, &document(""))
            .unwrap();
        store
            .connection
            .execute("UPDATE draft_sync SET changed_at=0,attempted_at=0", [])
            .unwrap();
        let candidate = store.next_draft_upload().unwrap().unwrap();
        assert_eq!(candidate.draft, cleared);
        let recovery = candidate.previous.unwrap();
        assert_eq!(recovery.id, upload.id);
        assert!(recovery.probe_only);
        store.complete_draft_upload(&recovery, 42, 7, None).unwrap();
        assert_eq!(store.draft(&account, &first.id).unwrap(), cleared);
    }

    #[test]
    fn verified_new_draft_version_queues_only_the_known_old_uid_for_trash() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let account = setup(&mut store);
        store
            .replace_remote_folders(
                &account,
                &[
                    mail_core::RemoteFolder {
                        name: "Drafts".into(),
                        display_name: "Drafts".into(),
                        delimiter: None,
                        selectable: true,
                    },
                    mail_core::RemoteFolder {
                        name: "Trash".into(),
                        display_name: "Trash".into(),
                        delimiter: None,
                        selectable: true,
                    },
                ],
                0,
            )
            .unwrap();
        let draft = store.create_draft(&account, &document("first")).unwrap();
        store.flush_draft_sync(&account).unwrap();
        let candidate = store.next_draft_upload().unwrap().unwrap();
        let raw = b"Message-ID: <first@example.invalid>\r\n\r\nfirst";
        let first = store
            .prepare_draft_upload(&candidate.draft, "Drafts", None, &"a".repeat(32), raw)
            .unwrap();
        store.begin_draft_upload(&account, &first.id).unwrap();
        store.complete_draft_upload(&first, 42, 7, None).unwrap();

        let updated = store
            .save_draft(&account, &draft.id, draft.revision, &document("second"))
            .unwrap();
        store.flush_draft_sync(&account).unwrap();
        let candidate = store.next_draft_upload().unwrap().unwrap();
        let raw = b"Message-ID: <second@example.invalid>\r\n\r\nsecond";
        let second = store
            .prepare_draft_upload(&candidate.draft, "Drafts", Some(42), &"b".repeat(32), raw)
            .unwrap();
        store.begin_draft_upload(&account, &second.id).unwrap();
        store
            .complete_draft_upload(&second, 42, 8, Some("Trash"))
            .unwrap();

        let operation: String = store
            .connection
            .query_row("SELECT operation FROM mail_operations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(operation.contains("Trash"));
        assert_eq!(
            store.draft_sync_status(&account).unwrap()[0].revision,
            updated.revision
        );
        assert_eq!(
            store.draft_sync_status(&account).unwrap()[0].detail,
            "已核验新版本；旧版本已排队移入垃圾箱"
        );
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
        s.complete_draft_upload(&recovered, 42, 7, None).unwrap();
        let status = s.draft_sync_status(&a).unwrap();
        assert_eq!(status[0].state, "pending");
        assert_eq!(status[0].base_revision, d.revision);
        assert_eq!(s.draft(&a, &d.id).unwrap(), updated);
        assert!(s.draft_upload(&a, &id).is_err());
        assert!(s.complete_draft_upload(&recovered, 42, 7, None).is_err());
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
        let retry = s.next_draft_upload().unwrap().unwrap().previous.unwrap();
        assert_eq!(retry.id, id);
        assert!(retry.probe_only);
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
        s.connection.execute_batch("DROP TRIGGER track_server_draft_update; CREATE TRIGGER track_server_draft_update AFTER UPDATE OF revision ON drafts BEGIN UPDATE draft_sync SET changed_at=unixepoch() WHERE account_id=NEW.account_id AND draft_id=NEW.id; END; ALTER TABLE draft_sync DROP COLUMN local_only; ALTER TABLE drafts DROP COLUMN deleted_at; ALTER TABLE accounts DROP COLUMN imap_auth; ALTER TABLE accounts DROP COLUMN smtp_auth; ALTER TABLE send_tasks DROP COLUMN save_sent_copy; DROP TABLE autostart_intent; DROP TABLE notification_candidates; DROP TABLE notification_pending; DROP TABLE notification_baselines; DROP TABLE desktop_settings; DROP VIEW protected_objects; DROP TRIGGER cache_body_insert; DROP TRIGGER cache_part_insert; DROP TABLE cache_access; DROP TABLE cache_settings; DROP TRIGGER stop_sent_draft_sync; DROP TRIGGER track_draft_scan_account; DROP TABLE draft_conflicts; DROP TABLE draft_scan_failures; DROP TABLE draft_scans; DROP TRIGGER track_server_draft_insert; DROP TRIGGER track_server_draft_update; DROP TABLE draft_uploads; DROP TABLE draft_sync; PRAGMA user_version=14;").unwrap();
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
