//! 草稿发现、明确冲突选择。任何选择均保留已有用户内容；不授予SMTP许可。
use crate::*;
use mail_core::DraftId;
use mail_protocol::{DraftConflict, DraftResolution};

impl Store {
    /// 原件关联和发现决策同一保存点，崩溃不能留下“已扫描但未关联”的半状态。
    pub fn import_discovered_draft(
        &mut self,
        version: &AccountVersion,
        folder: &str,
        epoch: u32,
        uid: u32,
        raw: &[u8],
        ambiguous: bool,
    ) -> Result<()> {
        self.validate_account_commit(Some(version))?;
        self.connection
            .execute_batch("SAVEPOINT import_discovered_draft")?;
        let result = (|| {
            let source = self.archive_server_draft(&version.account, folder, epoch, uid, raw)?;
            self.reconcile_discovered_draft(&source, ambiguous)
        })();
        if result.is_ok() {
            self.connection
                .execute_batch("RELEASE import_discovered_draft")?;
        } else {
            self.connection.execute_batch(
                "ROLLBACK TO import_discovered_draft; RELEASE import_discovered_draft",
            )?;
        }
        result
    }
    pub fn next_draft_scan(&mut self) -> Result<Option<AccountVersion>> {
        let account: Option<String> = self.connection.query_row(
            "SELECT s.account_id FROM draft_scans s JOIN account_options a ON a.account_id=s.account_id WHERE a.paused=0 AND s.attempted_at<=unixepoch()-60 ORDER BY s.attempted_at,s.account_id LIMIT 1", [], |r|r.get(0),
        ).optional()?;
        let Some(account) = account else {
            return Ok(None);
        };
        self.connection.execute(
            "UPDATE draft_scans SET attempted_at=unixepoch(),detail='' WHERE account_id=?1",
            [&account],
        )?;
        self.account_version(&AccountId(account)).map(Some)
    }
    pub fn draft_scan_missing(
        &self,
        account: &AccountId,
        folder: &str,
        epoch: u32,
        uids: &[u32],
    ) -> Result<Vec<u32>> {
        if self.server_draft_folder(account)?.as_deref() != Some(folder) || uids.len() > 10000 {
            return Err(Error::Conflict);
        }
        let mut q=self.connection.prepare("SELECT uid FROM server_draft_sources WHERE account_id=?1 AND remote_name=?2 AND uidvalidity=?3")?;
        let known = q
            .query_map(params![account.0, folder, epoch], |r| r.get::<_, u32>(0))?
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
        let mut q=self.connection.prepare("SELECT uid FROM draft_scan_failures WHERE account_id=?1 AND remote_name=?2 AND uidvalidity=?3 AND attempted_at>unixepoch()-900")?;
        let failed = q
            .query_map(params![account.0, folder, epoch], |r| r.get::<_, u32>(0))?
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
        Ok(uids
            .iter()
            .rev()
            .filter(|u| !known.contains(u) && !failed.contains(u))
            .take(4)
            .copied()
            .collect())
    }
    pub fn draft_scan_failure(
        &mut self,
        version: &AccountVersion,
        folder: &str,
        epoch: u32,
        uid: u32,
        detail: &str,
    ) -> Result<()> {
        self.validate_account_commit(Some(version))?;
        if self.server_draft_folder(&version.account)?.as_deref() != Some(folder)
            || detail.len() > 2048
        {
            return Err(Error::Conflict);
        }
        self.connection.execute("INSERT INTO draft_scan_failures(account_id,remote_name,uidvalidity,uid,detail) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(account_id,remote_name,uidvalidity,uid) DO UPDATE SET attempted_at=unixepoch(),detail=excluded.detail",params![version.account.0,folder,epoch,uid,detail])?;
        Ok(())
    }
    pub fn draft_scan_notice(&self, account: &AccountId) -> Result<String> {
        let mut q=self.connection.prepare("SELECT uid,detail FROM draft_scan_failures WHERE account_id=?1 ORDER BY attempted_at DESC LIMIT 8")?;
        let failures = q
            .query_map([&account.0], |r| {
                Ok(format!(
                    "UID {}：{}（原件仍在服务器，15分钟后再试）",
                    r.get::<_, u32>(0)?,
                    r.get::<_, String>(1)?
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let scan: Option<String> = self
            .connection
            .query_row(
                "SELECT detail FROM draft_scans WHERE account_id=?1",
                [&account.0],
                |r| r.get(0),
            )
            .optional()?;
        Ok(std::iter::once(scan.unwrap_or_default())
            .chain(failures)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("；"))
    }
    pub fn finish_draft_scan(&mut self, version: &AccountVersion, detail: &str) -> Result<()> {
        self.validate_account_commit(Some(version))?;
        self.connection.execute(
            "UPDATE draft_scans SET detail=?1 WHERE account_id=?2",
            params![detail, version.account.0],
        )?;
        Ok(())
    }
    pub fn record_draft_conflict(
        &mut self,
        account: &AccountId,
        draft: &DraftId,
        remote: Option<&str>,
        reason: &str,
    ) -> Result<()> {
        if reason.len() > 2048 {
            return Err(Error::Invalid("冲突提示过长"));
        }
        let withdrawn: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM drafts d JOIN draft_sync s ON s.account_id=d.account_id AND s.draft_id=d.id WHERE d.account_id=?1 AND d.id=?2 AND (d.deleted_at IS NOT NULL OR s.local_only=1))",params![account.0,draft.0],|r|r.get(0))?;
        if withdrawn {
            return Ok(());
        }
        if let Some(source) = remote {
            self.server_draft_source(account, source)?;
        }
        let base: Option<String> = self.connection.query_row(
            "SELECT base_source FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
            params![account.0, draft.0],
            |r| r.get(0),
        )?;
        let old: Option<(String,Option<String>)>=self.connection.query_row("SELECT id,remote_source FROM draft_conflicts WHERE account_id=?1 AND draft_id=?2 AND state='open'",params![account.0,draft.0],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let tx = self.connection.savepoint()?;
        if let Some((id, old_remote)) = &old {
            if old_remote.as_deref() == remote || remote.is_none() {
                tx.execute(
                    "UPDATE draft_conflicts SET reason=?1 WHERE id=?2",
                    params![reason, id],
                )?;
            } else {
                tx.execute(
                    "UPDATE draft_conflicts SET state='superseded' WHERE id=?1",
                    [id],
                )?;
            }
        }
        if old
            .as_ref()
            .is_none_or(|(_, old)| remote.is_some() && old.as_deref() != remote)
        {
            tx.execute("INSERT INTO draft_conflicts(id,account_id,draft_id,base_source,remote_source,reason) VALUES (?1,?2,?3,?4,?5,?6)",params![new_id()?,account.0,draft.0,base,remote,reason])?;
        }
        // Recording an unresolved old upload must not reactivate a sent draft,
        // including if the process exits before its caller finishes handling it.
        tx.execute("UPDATE draft_sync SET state=CASE WHEN state='stopped' THEN state ELSE 'conflict' END,detail=?1,attempted_at=unixepoch() WHERE account_id=?2 AND draft_id=?3",params![reason,account.0,draft.0])?;
        tx.commit()?;
        Ok(())
    }
    pub fn draft_conflicts(&self, account: Option<&AccountId>) -> Result<Vec<DraftConflict>> {
        let mut q=self.connection.prepare("SELECT id,account_id,draft_id,remote_copy,remote_source,reason FROM draft_conflicts WHERE state='open' AND (?1 IS NULL OR account_id=?1) ORDER BY created_at DESC,rowid DESC LIMIT 200")?;
        Ok(q.query_map([account.map(|a| &a.0)], |r| {
            Ok(DraftConflict {
                id: r.get(0)?,
                account: AccountId(r.get(1)?),
                local: DraftId(r.get(2)?),
                remote_copy: r.get::<_, Option<String>>(3)?.map(DraftId),
                original: r.get(4)?,
                reason: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?)
    }
    /// 所有决策保留原件和本地；“远端”切换到独立编辑副本，不覆盖迟到本地修改。
    pub fn resolve_draft_conflict(
        &mut self,
        account: &AccountId,
        id: &str,
        resolution: DraftResolution,
    ) -> Result<()> {
        let (draft,remote,state):(String,Option<String>,String)=self.connection.query_row("SELECT draft_id,remote_source,state FROM draft_conflicts WHERE id=?1 AND account_id=?2",params![id,account.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        if state != "open" {
            return Ok(());
        }
        let unknown: bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM draft_uploads WHERE account_id=?1 AND draft_id=?2 AND state IN ('in_flight','uncertain'))",params![account.0,draft],|r|r.get(0))?;
        if unknown && matches!(resolution, DraftResolution::KeepRemote) {
            return Err(Error::Invalid("上传结果尚未确认；请继续核验，或仅保留本地"));
        }
        let local = self.draft(account, &DraftId(draft.clone()))?;
        let source = remote
            .as_ref()
            .map(|v| self.server_draft_source(account, v))
            .transpose()?;
        if matches!(resolution, DraftResolution::KeepRemote) && source.is_none() {
            return Err(Error::Invalid("远端无可选原件；请选择保留本地或保留两份"));
        }
        self.connection
            .execute_batch("SAVEPOINT resolve_draft_conflict")?;
        let result = (|| {
            let mut copy = None;
            if let Some(source) = &source
                && !matches!(resolution, DraftResolution::KeepLocal)
            {
                match self.convert_server_draft_fresh(account, &source.id) {
                    Ok(d) => {
                        copy = Some(d.id);
                    }
                    Err(e) if matches!(resolution, DraftResolution::KeepRemote) => return Err(e),
                    Err(_) => (), // 两份/本地允许远端不可编辑；原始字节仍保留。
                }
            }
            if !(unknown && matches!(resolution, DraftResolution::KeepLocal)) {
                self.connection.execute("UPDATE draft_uploads SET state='conflict',detail='操作者明确选择版本，旧准备快照保留' WHERE account_id=?1 AND draft_id=?2 AND state IN ('prepared','in_flight','uncertain')",params![account.0,draft])?;
            }
            // 不重用同revision的旧上传ID；增加本地版本，旧快照永久保留。
            if unknown && matches!(resolution, DraftResolution::KeepLocal) {
                self.connection.execute("UPDATE draft_sync SET local_only=1,state='stopped',detail='已保留本地，暂停云端同步；旧上传证据保留' WHERE account_id=?1 AND draft_id=?2",params![account.0,draft])?;
            } else if matches!(resolution, DraftResolution::KeepRemote) {
                self.connection.execute("UPDATE draft_sync SET local_only=1,state='stopped',detail='保留本地副本；已选择远端编辑副本' WHERE account_id=?1 AND draft_id=?2",params![account.0,draft])?;
                self.select_editor_draft(account, copy.as_ref().ok_or(Error::Conflict)?)?;
            } else {
                let saved = self.save_draft(account, &local.id, local.revision, &local.document)?;
                if matches!(resolution, DraftResolution::KeepLocal)
                    && let Some(source) = &source
                    && source.converted.is_none()
                {
                    // The explicitly replaced version belongs to this draft.
                    // Retain its original locally, but allow cleanup only after
                    // the replacement APPEND has been verified.
                    self.connection.execute("UPDATE server_draft_sources SET converted=?1 WHERE id=?2 AND account_id=?3 AND converted IS NULL",params![local.id.0,source.id,account.0])?;
                }
                self.connection.execute("UPDATE draft_sync SET local_only=0,base_source=?1,base_revision=0,state='pending',lineage=?4,detail='已明确保留本地，将上传新版本；远端原件保留',attempted_at=0,changed_at=unixepoch()-5 WHERE account_id=?2 AND draft_id=?3",params![source.as_ref().map(|s|&s.id),account.0,saved.id.0,new_id()?])?;
                self.select_editor_draft(account, &saved.id)?;
            }
            self.connection.execute(
                "UPDATE draft_conflicts SET state=?1,remote_copy=?2 WHERE id=?3 AND account_id=?4",
                params![
                    match resolution {
                        DraftResolution::KeepBoth => "keep_both",
                        DraftResolution::KeepLocal => "keep_local",
                        DraftResolution::KeepRemote => "keep_remote",
                    },
                    copy.map(|d| d.0),
                    id,
                    account.0
                ],
            )?;
            Ok(())
        })();
        if result.is_ok() {
            self.connection
                .execute_batch("RELEASE resolve_draft_conflict")?;
        } else {
            self.connection.execute_batch(
                "ROLLBACK TO resolve_draft_conflict; RELEASE resolve_draft_conflict",
            )?;
        }
        result
    }
    /// 新UID仅在关联值与一个共同基线唯一匹配时纳入；歧义原件独立保留。
    pub fn reconcile_discovered_draft(
        &mut self,
        source: &mail_protocol::ServerDraftSource,
        ambiguous: bool,
    ) -> Result<()> {
        self.connection.execute("DELETE FROM draft_scan_failures WHERE account_id=?1 AND remote_name=?2 AND uidvalidity=?3 AND uid=?4",params![source.account.0,source.folder,source.uidvalidity,source.uid])?;
        if source.converted.is_some() {
            return Ok(());
        }
        let raw = self.server_draft_original(&source.account, &source.id)?;
        let identity = mail_content::server_draft_identity(&raw);
        if let Some((_, value)) = &identity {
            // A lineage associates versions; only the frozen MIME identifies our
            // unchanged upload. A different body with the same lineage is a real
            // remote edit, not a duplicate to suppress.
            let uploads: Vec<(String, String)> = self.connection.prepare("SELECT draft_id,raw_id FROM draft_uploads WHERE account_id=?1 AND remote_name=?2 AND state IN ('in_flight','uncertain','conflict')")?.query_map(params![source.account.0,source.folder],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<std::result::Result<_,_>>()?;
            let mut owners = std::collections::BTreeSet::new();
            for (draft, object) in uploads {
                let frozen = objects::read(
                    &self.objects,
                    &self.connection,
                    &source.account,
                    &object,
                    "attachment",
                    mail_content::MAX_SERVER_DRAFT_BYTES as u64,
                )?;
                if mail_content::server_draft_identity(&frozen)
                    .as_ref()
                    .map(|(_, v)| v)
                    != Some(value)
                {
                    continue;
                }
                if mail_content::same_server_draft_content(&frozen, &raw) {
                    return Ok(());
                }
                owners.insert(draft);
            }
            if !owners.is_empty() {
                for draft in owners {
                    let branch: Option<String> = self.connection.query_row(
                        "SELECT lineage FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
                        params![source.account.0, draft],
                        |r| r.get(0),
                    )?;
                    if branch.as_ref().is_none_or(|b| b == value) {
                        self.record_draft_conflict(
                            &source.account,
                            &DraftId(draft),
                            Some(&source.id),
                            "上传核验期间云端内容发生变化；两份内容均已保留",
                        )?;
                    }
                }
                return Ok(());
            }
            let withdrawn: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM draft_sync s JOIN drafts d ON d.account_id=s.account_id AND d.id=s.draft_id WHERE s.account_id=?1 AND COALESCE(s.lineage,s.draft_id)=?2 AND (d.deleted_at IS NOT NULL OR s.local_only=1))",params![source.account.0,value],|r|r.get(0))?;
            if withdrawn {
                return Ok(());
            }
        }
        let mut q=self.connection.prepare("SELECT d.id,s.base_source,s.state,s.lineage FROM draft_sync s JOIN drafts d ON d.id=s.draft_id AND d.account_id=s.account_id WHERE s.account_id=?1 AND s.base_source IS NOT NULL AND s.state!='stopped'")?;
        let links = q
            .query_map([&source.account.0], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(q);
        let mut matches = vec![];
        if let Some((_, value)) = &identity {
            for (draft, base, state, branch) in links {
                let old = self.server_draft_source(&source.account, &base)?;
                if old.folder != source.folder {
                    continue;
                }
                let old_raw = self.server_draft_original(&source.account, &base)?;
                let association = branch
                    .or_else(|| mail_content::server_draft_identity(&old_raw).map(|(_, v)| v));
                if association.as_ref() == Some(value) {
                    matches.push((draft, old, state));
                }
            }
        }
        if matches.len() == 1 {
            let (id, old, state) = matches.remove(0);
            if old.id == source.id {
                return Ok(());
            }
            if state == "uncertain" {
                self.connection.execute("UPDATE draft_conflicts SET remote_source=?1 WHERE account_id=?2 AND draft_id=?3 AND state='open'",params![source.id,source.account.0,id])?;
                return Ok(());
            } // 原上传仍由原幂等标识查证，不让扫描改写执行状态。

            if ambiguous
                || old.uidvalidity != source.uidvalidity
                || !matches!(state.as_str(), "synced" | "pending")
            {
                self.record_draft_conflict(
                    &source.account,
                    &DraftId(id),
                    Some(&source.id),
                    "远端出现关联新版本；基线变化或已有冲突，保留两份，请选择",
                )?;
                return Ok(());
            }
            let refresh = DraftRefresh {
                version: self.account_version(&source.account)?,
                draft: self.draft(&source.account, &DraftId(id))?,
                source: old,
            };
            self.observe_draft_remote_at(&refresh, Some(&raw), Some(source))?;
        } else if matches.len() > 1 {
            for (id, _, _) in matches {
                self.record_draft_conflict(
                    &source.account,
                    &DraftId(id),
                    Some(&source.id),
                    "关联标识存在多个本地归属；不猜测合并，请选择",
                )?;
            }
        } else if mail_content::lossless_server_copy(&raw).is_some() {
            // 受控模型可自动导入；外部HTML/不受控内容留原件，必须明确转换。
            let _ = self.convert_server_draft(&source.account, &source.id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft_refresh::tests::{linked, raw, setup};
    #[test]
    fn choosing_local_does_not_create_an_editor_copy_and_cleans_only_after_verified_replacement() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let base = linked(&mut s, &a);
        let mut document = base.draft.document.clone();
        document.body = mail_content::draft::Draft::plain("local edit").unwrap();
        let d = s
            .save_draft(&a, &base.draft.id, base.draft.revision, &document)
            .unwrap();
        let remote = s
            .archive_server_draft(&a, "Drafts", 42, 9, &raw("remote edit"))
            .unwrap();
        s.record_draft_conflict(&a, &d.id, Some(&remote.id), "both edited")
            .unwrap();
        let conflict = s.draft_conflicts(Some(&a)).unwrap().remove(0);
        s.resolve_draft_conflict(&a, &conflict.id, DraftResolution::KeepLocal)
            .unwrap();
        assert_eq!(s.draft_ids(&a).unwrap(), vec![d.id.clone()]);
        assert_eq!(s.draft(&a, &d.id).unwrap().document, document);
        assert!(s.operations(Some(&a)).unwrap().is_empty());
        let mut folders = s.remote_folders(&a).unwrap();
        folders.push(mail_core::RemoteFolder {
            name: "Trash".into(),
            display_name: "Trash".into(),
            delimiter: None,
            selectable: true,
        });
        s.replace_remote_folders(&a, &folders, 0).unwrap();
        let d = s.draft(&a, &d.id).unwrap();
        let upload = s
            .prepare_draft_upload(&d, "Drafts", Some(42), &"d".repeat(32), &raw("local edit"))
            .unwrap();
        s.begin_draft_upload(&a, &upload.id).unwrap();
        s.complete_draft_upload(&upload, 42, 11, Some("Trash"))
            .unwrap();
        let mut uids: Vec<_> = s
            .operations(Some(&a))
            .unwrap()
            .iter()
            .map(|o| o.location.uid)
            .collect();
        uids.sort();
        assert_eq!(uids, vec![7, 9]);
        assert_eq!(
            s.server_draft_original(&a, &remote.id).unwrap(),
            raw("remote edit")
        );
        assert_eq!(s.draft_ids(&a).unwrap().len(), 1);
    }

    #[test]
    fn scanning_own_unknown_upload_does_not_import_a_duplicate_or_revive_a_deletion() {
        for persisted_lineage in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("data");
            let mut s = Store::open(&path).unwrap();
            let a = setup(&mut s);
            let b = setup(&mut s);
            let original = linked(&mut s, &a);
            let d = s
                .save_draft(
                    &a,
                    &original.draft.id,
                    original.draft.revision,
                    &original.draft.document,
                )
                .unwrap();
            let bytes = [
                b"X-QingMail-Draft: <fixture@example.invalid>\r\n".as_slice(),
                &raw("own upload"),
            ]
            .concat();
            let u = s
                .prepare_draft_upload(&d, "Drafts", Some(42), &"a".repeat(32), &bytes)
                .unwrap();
            s.begin_draft_upload(&a, &u.id).unwrap();
            if persisted_lineage {
                s.connection.execute("UPDATE draft_sync SET lineage='<fixture@example.invalid>' WHERE account_id=?1", [&a.0]).unwrap();
            }
            let traced = [b"X-QQ-mid: fixture\r\n".as_slice(), &bytes].concat();
            let source = s
                .archive_server_draft(&a, "Drafts", 42, 9, &traced)
                .unwrap();
            s.reconcile_discovered_draft(&source, false).unwrap();
            assert_eq!(s.draft_ids(&a).unwrap(), vec![d.id.clone()]);
            assert!(s.draft_conflicts(Some(&a)).unwrap().is_empty());
            let changed = [
                b"X-QingMail-Draft: <fixture@example.invalid>\r\n".as_slice(),
                &raw("external edit during upload"),
            ]
            .concat();
            let changed = s
                .archive_server_draft(&a, "Drafts", 42, 10, &changed)
                .unwrap();
            s.reconcile_discovered_draft(&changed, false).unwrap();
            assert_eq!(
                s.draft_conflicts(Some(&a)).unwrap()[0].original.as_ref(),
                Some(&changed.id)
            );
            assert_eq!(s.draft_ids(&a).unwrap(), vec![d.id.clone()]);
            // An identical identity in a different account has no ownership here.
            let other = s.archive_server_draft(&b, "Drafts", 42, 9, &bytes).unwrap();
            s.reconcile_discovered_draft(&other, false).unwrap();
            assert_eq!(s.draft_ids(&b).unwrap().len(), 1);
            s.delete_draft(&a, &d.id, d.revision, None).unwrap();
            drop(s);
            let mut s = Store::open(&path).unwrap();
            let source = s
                .archive_server_draft(&a, "Drafts", 42, 11, &bytes)
                .unwrap();
            s.reconcile_discovered_draft(&source, false).unwrap();
            assert!(s.draft_ids(&a).unwrap().is_empty());
            assert_eq!(s.server_draft_original(&a, &source.id).unwrap(), bytes);
            assert!(s.operations(Some(&a)).unwrap().is_empty());
        }
    }

    #[test]
    fn alternating_remote_uid_updates_and_concurrent_edits_keep_both_across_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut s = Store::open(&path).unwrap();
        let a = setup(&mut s);
        let old = linked(&mut s, &a);
        let next = s
            .archive_server_draft(&a, "Drafts", 42, 9, &raw("other client"))
            .unwrap();
        s.reconcile_discovered_draft(&next, false).unwrap();
        let clean = s.draft(&a, &old.draft.id).unwrap();
        assert_eq!(clean.document.body.text(), "other client");
        assert_eq!(
            s.draft_sync_status(&a).unwrap()[0].source.as_ref(),
            Some(&next.id)
        );
        let mut edited = clean.document.clone();
        edited.body = mail_content::draft::Draft::plain("offline local").unwrap();
        let local = s
            .save_draft(&a, &clean.id, clean.revision, &edited)
            .unwrap();
        let remote = s
            .archive_server_draft(&a, "Drafts", 42, 11, &raw("concurrent remote"))
            .unwrap();
        s.reconcile_discovered_draft(&remote, false).unwrap();
        let conflicts = s.draft_conflicts(Some(&a)).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].original.as_ref(), Some(&remote.id));
        drop(s);
        let mut s = Store::open(&path).unwrap();
        s.resolve_draft_conflict(&a, &conflicts[0].id, DraftResolution::KeepBoth)
            .unwrap();
        assert_eq!(s.draft(&a, &local.id).unwrap().document, local.document);
        assert!(s.draft_conflicts(Some(&a)).unwrap().is_empty());
        assert_eq!(s.draft_ids(&a).unwrap().len(), 2);
        let candidate = s.next_draft_upload().unwrap().unwrap();
        assert_eq!(candidate.draft.id, local.id);
        assert_ne!(candidate.lineage, "<fixture@example.invalid>");
        let revision = candidate.draft.revision;
        s.resolve_draft_conflict(&a, &conflicts[0].id, DraftResolution::KeepBoth)
            .unwrap();
        assert_eq!(s.draft(&a, &local.id).unwrap().revision, revision);
        assert_eq!(
            s.server_draft_original(&a, &old.source.id).unwrap(),
            raw("first")
        );
        assert_eq!(
            s.server_draft_original(&a, &remote.id).unwrap(),
            raw("concurrent remote")
        );
        let next_remote = s
            .archive_server_draft(&a, "Drafts", 42, 13, &raw("remote branch edited again"))
            .unwrap();
        s.reconcile_discovered_draft(&next_remote, false).unwrap();
        assert!(s.draft_conflicts(Some(&a)).unwrap().is_empty());
        assert_eq!(s.draft(&a, &local.id).unwrap().document, local.document);
    }
    #[test]
    fn remote_selection_preserves_latest_local_edits_and_ambiguous_epoch_is_not_merged() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let b = setup(&mut s);
        let old = linked(&mut s, &a);
        let remote = s
            .archive_server_draft(&a, "Drafts", 42, 9, &raw("remote"))
            .unwrap();
        s.reconcile_discovered_draft(&remote, true).unwrap();
        let conflict = s.draft_conflicts(Some(&a)).unwrap().remove(0);
        assert!(
            s.resolve_draft_conflict(&b, &conflict.id, DraftResolution::KeepRemote)
                .is_err()
        );
        let mut doc = old.draft.document.clone();
        doc.body = mail_content::draft::Draft::plain("latest local").unwrap();
        let local = s
            .save_draft(&a, &old.draft.id, old.draft.revision, &doc)
            .unwrap();
        s.resolve_draft_conflict(&a, &conflict.id, DraftResolution::KeepRemote)
            .unwrap();
        assert_eq!(s.draft(&a, &local.id).unwrap(), local);
        let selected = s.selected_local_draft(&a).unwrap().unwrap();
        assert_ne!(selected, local.id);
        assert_eq!(
            s.draft(&a, &selected).unwrap().document.body.text(),
            "remote"
        );
        let status = s.draft_sync_status(&a).unwrap();
        assert_eq!(
            status.iter().find(|x| x.draft == local.id).unwrap().state,
            "stopped"
        );
        let next = s
            .archive_server_draft(&a, "Drafts", 99, 1, &raw("new epoch"))
            .unwrap();
        s.reconcile_discovered_draft(&next, false).unwrap();
        assert_eq!(
            s.draft(&a, &selected).unwrap().document.body.text(),
            "remote"
        );
        assert_eq!(s.draft_conflicts(Some(&a)).unwrap().len(), 1);
    }
    #[test]
    fn external_plain_client_can_replace_uid_without_metadata_but_html_requires_choice() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let old = linked(&mut s, &a);
        let mut folders = s.remote_folders(&a).unwrap();
        folders.push(mail_core::RemoteFolder {
            name: "Trash".into(),
            display_name: "Trash".into(),
            delimiter: None,
            selectable: true,
        });
        s.replace_remote_folders(&a, &folders, 0).unwrap();
        let raw=b"Message-ID: <fixture@example.invalid>\r\nSubject: external update\r\nBcc: hidden@example.invalid\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nexternal plain";
        let source = s.archive_server_draft(&a, "Drafts", 42, 9, raw).unwrap();
        s.reconcile_discovered_draft(&source, false).unwrap();
        assert_eq!(
            s.draft(&a, &old.draft.id).unwrap().document.body.text(),
            "external plain"
        );
        let raw=b"Message-ID: <fixture@example.invalid>\r\nSubject: html update\r\nContent-Type: text/html\r\n\r\n<b>external HTML</b>";
        let source = s.archive_server_draft(&a, "Drafts", 42, 11, raw).unwrap();
        s.reconcile_discovered_draft(&source, false).unwrap();
        let conflict = s.draft_conflicts(Some(&a)).unwrap().remove(0);
        s.resolve_draft_conflict(&a, &conflict.id, DraftResolution::KeepRemote)
            .unwrap();
        assert!(
            s.draft(&a, &s.selected_local_draft(&a).unwrap().unwrap())
                .unwrap()
                .document
                .body
                .text()
                .contains("external HTML")
        );
    }
    #[test]
    fn missing_remote_and_unknown_append_require_explicit_new_branch_not_replay() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let old = linked(&mut s, &a);
        s.observe_draft_remote(&old, None).unwrap();
        let conflict = s.draft_conflicts(Some(&a)).unwrap().remove(0);
        assert!(
            s.resolve_draft_conflict(&a, &conflict.id, DraftResolution::KeepRemote)
                .is_err()
        );
        s.resolve_draft_conflict(&a, &conflict.id, DraftResolution::KeepLocal)
            .unwrap();
        let local = s.draft(&a, &old.draft.id).unwrap();
        let id = "d".repeat(32);
        s.prepare_draft_upload(&local, "Drafts", None, &id, &raw("fixed unknown"))
            .unwrap();
        s.begin_draft_upload(&a, &id).unwrap();
        s.fail_draft_sync(&a, &local.id, "uncertain", "APPEND response lost")
            .unwrap();
        let conflict = s.draft_conflicts(Some(&a)).unwrap().remove(0);
        assert!(
            s.resolve_draft_conflict(&a, &conflict.id, DraftResolution::KeepRemote)
                .is_err()
        );
        s.resolve_draft_conflict(&a, &conflict.id, DraftResolution::KeepBoth)
            .unwrap();
        assert!(s.draft_upload(&a, &id).is_err());
        let next = s.next_draft_upload().unwrap().unwrap();
        assert!(next.previous.is_none());
        assert!(next.draft.revision > local.revision);
        s.fail_draft_sync_branch(&a, &local.id, "old-branch", "uncertain", "late result")
            .unwrap();
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "pending");
        assert!(s.task_ids(&a).unwrap().is_empty());
    }
    #[test]
    fn discovery_is_bounded_account_scoped_and_external_conversion_is_explicit() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let b = setup(&mut s);
        s.connection
            .execute(
                "UPDATE account_options SET paused=1 WHERE account_id=?1",
                [&a.0],
            )
            .unwrap();
        assert_eq!(s.next_draft_scan().unwrap().unwrap().account, b);
        assert!(s.next_draft_scan().unwrap().is_none());
        let uids = (1..=250).rev().collect::<Vec<_>>();
        assert_eq!(
            s.draft_scan_missing(&a, "Drafts", 42, &uids).unwrap(),
            vec![1, 2, 3, 4]
        );
        let external = s
            .archive_server_draft(
                &a,
                "Drafts",
                42,
                1,
                b"Subject: external\r\nContent-Type: text/html\r\n\r\n<b>keep format</b>",
            )
            .unwrap();
        s.reconcile_discovered_draft(&external, false).unwrap();
        assert!(s.draft_ids(&a).unwrap().is_empty());
        let controlled = s
            .archive_server_draft(&a, "Drafts", 42, 2, &raw("new"))
            .unwrap();
        s.reconcile_discovered_draft(&controlled, false).unwrap();
        s.reconcile_discovered_draft(&s.server_draft_source(&a, &controlled.id).unwrap(), false)
            .unwrap();
        assert_eq!(s.draft_ids(&a).unwrap().len(), 1);
        let other = s
            .archive_server_draft(&b, "Drafts", 42, 2, &raw("other account"))
            .unwrap();
        s.reconcile_discovered_draft(&other, false).unwrap();
        assert_eq!(s.draft_ids(&b).unwrap().len(), 1);
        assert_eq!(
            s.draft_scan_missing(&a, "Drafts", 42, &uids).unwrap(),
            vec![3, 4, 5, 6]
        );
    }
    #[test]
    fn schema15_migration_preserves_conflicts_originals_and_credentials() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut s = Store::open(&path).unwrap();
        let a = setup(&mut s);
        let old = linked(&mut s, &a);
        s.fail_draft_sync(&a, &old.draft.id, "conflict", "old conflict")
            .unwrap();
        let version = s.account_version(&a).unwrap();
        s.connection.execute_batch("DROP TRIGGER track_server_draft_update; CREATE TRIGGER track_server_draft_update AFTER UPDATE OF revision ON drafts BEGIN UPDATE draft_sync SET changed_at=unixepoch() WHERE account_id=NEW.account_id AND draft_id=NEW.id; END; ALTER TABLE draft_sync DROP COLUMN local_only; ALTER TABLE drafts DROP COLUMN deleted_at; ALTER TABLE accounts DROP COLUMN imap_auth; ALTER TABLE accounts DROP COLUMN smtp_auth; ALTER TABLE send_tasks DROP COLUMN save_sent_copy; DROP TABLE autostart_intent; DROP TABLE notification_candidates; DROP TABLE notification_pending; DROP TABLE notification_baselines; DROP TABLE desktop_settings; DROP VIEW protected_objects; DROP TRIGGER cache_body_insert; DROP TRIGGER cache_part_insert; DROP TABLE cache_access; DROP TABLE cache_settings; DROP TRIGGER stop_sent_draft_sync; DROP TRIGGER track_draft_scan_account; DROP TABLE draft_conflicts; DROP TABLE draft_scan_failures; DROP TABLE draft_scans; ALTER TABLE draft_sync DROP COLUMN lineage; PRAGMA user_version=15;").unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.draft(&a, &old.draft.id).unwrap(), old.draft);
        assert_eq!(s.account_version(&a).unwrap(), version);
        assert_eq!(s.draft_conflicts(Some(&a)).unwrap().len(), 1);
        assert_eq!(
            s.server_draft_original(&a, &old.source.id).unwrap(),
            raw("first")
        );
        assert!(std::fs::read_dir(&path).unwrap().any(|p| {
            p.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("migration-v15-")
        }));
    }
    #[test]
    fn large_original_conversion_and_frozen_upload_use_scoped_limit() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let mut raw=b"Subject: large\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nsmall body\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=large.bin\r\n\r\n".to_vec();
        raw.extend(vec![b'x'; 9 * 1024 * 1024]);
        raw.extend_from_slice(b"\r\n--x--\r\n");
        assert!(mail_content::parse(&raw).is_err());
        let source = s.archive_server_draft(&a, "Drafts", 42, 1, &raw).unwrap();
        let draft = s.convert_server_draft(&a, &source.id).unwrap();
        assert_eq!(draft.document.body.text(), "small body");
        assert_eq!(draft.document.attachments.len(), 1);
        let id = "f".repeat(32);
        let upload = s
            .prepare_draft_upload(&draft, "Drafts", Some(42), &id, &raw)
            .unwrap();
        s.begin_draft_upload(&a, &id).unwrap();
        s.complete_draft_upload(&upload, 42, 2, None).unwrap();
        assert_eq!(s.server_draft_sources(&a).unwrap().len(), 2);
        s.remove_account(&a).unwrap();
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::draft_refresh::tests::{linked, setup};
    #[test]
    fn smtp_acceptance_removes_sent_revision_and_stops_draft_sync() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let old = linked(&mut s, &a);
        s.replace_remote_folders(
            &a,
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
        let mut doc = old.draft.document.clone();
        doc.headers.to = vec!["to@example.invalid".into()];
        let draft = s
            .save_draft(&a, &old.draft.id, old.draft.revision, &doc)
            .unwrap();
        s.record_draft_conflict(&a, &draft.id, None, "existing conflict")
            .unwrap();
        let task = s.enqueue_draft(&a, &draft.id, draft.revision).unwrap();
        let attempt = s.begin_smtp(&a, &task).unwrap();
        s.finish_smtp(attempt, mail_core::SmtpOutcome::Accepted, Some("Trash"))
            .unwrap();
        assert!(s.draft(&a, &draft.id).is_err());
        assert!(s.draft_sync_status(&a).unwrap().is_empty());
        assert!(s.draft_conflicts(Some(&a)).unwrap().is_empty());
        assert_eq!(s.operations(Some(&a)).unwrap().len(), 1);
        assert!(s.next_draft_upload().unwrap().is_none());
        assert!(s.next_draft_refresh().unwrap().is_none());
        assert_eq!(
            s.server_draft_original(&a, &old.source.id).unwrap(),
            crate::draft_refresh::tests::raw("first")
        );
    }
    #[test]
    fn oversize_failure_does_not_starve_later_uids_and_has_visible_notice() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let version = s.account_version(&a).unwrap();
        s.draft_scan_failure(&version, "Drafts", 42, 1, "原件超过40MiB边界")
            .unwrap();
        let uids = (1..=10).rev().collect::<Vec<_>>();
        assert_eq!(
            s.draft_scan_missing(&a, "Drafts", 42, &uids).unwrap(),
            vec![2, 3, 4, 5]
        );
        assert!(s.draft_scan_notice(&a).unwrap().contains("40MiB"));
        s.finish_draft_scan(&version, "请选择服务器草稿目录")
            .unwrap();
        assert!(s.draft_scan_notice(&a).unwrap().contains("请选择"));
        s.remove_account(&a).unwrap();
    }
}

#[cfg(test)]
mod frozen_choice_tests {
    use super::*;
    use crate::draft_refresh::tests::{linked, raw, setup};
    #[test]
    fn choose_remote_reads_frozen_original_instead_of_already_edited_conversion() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let old = linked(&mut s, &a);
        let remote = s
            .archive_server_draft(&a, "Drafts", 42, 9, &raw("frozen remote"))
            .unwrap();
        s.record_draft_conflict(&a, &old.draft.id, Some(&remote.id), "conflict")
            .unwrap();
        let copy = s.convert_server_draft(&a, &remote.id).unwrap();
        let mut doc = copy.document.clone();
        doc.body = mail_content::draft::Draft::plain("edited other conversion").unwrap();
        let edited = s.save_draft(&a, &copy.id, copy.revision, &doc).unwrap();
        let conflict = s.draft_conflicts(Some(&a)).unwrap().remove(0);
        s.resolve_draft_conflict(&a, &conflict.id, DraftResolution::KeepRemote)
            .unwrap();
        let selected = s.selected_local_draft(&a).unwrap().unwrap();
        assert_ne!(selected, copy.id);
        assert_eq!(
            s.draft(&a, &selected).unwrap().document.body.text(),
            "frozen remote"
        );
        assert_eq!(s.draft(&a, &copy.id).unwrap(), edited);
        assert_eq!(s.draft(&a, &old.draft.id).unwrap(), old.draft);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::draft_refresh::tests::{linked, raw, setup};
    #[test]
    fn interrupted_discovery_association_rolls_back_uid_marker_and_can_retry() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let old = linked(&mut s, &a);
        s.save_draft(&a, &old.draft.id, old.draft.revision, &old.draft.document)
            .unwrap();
        let version = s.account_version(&a).unwrap();
        s.connection.execute_batch("CREATE TRIGGER fail_conflict_insert BEFORE INSERT ON draft_conflicts BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(
            s.import_discovered_draft(&version, "Drafts", 42, 9, &raw("remote changed"), false)
                .is_err()
        );
        assert_eq!(s.server_draft_sources(&a).unwrap().len(), 1);
        assert_eq!(
            s.draft_scan_missing(&a, "Drafts", 42, &[9, 7]).unwrap(),
            vec![9]
        );
        s.connection
            .execute_batch("DROP TRIGGER fail_conflict_insert;")
            .unwrap();
        s.import_discovered_draft(&version, "Drafts", 42, 9, &raw("remote changed"), false)
            .unwrap();
        assert_eq!(s.server_draft_sources(&a).unwrap().len(), 2);
        assert_eq!(s.draft_conflicts(Some(&a)).unwrap().len(), 1);
    }
}
