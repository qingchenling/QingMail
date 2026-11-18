use super::*;
use mail_core::{BatchKind, FolderBatch};
use mail_protocol::{DesktopPreferences, OpenMailTarget};

#[derive(Debug)]
pub struct NotificationBatch {
    pub target: OpenMailTarget,
    pub account_name: String,
    pub count: u64,
    pub subject: String,
    pub sender: String,
    pub preferences: DesktopPreferences,
}

/// Called inside the index transaction, after all messages have been published.
pub(super) fn observe_batch(
    tx: &rusqlite::Transaction<'_>,
    account: &AccountId,
    folder: &str,
    batch: &FolderBatch,
    now: i64,
) -> Result<()> {
    if !batch.remote_name.eq_ignore_ascii_case("INBOX") {
        return Ok(());
    }
    let previous: Option<(u32,u32)> = tx.query_row("SELECT uidvalidity,high_uid FROM notification_baselines WHERE account_id=?1 AND folder_id=?2",params![account.0,folder],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let high = batch
        .messages
        .iter()
        .map(|(uid, _)| *uid)
        .max()
        .unwrap_or(0);
    let reset = previous.is_none_or(|(epoch, _)| epoch != batch.uidvalidity)
        || batch.kind == BatchKind::History;
    let high = if let Some((epoch, old)) = previous {
        if epoch == batch.uidvalidity {
            old.max(high)
        } else {
            high
        }
    } else {
        high
    };
    if reset {
        tx.execute(
            "DELETE FROM notification_candidates WHERE account_id=?1 AND folder_id=?2",
            params![account.0, folder],
        )?;
        tx.execute(
            "DELETE FROM notification_pending WHERE account_id=?1 AND folder_id=?2",
            params![account.0, folder],
        )?;
    } else if let Some((_, low)) = previous
        && high > low
    {
        for (uid, _) in &batch.messages {
            if *uid > low && !batch.flags.get(uid).copied().unwrap_or_default().seen {
                tx.execute(
                    "INSERT OR IGNORE INTO notification_candidates VALUES(?1,?2,?3,?4)",
                    params![account.0, folder, batch.uidvalidity, uid],
                )?;
            }
        }
        tx.execute("INSERT INTO notification_pending VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(account_id,folder_id) DO UPDATE SET high_uid=excluded.high_uid",params![account.0,folder,batch.uidvalidity,low,high,now])?;
    }
    tx.execute("INSERT INTO notification_baselines(account_id,folder_id,uidvalidity,high_uid) VALUES(?1,?2,?3,?4) ON CONFLICT(account_id,folder_id) DO UPDATE SET uidvalidity=excluded.uidvalidity,high_uid=excluded.high_uid",params![account.0,folder,batch.uidvalidity,high])?;
    Ok(())
}
impl Store {
    pub fn desktop_preferences(&self) -> Result<DesktopPreferences> {
        Ok(self.connection.query_row("SELECT revision,notifications,show_sender_subject,show_preview FROM desktop_settings WHERE id=1",[],|r|Ok(DesktopPreferences {revision:r.get::<_,i64>(0)? as u64,notifications:r.get(1)?,show_sender_subject:r.get(2)?,show_preview:r.get(3)?}))?)
    }
    pub fn update_desktop_preferences(
        &mut self,
        prefs: &DesktopPreferences,
    ) -> Result<DesktopPreferences> {
        let tx = self.connection.transaction()?;
        if tx.execute("UPDATE desktop_settings SET revision=revision+1,notifications=?1,show_sender_subject=?2,show_preview=?3 WHERE id=1 AND revision=?4",params![prefs.notifications,prefs.show_sender_subject,prefs.show_preview,i64::try_from(prefs.revision).map_err(|_| Error::Conflict)?])? != 1 { return Err(Error::Conflict); }
        // A setting change never releases a backlog of old mail.
        tx.execute("DELETE FROM notification_pending", [])?;
        tx.execute("DELETE FROM notification_candidates", [])?;
        tx.commit()?;
        self.desktop_preferences()
    }
    /// Consumes ready intervals durably BEFORE any external D-Bus call. Unknown results are never replayed.
    pub fn take_notifications(&mut self, now: i64) -> Result<Vec<NotificationBatch>> {
        let preferences = self.desktop_preferences()?;
        let tx = self.connection.transaction()?;
        let pending:Vec<(String,String,u32,u32,u32)>=tx.prepare("SELECT p.account_id,p.folder_id,p.uidvalidity,p.low_uid,p.high_uid FROM notification_pending p JOIN notification_baselines b USING(account_id,folder_id) WHERE p.created_at<=?1-2 AND b.next_attempt<=?1 ORDER BY p.created_at LIMIT 16")?.query_map([now],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<std::result::Result<_,_>>()?;
        let mut batches = vec![];
        for (account, folder, epoch, low, high) in pending {
            let count:i64=tx.query_row("SELECT count(*) FROM visible_folder_messages v JOIN notification_candidates n ON n.account_id=v.account_id AND n.folder_id=v.folder_id AND n.uidvalidity=v.uidvalidity AND n.uid=v.uid JOIN folders f ON f.id=v.folder_id AND f.account_id=v.account_id AND f.uidvalidity=v.uidvalidity WHERE v.account_id=?1 AND v.folder_id=?2 AND v.uidvalidity=?3 AND v.uid>?4 AND v.uid<=?5 AND v.seen=0",params![account,folder,epoch,low,high],|r|r.get(0))?;
            if count > 0 && preferences.notifications {
                let (uid,subject,sender):(u32,String,String)=tx.query_row("SELECT v.uid,m.subject,m.sender FROM visible_folder_messages v JOIN notification_candidates n ON n.account_id=v.account_id AND n.folder_id=v.folder_id AND n.uidvalidity=v.uidvalidity AND n.uid=v.uid JOIN messages m ON m.id=v.message_id AND m.account_id=v.account_id WHERE v.account_id=?1 AND v.folder_id=?2 AND v.uidvalidity=?3 AND v.uid>?4 AND v.uid<=?5 AND v.seen=0 ORDER BY v.uid DESC LIMIT 1",params![account,folder,epoch,low,high],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                let name: String = tx.query_row(
                    "SELECT display_name FROM accounts WHERE id=?1",
                    [&account],
                    |r| r.get(0),
                )?;
                let account = AccountId(account.clone());
                batches.push(NotificationBatch {
                    target: OpenMailTarget {
                        account: account.clone(),
                        message: if count == 1 {
                            Some(MailUid {
                                account,
                                folder_id: folder.clone(),
                                uidvalidity: epoch,
                                uid,
                            })
                        } else {
                            None
                        },
                    },
                    account_name: name,
                    count: count as u64,
                    subject,
                    sender,
                    preferences: preferences.clone(),
                });
            }
            tx.execute(
                "DELETE FROM notification_candidates WHERE account_id=?1 AND folder_id=?2",
                params![account, folder],
            )?;
            tx.execute(
                "DELETE FROM notification_pending WHERE account_id=?1 AND folder_id=?2",
                params![account, folder],
            )?;
            tx.execute("UPDATE notification_baselines SET next_attempt=?3 WHERE account_id=?1 AND folder_id=?2",params![account,folder,now+10])?;
        }
        tx.commit()?;
        Ok(batches)
    }
    pub fn validate_notification_target(&self, target: &OpenMailTarget) -> Result<bool> {
        if self.account(&target.account)?.is_none() {
            return Ok(false);
        }
        match &target.message {
            Some(m) if m.account == target.account => Ok(self.check_message_location(m).is_ok()),
            Some(_) => Ok(false),
            None => Ok(true),
        }
    }
}
impl Store {
    pub fn notification_due(&self) -> Result<Option<i64>> {
        Ok(self.connection.query_row("SELECT min(max(p.created_at+2,b.next_attempt)) FROM notification_pending p JOIN notification_baselines b USING(account_id,folder_id)",[],|r|r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Store, AccountId) {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(&root.path().join("data")).unwrap();
        let account = store.create_account(&crate::tests::settings()).unwrap();
        store
            .replace_remote_folders(
                &account,
                &[mail_core::RemoteFolder {
                    name: "INBOX".into(),
                    display_name: "收件箱".into(),
                    delimiter: None,
                    selectable: true,
                }],
                1,
            )
            .unwrap();
        (root, store, account)
    }
    fn batch(epoch: u32, uids: &[u32]) -> FolderBatch {
        FolderBatch {
            remote_name: "INBOX".into(),
            uidvalidity: epoch,
            since: 0,
            has_more: false,
            complete: true,
            kind: BatchKind::Recent,
            messages: uids
                .iter()
                .map(|uid| {
                    (
                        *uid,
                        MessageEnvelope {
                            subject: format!("subject {uid}"),
                            sender: "sender".into(),
                            received_at: 1,
                        },
                    )
                })
                .collect(),
            flags: Default::default(),
        }
    }
    #[test]
    fn notification_baseline_duplicate_history_epoch_and_restart() {
        let (root, mut s, a) = setup();
        s.apply_folder_batch(&a, &batch(1, &[1, 2]), 1).unwrap();
        assert!(s.take_notifications(10).unwrap().is_empty());
        s.apply_folder_batch(&a, &batch(1, &[1, 2, 3]), 11).unwrap();
        assert!(s.take_notifications(12).unwrap().is_empty());
        s.apply_folder_batch(&a, &batch(1, &[1, 2, 3]), 12).unwrap();
        drop(s);
        let mut s = Store::open(&root.path().join("data")).unwrap();
        let n = s.take_notifications(13).unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].count, 1);
        assert_eq!(n[0].target.message.as_ref().unwrap().uid, 3);
        assert!(s.take_notifications(100).unwrap().is_empty());
        let mut history = batch(1, &[1]);
        history.kind = BatchKind::History;
        s.apply_folder_batch(&a, &history, 101).unwrap();
        assert!(s.take_notifications(105).unwrap().is_empty());
        s.apply_folder_batch(&a, &batch(2, &[1, 2, 3, 4]), 106)
            .unwrap();
        assert!(s.take_notifications(110).unwrap().is_empty());
        s.apply_folder_batch(&a, &batch(2, &[1, 2, 3, 4, 5]), 111)
            .unwrap();
        assert_eq!(s.take_notifications(113).unwrap()[0].count, 1);
    }
    #[test]
    fn notification_coalescing_cooldown_and_account_isolation() {
        let (_root, mut s, a) = setup();
        let b = s.create_account(&crate::tests::settings()).unwrap();
        let catalog = s.remote_folders(&a).unwrap();
        s.replace_remote_folders(&b, &catalog, 1).unwrap();
        for id in [&a, &b] {
            s.apply_folder_batch(id, &batch(1, &[1]), 1).unwrap();
        }
        s.apply_folder_batch(&a, &batch(1, &[1, 2]), 10).unwrap();
        s.apply_folder_batch(&a, &batch(1, &[1, 2, 3]), 11).unwrap();
        s.apply_folder_batch(&b, &batch(1, &[1, 2]), 10).unwrap();
        let n = s.take_notifications(12).unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(n.iter().find(|n| n.target.account == a).unwrap().count, 2);
        assert!(
            n.iter()
                .find(|n| n.target.account == a)
                .unwrap()
                .target
                .message
                .is_none()
        );
        s.apply_folder_batch(&a, &batch(1, &[1, 2, 3, 4]), 13)
            .unwrap();
        assert_eq!(s.notification_due().unwrap(), Some(22));
        assert!(s.take_notifications(21).unwrap().is_empty());
        assert_eq!(s.take_notifications(22).unwrap()[0].count, 1);
    }
    #[test]
    fn notification_filters_seen_deleted_and_initially_seen() {
        let (_root, mut s, a) = setup();
        s.apply_folder_batch(&a, &batch(1, &[1]), 1).unwrap();
        let mut arrival = batch(1, &[1, 2, 3, 4]);
        arrival.flags.insert(
            4,
            mail_core::MessageFlags {
                seen: true,
                flagged: false,
            },
        );
        s.apply_folder_batch(&a, &arrival, 10).unwrap();
        let mut update = batch(1, &[1, 2, 4]);
        update.flags.insert(
            2,
            mail_core::MessageFlags {
                seen: true,
                flagged: false,
            },
        );
        s.apply_folder_batch(&a, &update, 11).unwrap();
        // UID3 deleted, UID2 read, UID4 was initially seen; none may notify.
        assert!(s.take_notifications(12).unwrap().is_empty());
    }
    #[test]
    fn notification_off_setting_revision_and_atomic_failure() {
        let (_root, mut s, a) = setup();
        s.apply_folder_batch(&a, &batch(1, &[1]), 1).unwrap();
        let mut p = s.desktop_preferences().unwrap();
        assert!(p.notifications);
        assert!(!p.show_sender_subject && !p.show_preview);
        p.notifications = false;
        let saved = s.update_desktop_preferences(&p).unwrap();
        assert_eq!(saved.revision, 1);
        assert!(matches!(
            s.update_desktop_preferences(&p),
            Err(Error::Conflict)
        ));
        s.apply_folder_batch(&a, &batch(1, &[1, 2]), 10).unwrap();
        assert!(s.take_notifications(12).unwrap().is_empty());
        let mut p = saved;
        p.notifications = true;
        s.update_desktop_preferences(&p).unwrap();
        assert!(s.take_notifications(30).unwrap().is_empty());
        s.connection.execute_batch("CREATE TRIGGER fail_notification BEFORE INSERT ON notification_candidates BEGIN SELECT RAISE(ABORT,'disk failure'); END;").unwrap();
        assert!(s.apply_folder_batch(&a, &batch(1, &[1, 2, 3]), 31).is_err());
        assert_eq!(
            s.folder_cache(&a, "INBOX", None)
                .unwrap()
                .unwrap()
                .messages
                .len(),
            2
        );
        s.connection
            .execute_batch("DROP TRIGGER fail_notification")
            .unwrap();
        s.apply_folder_batch(&a, &batch(1, &[1, 2, 3]), 32).unwrap();
        assert_eq!(s.take_notifications(34).unwrap()[0].count, 1);
    }
    #[test]
    fn notification_schema17_migration_and_account_removal() {
        let (root, mut s, a) = setup();
        s.apply_folder_batch(&a, &batch(1, &[1]), 1).unwrap();
        s.connection.execute_batch("DROP TABLE notification_candidates; DROP TABLE notification_pending; DROP TABLE notification_baselines; DROP TABLE desktop_settings; PRAGMA user_version=17;").unwrap();
        drop(s);
        let mut s = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(
            s.desktop_preferences().unwrap(),
            DesktopPreferences::default()
        );
        assert_eq!(
            s.folder_cache(&a, "INBOX", None)
                .unwrap()
                .unwrap()
                .messages
                .len(),
            1
        );
        assert!(
            std::fs::read_dir(root.path().join("data"))
                .unwrap()
                .any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("migration-v17-"))
        );
        s.apply_folder_batch(&a, &batch(1, &[1]), 10).unwrap();
        s.apply_folder_batch(&a, &batch(1, &[1, 2]), 11).unwrap();
        let target = OpenMailTarget {
            account: a.clone(),
            message: Some(
                s.folder_cache(&a, "INBOX", None).unwrap().unwrap().messages[0]
                    .location
                    .clone(),
            ),
        };
        assert!(s.validate_notification_target(&target).unwrap());
        s.remove_account(&a).unwrap();
        assert!(s.take_notifications(20).unwrap().is_empty());
        assert!(!s.validate_notification_target(&target).unwrap());
    }
    #[test]
    fn notification_offline_read_overlay_suppresses_pending_alerts() {
        let (_root, mut s, a) = setup();
        s.apply_folder_batch(&a, &batch(1, &[1]), 1).unwrap();
        s.apply_folder_batch(&a, &batch(1, &[1, 2]), 10).unwrap();
        let location = s.folder_cache(&a, "INBOX", None).unwrap().unwrap().messages[0]
            .location
            .clone();
        let request = mail_protocol::Request::new(
            "f".repeat(32),
            mail_protocol::Command::EnqueueOperation {
                locations: vec![location],
                operation: mail_protocol::MailOperation::Flags {
                    seen: Some(true),
                    flagged: None,
                },
            },
        );
        s.enqueue_operations_with_trash(&request, &Default::default())
            .unwrap();
        assert!(s.take_notifications(12).unwrap().is_empty());
    }
}
