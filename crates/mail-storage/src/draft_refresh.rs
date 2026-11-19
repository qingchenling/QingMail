//! 已关联原件的低频复查；落盘前重新核对共同基线，绝不覆盖并发本地编辑。
use crate::*;

#[derive(Debug, Clone)]
pub struct DraftRefresh {
    pub version: AccountVersion,
    pub draft: StoredDraft,
    pub source: mail_protocol::ServerDraftSource,
}

impl Store {
    pub fn next_draft_refresh(&mut self) -> Result<Option<DraftRefresh>> {
        let row: Option<(String, String, String)> = self.connection.query_row(
            "SELECT s.account_id,s.draft_id,s.base_source FROM draft_sync s JOIN account_options a ON a.account_id=s.account_id AND a.paused=0 JOIN drafts d ON d.id=s.draft_id AND d.account_id=s.account_id WHERE s.state='synced' AND s.base_source IS NOT NULL AND d.revision=s.base_revision AND s.attempted_at<=unixepoch()-60 AND NOT EXISTS(SELECT 1 FROM send_tasks t WHERE t.account_id=s.account_id AND t.source_draft=s.draft_id AND t.source_revision>=d.revision) ORDER BY s.attempted_at,s.account_id,s.draft_id LIMIT 1",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).optional()?;
        let Some((account, draft, source)) = row else {
            return Ok(None);
        };
        self.connection.execute(
            "UPDATE draft_sync SET attempted_at=unixepoch() WHERE account_id=?1 AND draft_id=?2",
            params![account, draft],
        )?;
        let account = AccountId(account);
        Ok(Some(DraftRefresh {
            version: self.account_version(&account)?,
            draft: self.draft(&account, &mail_core::DraftId(draft))?,
            source: self.server_draft_source(&account, &source)?,
        }))
    }

    /// None仅用于远端定位无法验证；网络暂时失败由调用方退避，不冒充远端删除。
    pub fn observe_draft_remote(
        &mut self,
        refresh: &DraftRefresh,
        raw: Option<&[u8]>,
    ) -> Result<bool> {
        self.observe_draft_remote_at(refresh, raw, None)
    }
    pub(super) fn observe_draft_remote_at(
        &mut self,
        refresh: &DraftRefresh,
        raw: Option<&[u8]>,
        remote: Option<&mail_protocol::ServerDraftSource>,
    ) -> Result<bool> {
        let account = &refresh.draft.account;
        let id = &refresh.draft.id;
        if self.account_version(account)? != refresh.version {
            return Ok(false);
        }
        let blocked: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM draft_uploads WHERE account_id=?1 AND draft_id=?2 AND state IN ('prepared','in_flight','uncertain')) OR EXISTS(SELECT 1 FROM send_tasks WHERE account_id=?1 AND source_draft=?2 AND source_revision>=?3)",
            params![account.0,id.0,refresh.draft.revision], |r| r.get(0),
        )?;
        if blocked {
            return Ok(false);
        }
        let current: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT base_source,state FROM draft_sync WHERE account_id=?1 AND draft_id=?2",
                params![account.0, id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((base, state)) = current else {
            return Ok(false);
        };
        // 迟到的网络结果不得解除未知上传或覆盖已经更新的关联。
        if base != refresh.source.id || !matches!(state.as_str(), "synced" | "pending") {
            return Ok(false);
        }
        if self.server_draft_folder(account)?.as_deref() != Some(&refresh.source.folder) {
            self.record_draft_conflict(
                account,
                id,
                remote.map(|s| s.id.as_str()),
                "草稿目录已变化；原件与本地副本保留",
            )?;
            return Ok(true);
        }
        let Some(raw) = raw else {
            self.record_draft_conflict(
                account,
                id,
                remote.map(|s| s.id.as_str()),
                "远端定位无法验证；可能已删除或UIDVALIDITY变化，保留本地与原件，请核对",
            )?;
            return Ok(true);
        };
        if raw == self.server_draft_original(account, &base)? {
            if let Some(remote) = remote {
                self.connection.execute("UPDATE server_draft_sources SET converted=?1,baseline_revision=(SELECT base_revision FROM draft_sync WHERE account_id=?2 AND draft_id=?1) WHERE id=?3 AND account_id=?2",params![id.0,account.0,remote.id])?;
                self.connection.execute(
                    "UPDATE draft_sync SET base_source=?1 WHERE account_id=?2 AND draft_id=?3",
                    params![remote.id, account.0, id.0],
                )?;
                return Ok(true);
            }
            return Ok(false);
        }
        let source = if let Some(remote) = remote {
            remote.clone()
        } else {
            self.archive_server_draft(
                account,
                &refresh.source.folder,
                refresh.source.uidvalidity,
                refresh.source.uid,
                raw,
            )?
        };
        let local = self.draft(account, id)?;
        // 外部HTML等转换可能丢格式，只保留原件，等待用户明确转换。
        let copy = mail_content::lossless_server_copy(raw);
        if local.revision != refresh.draft.revision || state != "synced" || copy.is_none() {
            self.record_draft_conflict(
                account,
                id,
                Some(&source.id),
                "远端已变化；已保留新原件和本地副本，请在服务器草稿中查看，未自动覆盖",
            )?;
            return Ok(true);
        }
        self.connection
            .execute_batch("SAVEPOINT refresh_server_draft")?;
        let result = (|| {
            let document = self.server_copy_document(account, copy.expect("validated"))?;
            let saved = self.save_draft(account, id, local.revision, &document)?;
            self.connection.execute("UPDATE server_draft_sources SET converted=?1,baseline_revision=?2 WHERE id=?3 AND account_id=?4", params![id.0,saved.revision,source.id,account.0])?;
            self.connection.execute("UPDATE draft_sync SET base_source=?1,base_revision=?2,state='synced',detail='远端更新已保存；旧原件保留' WHERE account_id=?3 AND draft_id=?4", params![source.id,saved.revision,account.0,id.0])?;
            Ok(true)
        })();
        if result.is_ok() {
            self.connection
                .execute_batch("RELEASE refresh_server_draft")?;
        } else {
            self.connection
                .execute_batch("ROLLBACK TO refresh_server_draft; RELEASE refresh_server_draft")?;
            self.record_draft_conflict(
                account,
                id,
                Some(&source.id),
                "远端更新无法导入；新原件与本地副本保留",
            )?;
            return Ok(true);
        }
        result
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    pub(crate) fn setup(s: &mut Store) -> AccountId {
        let server = ServerSettings {
            authentication: Default::default(),
            host: "example.invalid".into(),
            port: 993,
            username: "fixture".into(),
            security: ConnectionSecurity::Tls,
        };
        let a = s
            .create_account(&AccountSettings {
                display_name: "drafts".into(),
                address: "me@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        s.replace_remote_folders(
            &a,
            &[mail_core::RemoteFolder {
                name: "Drafts".into(),
                display_name: "Drafts".into(),
                delimiter: None,
                selectable: true,
            }],
            0,
        )
        .unwrap();
        s.set_server_draft_folder(&a, "Drafts").unwrap();
        a
    }
    pub(crate) fn raw(text: &str) -> Vec<u8> {
        let model = mail_content::draft::Draft::plain(text).unwrap();
        format!("Message-ID: <fixture@example.invalid>\r\nSubject: remote\r\nBcc: hidden@example.invalid\r\nContent-Type: multipart/mixed; boundary=fixture\r\n\r\n--fixture\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n--fixture\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{}\r\n--fixture\r\nContent-Type: application/vnd.qingmail.draft+json\r\n\r\n{}\r\n--fixture--\r\n", model.plain_export(),model.html(),String::from_utf8(model.to_json().unwrap()).unwrap()).into_bytes()
    }
    pub(crate) fn linked(s: &mut Store, a: &AccountId) -> DraftRefresh {
        let source = s
            .archive_server_draft(a, "Drafts", 42, 7, &raw("first"))
            .unwrap();
        let draft = s.convert_server_draft(a, &source.id).unwrap();
        DraftRefresh {
            version: s.account_version(a).unwrap(),
            draft,
            source,
        }
    }
    #[test]
    fn remote_refresh_is_atomic_retains_original_and_rejects_late_editor_save() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        let mut s = Store::open(&path).unwrap();
        let a = setup(&mut s);
        let refresh = linked(&mut s, &a);
        let updated = raw("远端更新");
        assert!(mail_content::server_draft_model(&updated).is_some());
        assert!(s.observe_draft_remote(&refresh, Some(&updated)).unwrap());
        let saved = s.draft(&a, &refresh.draft.id).unwrap();
        assert_eq!(saved.document.body.text(), "远端更新");
        assert_eq!(saved.document.headers.bcc, vec!["hidden@example.invalid"]);
        assert!(
            s.save_draft(
                &a,
                &saved.id,
                refresh.draft.revision,
                &refresh.draft.document
            )
            .is_err()
        );
        assert!(!s.observe_draft_remote(&refresh, None).unwrap());
        assert_eq!(
            s.server_draft_original(&a, &refresh.source.id).unwrap(),
            raw("first")
        );
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.draft(&a, &saved.id).unwrap(), saved);
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "synced");
    }
    #[test]
    fn concurrent_edit_and_external_conversion_preserve_both_without_upload() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let refresh = linked(&mut s, &a);
        let mut doc = refresh.draft.document.clone();
        doc.body = mail_content::draft::Draft::plain("offline edit").unwrap();
        let local = s
            .save_draft(&a, &refresh.draft.id, refresh.draft.revision, &doc)
            .unwrap();
        assert!(
            s.observe_draft_remote(&refresh, Some(&raw("other client")))
                .unwrap()
        );
        assert_eq!(s.draft(&a, &local.id).unwrap(), local);
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "conflict");
        s.flush_draft_sync(&a).unwrap();
        assert!(s.next_draft_upload().unwrap().is_none());
        assert_eq!(s.server_draft_sources(&a).unwrap().len(), 2);
        // 外部HTML没有可验证的富文本模型，不可自动转换并覆盖干净本地版本。
        let b = setup(&mut s);
        let refresh = linked(&mut s, &b);
        let external = b"Subject: external\r\nContent-Type: text/html\r\n\r\n<b>external</b>";
        assert!(s.observe_draft_remote(&refresh, Some(external)).unwrap());
        assert_eq!(s.draft(&b, &refresh.draft.id).unwrap(), refresh.draft);
        assert_eq!(s.draft_sync_status(&b).unwrap()[0].state, "conflict");
    }
    #[test]
    fn missing_location_paused_accounts_and_unknown_uploads_remain_safe() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let refresh = linked(&mut s, &a);
        s.connection
            .execute(
                "UPDATE account_options SET paused=1 WHERE account_id=?1",
                [&a.0],
            )
            .unwrap();
        assert!(s.next_draft_refresh().unwrap().is_none());
        s.connection
            .execute(
                "UPDATE account_options SET paused=0 WHERE account_id=?1",
                [&a.0],
            )
            .unwrap();
        assert_eq!(
            s.next_draft_refresh().unwrap().unwrap().draft.id,
            refresh.draft.id
        );
        assert!(s.next_draft_refresh().unwrap().is_none());
        s.fail_draft_sync(&a, &refresh.draft.id, "uncertain", "lost response")
            .unwrap();
        assert!(!s.observe_draft_remote(&refresh, None).unwrap());
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "uncertain");
        s.fail_draft_sync(&a, &refresh.draft.id, "pending", "local edit")
            .unwrap();
        assert!(s.observe_draft_remote(&refresh, None).unwrap());
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "conflict");
        assert_eq!(s.draft(&a, &refresh.draft.id).unwrap(), refresh.draft);
    }
    #[test]
    fn changed_credentials_and_pending_upload_block_late_remote_results() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let refresh = linked(&mut s, &a);
        let mut stale = refresh.clone();
        stale.version.settings.imap.host = "old.example.invalid".into();
        assert!(
            !s.observe_draft_remote(&stale, Some(&raw("remote")))
                .unwrap()
        );
        let id = "e".repeat(32);
        s.prepare_draft_upload(&refresh.draft, "Drafts", Some(42), &id, &raw("frozen"))
            .unwrap();
        assert!(
            !s.observe_draft_remote(&refresh, Some(&raw("remote")))
                .unwrap()
        );
        assert_eq!(s.draft(&a, &refresh.draft.id).unwrap(), refresh.draft);
        assert_eq!(s.server_draft_sources(&a).unwrap().len(), 1);
        assert!(!s.draft_upload(&a, &id).unwrap().probe_only);
    }
    #[test]
    fn invalid_remote_attachment_rolls_back_import_but_keeps_new_original() {
        let root = tempfile::tempdir().unwrap();
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let a = setup(&mut s);
        let refresh = linked(&mut s, &a);
        let updated = String::from_utf8(raw("remote")).unwrap().replace("--fixture--\r\n", "--fixture\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=ok.txt\r\n\r\nok\r\n--fixture\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=../bad\r\n\r\nbad\r\n--fixture--\r\n");
        assert!(mail_content::server_draft_model(updated.as_bytes()).is_some());
        assert!(
            s.observe_draft_remote(&refresh, Some(updated.as_bytes()))
                .unwrap()
        );
        assert_eq!(s.draft(&a, &refresh.draft.id).unwrap(), refresh.draft);
        assert_eq!(s.draft_sync_status(&a).unwrap()[0].state, "conflict");
        let sources = s.server_draft_sources(&a).unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(
            s.server_draft_original(&a, &sources[0].id).unwrap(),
            updated.as_bytes()
        );
        let count: u32 = s
            .connection
            .query_row(
                "SELECT count(*) FROM attachments WHERE account_id=?1",
                [&a.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}
