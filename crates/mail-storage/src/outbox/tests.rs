use super::*;
use crate::{
    APPLICATION_ID, PrivateDir, SCHEMA, SCHEMA_V2, SCHEMA_V3, SCHEMA_VERSION, migrate,
    tests::settings,
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    process::Command,
};

fn document(text: &str) -> DraftDocument {
    let mut body = Draft::plain(text).unwrap();
    if !text.is_empty() {
        body.inline(0..text.len(), |a| a.bold = true).unwrap();
    }
    DraftDocument {
        headers: DraftHeaders {
            subject: "主题🙂".into(),
            to: vec!["to@example.org".into()],
            cc: vec!["cc@example.org".into()],
            bcc: vec!["bcc@example.org".into()],
        },
        body,
        attachments: vec![],
    }
}
fn setup() -> (tempfile::TempDir, Store, AccountId) {
    let root = tempfile::tempdir().unwrap();
    let mut store = Store::open(&root.path().join("qingmail")).unwrap();
    let account = store.create_account(&settings()).unwrap();
    (root, store, account)
}
fn task(store: &mut Store, account: &AccountId) -> SendTaskId {
    let draft = store
        .create_draft(account, &document("你好，world🙂"))
        .unwrap();
    store
        .enqueue_draft(account, &draft.id, draft.revision)
        .unwrap()
}

#[test]
fn multiple_drafts_roundtrip_formats_headers_attachments_and_revision_conflicts() {
    let (root, mut store, account) = setup();
    let attachment = store
        .import_attachment(
            &account,
            "中文附件.txt",
            "text/plain",
            &mut "内容🙂".as_bytes(),
        )
        .unwrap();
    let mut doc = document("你好 world🙂");
    doc.attachments.push(attachment.id.clone());
    let a = store.create_draft(&account, &doc).unwrap();
    let b = store
        .create_draft(&account, &document("另一份草稿"))
        .unwrap();
    assert_ne!(a.id, b.id);
    let changed = document("更新的文字");
    let newer = store.save_draft(&account, &a.id, 1, &changed).unwrap();
    assert_eq!(newer.revision, 2);
    assert!(matches!(
        store.save_draft(&account, &a.id, 1, &doc),
        Err(Error::Conflict)
    ));
    drop(store);
    let store = Store::open(&root.path().join("qingmail")).unwrap();
    assert_eq!(store.draft_ids(&account).unwrap().len(), 2);
    assert_eq!(store.draft(&account, &a.id).unwrap(), newer);
    assert_eq!(store.draft(&account, &b.id).unwrap(), b);
    assert_eq!(
        store.attachment(&account, &attachment.id).unwrap(),
        attachment
    );
}

#[test]
fn task_snapshot_stays_frozen_after_draft_change_and_discard() {
    let (_root, mut store, account) = setup();
    let a = store
        .import_attachment(&account, "a.txt", "text/plain", &mut b"original".as_slice())
        .unwrap();
    let mut doc = document("原始正文");
    doc.attachments.push(a.id.clone());
    let draft = store.create_draft(&account, &doc).unwrap();
    let id = store.enqueue_draft(&account, &draft.id, 1).unwrap();
    assert_eq!(store.enqueue_draft(&account, &draft.id, 1).unwrap(), id);
    let snapshot = store.task_snapshot(&account, &id).unwrap();
    assert!(snapshot.html.contains("<strong>"));
    assert_eq!(snapshot.headers, doc.headers);
    assert_eq!(snapshot.attachments[0].bytes, b"original");
    let new = store
        .save_draft(&account, &draft.id, 1, &document("不能改变发送内容"))
        .unwrap();
    store
        .discard_draft(&account, &draft.id, new.revision)
        .unwrap();
    assert_eq!(store.task_snapshot(&account, &id).unwrap(), snapshot);
    assert!(
        store
            .connection
            .execute("DELETE FROM attachments WHERE id=?1", [&a.id.0])
            .is_err()
    );
    assert!(
        store
            .connection
            .execute("DELETE FROM objects WHERE id=?1", [&a.id.0])
            .is_err()
    );
    let attempt = store.begin_smtp(&account, &id).unwrap();
    assert_eq!(attempt.snapshot(), &snapshot);
    assert!(store.begin_smtp(&account, &id).is_err());
    store.finish_smtp(attempt, SmtpOutcome::Accepted).unwrap();
    assert_eq!(
        store.task(&account, &id).unwrap().state,
        SendState::CopyPending
    );
    assert!(store.begin_smtp(&account, &id).is_err());
    assert!(
        store
            .resend_task(&account, &id, ResendAuthorization::UserConfirmed)
            .is_err()
    );
    assert_eq!(store.begin_copy(&account, &id).unwrap(), snapshot);
    store
        .finish_copy(
            &account,
            &id,
            CopyOutcome::Failed {
                code: "copy_append_rejected",
            },
        )
        .unwrap();
    assert_eq!(
        store.task(&account, &id).unwrap().state,
        SendState::CopyPending
    );
    store
        .finish_copy(
            &account,
            &id,
            CopyOutcome::Saved {
                folder: "Sent".into(),
                uidvalidity: 42,
                uid: Some(7),
                existed: false,
            },
        )
        .unwrap();
    assert_eq!(store.task(&account, &id).unwrap().state, SendState::Sent);
    assert!(store.begin_smtp(&account, &id).is_err());
    assert!(store.begin_copy(&account, &id).is_err());
}

#[test]
fn restart_never_resumes_pending_or_sending_and_keeps_copy_only_path() {
    let (root, mut store, account) = setup();
    let pending = task(&mut store, &account);
    let sending = task(&mut store, &account);
    let _attempt = store.begin_smtp(&account, &sending).unwrap();
    let copy = task(&mut store, &account);
    let accepted = store.begin_smtp(&account, &copy).unwrap();
    store.finish_smtp(accepted, SmtpOutcome::Accepted).unwrap();
    let unsent = store
        .create_draft(&account, &document("离线编辑，不自动发送"))
        .unwrap();
    drop(store);
    let mut store = Store::open(&root.path().join("qingmail")).unwrap();
    assert_eq!(
        store.task(&account, &pending).unwrap().state,
        SendState::Failed
    );
    assert_eq!(
        store.task(&account, &sending).unwrap().state,
        SendState::Uncertain
    );
    assert_eq!(
        store.task(&account, &copy).unwrap().state,
        SendState::CopyPending
    );
    for id in [&pending, &sending, &copy] {
        assert!(store.begin_smtp(&account, id).is_err());
    }
    assert_eq!(store.task_ids(&account).unwrap().len(), 3);
    assert_eq!(store.draft(&account, &unsent.id).unwrap(), unsent);
    assert!(store.begin_copy(&account, &copy).is_ok());
    drop(store);
    let store = Store::open(&root.path().join("qingmail")).unwrap();
    assert_eq!(
        store.task(&account, &sending).unwrap().state,
        SendState::Uncertain
    );
}

#[test]
fn explicit_resend_creates_one_new_task_and_preserves_uncertain_record() {
    let (_root, mut store, account) = setup();
    for outcome in [SmtpOutcome::Uncertain, SmtpOutcome::DefinitelyRejected] {
        let previous = task(&mut store, &account);
        let attempt = store.begin_smtp(&account, &previous).unwrap();
        store.finish_smtp(attempt, outcome).unwrap();
        let original = store.task(&account, &previous).unwrap();
        assert!(store.begin_smtp(&account, &previous).is_err());
        let next = store
            .resend_task(&account, &previous, ResendAuthorization::UserConfirmed)
            .unwrap();
        assert_ne!(next, previous);
        assert_eq!(
            store
                .resend_task(&account, &previous, ResendAuthorization::UserConfirmed)
                .unwrap(),
            next
        );
        assert_eq!(store.task(&account, &previous).unwrap(), original);
        assert_eq!(
            store.task(&account, &next).unwrap().resend_of,
            Some(previous.clone())
        );
        let next_snapshot = store.task_snapshot(&account, &next).unwrap();
        let previous_snapshot = store.task_snapshot(&account, &previous).unwrap();
        assert_ne!(next_snapshot.message_id, previous_snapshot.message_id);
        assert_eq!(next_snapshot.headers, previous_snapshot.headers);
        assert_eq!(next_snapshot.plain, previous_snapshot.plain);
        assert_eq!(next_snapshot.html, previous_snapshot.html);
        assert!(store.begin_copy(&account, &next).is_err());
        let attempt = store.begin_smtp(&account, &next).unwrap();
        store
            .finish_smtp(attempt, SmtpOutcome::DefinitelyRejected)
            .unwrap();
    }
}

#[test]
fn cross_account_references_invalid_headers_and_duplicate_attachments_are_rejected() {
    let (_root, mut store, account) = setup();
    let other = store.create_account(&settings()).unwrap();
    let attachment = store
        .import_attachment(
            &account,
            "a",
            "application/octet-stream",
            &mut b"a".as_slice(),
        )
        .unwrap();
    let mut doc = document("body");
    doc.attachments.push(attachment.id.clone());
    assert!(store.create_draft(&other, &doc).is_err());
    doc.attachments.push(attachment.id);
    assert!(store.create_draft(&account, &doc).is_err());
    let draft = store.create_draft(&account, &document("body")).unwrap();
    assert!(store.draft(&other, &draft.id).is_err());
    assert!(store.enqueue_draft(&other, &draft.id, 1).is_err());
    let id = store.enqueue_draft(&account, &draft.id, 1).unwrap();
    assert!(store.begin_smtp(&other, &id).is_err());
    let mut incomplete = document("body");
    incomplete.headers.to = vec!["unfinished".into()];
    incomplete.headers.cc.clear();
    incomplete.headers.bcc.clear();
    let draft = store.create_draft(&account, &incomplete).unwrap();
    assert!(store.enqueue_draft(&account, &draft.id, 1).is_err());
    incomplete.headers.subject = "header\r\ninjection".into();
    assert!(store.create_draft(&account, &incomplete).is_err());
    assert!(
        store
            .import_attachment(&account, "../bad", "text/plain", &mut b"x".as_slice())
            .is_err()
    );
    assert!(
        store
            .import_attachment(
                &account,
                "bad",
                "text/plain\r\nX: bad",
                &mut b"x".as_slice()
            )
            .is_err()
    );
}

#[test]
fn attachment_tampering_or_symlink_prevents_smtp_without_changing_state() {
    let (root, mut store, account) = setup();
    let attachment = store
        .import_attachment(&account, "a.txt", "text/plain", &mut b"original".as_slice())
        .unwrap();
    let mut doc = document("body");
    doc.attachments.push(attachment.id.clone());
    let draft = store.create_draft(&account, &doc).unwrap();
    let id = store.enqueue_draft(&account, &draft.id, 1).unwrap();
    let file = root.path().join("qingmail/objects").join(&attachment.id.0);
    fs::write(&file, b"modified").unwrap();
    assert!(store.begin_smtp(&account, &id).is_err());
    assert_eq!(store.task(&account, &id).unwrap().state, SendState::Pending);
    fs::remove_file(&file).unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, b"original").unwrap();
    symlink(&outside, &file).unwrap();
    assert!(store.begin_smtp(&account, &id).is_err());
    assert_eq!(store.task(&account, &id).unwrap().state, SendState::Pending);
}

#[test]
fn failed_draft_or_task_write_retains_saved_revision_and_no_send_permission() {
    let (root, mut store, account) = setup();
    let path = root.path().join("qingmail");
    let draft = store
        .create_draft(&account, &document("最后成功保存"))
        .unwrap();
    let object_count = fs::read_dir(path.join("objects")).unwrap().count();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
    let save = store.save_draft(&account, &draft.id, 1, &document("失败的修改"));
    let enqueue = store.enqueue_draft(&account, &draft.id, 1);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(save.is_err());
    assert!(enqueue.is_err());
    assert_eq!(store.draft(&account, &draft.id).unwrap(), draft);
    assert_eq!(
        fs::read_dir(path.join("objects")).unwrap().count(),
        object_count
    );
    assert!(store.task_ids(&account).unwrap().is_empty());
    let id = store.enqueue_draft(&account, &draft.id, 1).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
    let attempt = store.begin_smtp(&account, &id);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(attempt.is_err());
    assert_eq!(store.task(&account, &id).unwrap().state, SendState::Pending);
}

#[test]
fn v1_upgrade_backs_up_and_preserves_accounts_and_failed_upgrade_rolls_back() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("qingmail");
    let directory = PrivateDir::open(&path).unwrap();
    let _file = directory.file("state.sqlite3", false).unwrap();
    let mut conn = Connection::open(directory.path("state.sqlite3")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute("INSERT INTO accounts VALUES ('saved','旧账号','x@example.org','imap.example.org',993,'x','tls','smtp.example.org',465,'x','tls')",[]).unwrap();
    assert!(
        migrate(
            &mut conn,
            &directory,
            true,
            SCHEMA,
            "CREATE TABLE temporary_upgrade(id TEXT); INVALID SQL;",
            SCHEMA_V3
        )
        .is_err()
    );
    assert_eq!(
        conn.query_row::<u32, _, _>("PRAGMA user_version", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row::<u32, _, _>(
            "SELECT count(*) FROM sqlite_schema WHERE name='temporary_upgrade'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    drop(conn);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .connection
            .query_row::<u32, _, _>("PRAGMA user_version", [], |r| r.get(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        store
            .account(&AccountId("saved".into()))
            .unwrap()
            .unwrap()
            .display_name,
        "旧账号"
    );
    let backups: Vec<_> = fs::read_dir(&path)
        .unwrap()
        .map(|x| x.unwrap())
        .filter(|x| x.file_name().to_string_lossy().starts_with("migration-v1-"))
        .collect();
    assert_eq!(backups.len(), 2);
    for backup in backups {
        let backup = Connection::open(backup.path()).unwrap();
        assert_eq!(
            backup
                .query_row::<u32, _, _>("PRAGMA user_version", [], |r| r.get(0))
                .unwrap(),
            1
        );
        assert_eq!(
            backup
                .query_row::<String, _, _>(
                    "SELECT display_name FROM accounts WHERE id='saved'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            "旧账号"
        );
    }
    // 确保迁移常量不是空占位。
    assert!(SCHEMA_V2.contains("send_tasks"));
}

#[test]
fn process_exit_after_smtp_acceptance_can_only_resume_copy() {
    const KEY: &str = "QINGMAIL_OUTBOX_CRASH_TEST";
    if let Some(path) = std::env::var_os(KEY) {
        let mut store = Store::open(std::path::Path::new(&path)).unwrap();
        let account = store.accounts().unwrap()[0].0.clone();
        let id = task(&mut store, &account);
        let attempt = store.begin_smtp(&account, &id).unwrap();
        store.finish_smtp(attempt, SmtpOutcome::Accepted).unwrap();
        std::process::exit(74);
    }
    let (root, store, account) = setup();
    drop(store);
    let path = root.path().join("qingmail");
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "outbox::tests::process_exit_after_smtp_acceptance_can_only_resume_copy",
        ])
        .env(KEY, &path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(74));
    let mut store = Store::open(&path).unwrap();
    let ids = store.task_ids(&account).unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(
        store.task(&account, &ids[0]).unwrap().state,
        SendState::CopyPending
    );
    assert!(store.begin_smtp(&account, &ids[0]).is_err());
    assert!(
        store
            .resend_task(&account, &ids[0], ResendAuthorization::UserConfirmed)
            .is_err()
    );
    assert!(store.begin_copy(&account, &ids[0]).is_ok());
}

#[test]
fn cancel_and_stop_do_not_create_submissions_or_claim_recall() {
    let (_root, mut store, account) = setup();
    let cancelled = task(&mut store, &account);
    store.cancel_pending(&account, &cancelled).unwrap();
    assert_eq!(
        store.task(&account, &cancelled).unwrap().state,
        SendState::Failed
    );
    assert!(store.begin_smtp(&account, &cancelled).is_err());
    let uncertain = task(&mut store, &account);
    let attempt = store.begin_smtp(&account, &uncertain).unwrap();
    assert!(store.cancel_pending(&account, &uncertain).is_err());
    assert!(
        store
            .finish_copy(
                &account,
                &uncertain,
                CopyOutcome::Saved {
                    folder: "Sent".into(),
                    uidvalidity: 42,
                    uid: Some(7),
                    existed: false,
                },
            )
            .is_err()
    );
    assert!(store.begin_copy(&account, &uncertain).is_err());
    assert!(
        store
            .resend_task(&account, &uncertain, ResendAuthorization::UserConfirmed)
            .is_err()
    );
    store.finish_smtp(attempt, SmtpOutcome::Uncertain).unwrap();
    store.stop_uncertain(&account, &uncertain).unwrap();
    assert_eq!(
        store.task(&account, &uncertain).unwrap().state,
        SendState::Uncertain
    );
    assert_eq!(store.task_ids(&account).unwrap().len(), 2);
    assert!(store.begin_smtp(&account, &uncertain).is_err());
}

#[test]
fn oversized_or_interrupted_import_keeps_no_partial_object() {
    let (root, mut store, account) = setup();
    let dir = root.path().join("qingmail/objects");
    let mut oversized = std::io::repeat(1).take(MAX_ATTACHMENT_BYTES + 1);
    assert!(
        store
            .import_attachment(
                &account,
                "large.bin",
                "application/octet-stream",
                &mut oversized
            )
            .is_err()
    );
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    struct Failing;
    impl Read for Failing {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("synthetic read failure"))
        }
    }
    assert!(
        store
            .import_attachment(
                &account,
                "broken.bin",
                "application/octet-stream",
                &mut Failing
            )
            .is_err()
    );
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
}

#[test]
fn corrupt_body_is_preserved_and_not_submitted() {
    let (root, mut store, account) = setup();
    let draft = store.create_draft(&account, &document("original")).unwrap();
    let id: String = store
        .connection
        .query_row(
            "SELECT body_id FROM drafts WHERE id=?1",
            [&draft.id.0],
            |r| r.get(0),
        )
        .unwrap();
    let path = root.path().join("qingmail/objects").join(&id);
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 1;
    fs::write(&path, &bytes).unwrap();
    assert!(store.draft(&account, &draft.id).is_err());
    assert!(store.enqueue_draft(&account, &draft.id, 1).is_err());
    assert!(store.task_ids(&account).unwrap().is_empty());
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn process_exit_during_smtp_recovers_uncertain_and_rejects_submit() {
    const KEY: &str = "QINGMAIL_OUTBOX_SENDING_CRASH";
    if let Some(path) = std::env::var_os(KEY) {
        let mut store = Store::open(std::path::Path::new(&path)).unwrap();
        let account = store.accounts().unwrap()[0].0.clone();
        let id = task(&mut store, &account);
        let _permit = store.begin_smtp(&account, &id).unwrap();
        std::process::exit(75);
    }
    let (root, store, account) = setup();
    drop(store);
    let path = root.path().join("qingmail");
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "outbox::tests::process_exit_during_smtp_recovers_uncertain_and_rejects_submit",
        ])
        .env(KEY, &path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(75));
    let mut store = Store::open(&path).unwrap();
    let ids = store.task_ids(&account).unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(
        store.task(&account, &ids[0]).unwrap().state,
        SendState::Uncertain
    );
    assert!(store.begin_smtp(&account, &ids[0]).is_err());
}

#[test]
fn local_workspace_is_not_remote_account_and_cannot_enqueue() {
    let (root, mut store, remote) = setup();
    let local = store.local_draft_account().unwrap();
    assert_eq!(store.local_draft_account().unwrap(), local);
    assert!(store.account(&local).unwrap().is_none());
    assert_eq!(store.accounts().unwrap().len(), 1);
    let draft = store.create_draft(&local, &document("本地草稿")).unwrap();
    store.select_local_draft(&local, &draft.id).unwrap();
    assert!(store.select_local_draft(&remote, &draft.id).is_err());
    assert!(store.enqueue_draft(&local, &draft.id, 1).is_err());
    assert!(store.task_ids(&local).unwrap().is_empty());
    drop(store);
    let store = Store::open(&root.path().join("qingmail")).unwrap();
    assert_eq!(
        store.selected_local_draft(&local).unwrap(),
        Some(draft.id.clone())
    );
    assert_eq!(store.draft(&local, &draft.id).unwrap(), draft);
}

#[test]
fn v2_upgrade_keeps_existing_drafts_and_remote_account_identity() {
    let (root, mut store, account) = setup();
    let draft = store
        .create_draft(&account, &document("v2 草稿🙂"))
        .unwrap();
    let path = root.path().join("qingmail");
    drop(store);
    // 从当前库移除 v3/v4 的结构，恢复真实 v2 布局；正文和 v2 行保持原样。
    let conn = Connection::open(path.join("state.sqlite3")).unwrap();
    conn.execute_batch("DROP TABLE server_draft_sources; DROP TABLE server_draft_folders; DROP TABLE part_resources; DROP TABLE message_structures; DROP VIEW visible_folder_messages; DROP TABLE mail_operations; DROP TRIGGER account_default_options; DROP TABLE account_options; DROP TRIGGER account_legacy_credential; DROP TABLE account_credentials; DROP TABLE mutation_results; DROP TABLE message_bodies; DROP TABLE remote_folders; DROP TABLE folder_sync; DROP TABLE catalog_sync; DROP TABLE local_editor; DROP INDEX one_local_workspace; ALTER TABLE accounts DROP COLUMN kind; ALTER TABLE send_tasks DROP COLUMN copy_folder; ALTER TABLE send_tasks DROP COLUMN copy_uidvalidity; ALTER TABLE send_tasks DROP COLUMN copy_uid; ALTER TABLE folder_messages DROP COLUMN seen; ALTER TABLE folder_messages DROP COLUMN flagged; ALTER TABLE messages DROP COLUMN has_attachments; PRAGMA user_version=2;").unwrap();
    drop(conn);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.draft(&account, &draft.id).unwrap(), draft);
    assert_eq!(store.accounts().unwrap().len(), 1);
    let local = store.local_draft_account().unwrap();
    assert_ne!(local, account);
    assert_eq!(store.accounts().unwrap().len(), 1);
    assert!(fs::read_dir(&path).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("migration-v2-")
    }));
}

#[test]
fn copy_is_confirmed_before_sent_and_failures_keep_the_task_retryable() {
    let (_root, mut store, account) = setup();
    let id = task(&mut store, &account);
    let attempt = store.begin_smtp(&account, &id).unwrap();
    store.finish_smtp(attempt, SmtpOutcome::Accepted).unwrap();
    assert_eq!(
        store.task(&account, &id).unwrap().state,
        SendState::CopyPending
    );
    let ready = store.copy_ready(&account, &id).unwrap();
    assert_eq!(ready.imap, settings().imap);
    assert!(ready.folder.is_none());
    assert_eq!(store.copy_pending_tasks(&account).unwrap().len(), 1);

    for code in [
        "copy_no_sent_folder",
        "copy_append_rejected",
        "copy_result_unknown",
    ] {
        let outcome = if code == "copy_result_unknown" {
            CopyOutcome::Uncertain { code }
        } else {
            CopyOutcome::Failed { code }
        };
        store.finish_copy(&account, &id, outcome).unwrap();
        let task = store.task(&account, &id).unwrap();
        assert_eq!(task.state, SendState::CopyPending);
        assert_eq!(task.detail, code);
        assert!(task.copy.is_none());
        assert!(store.begin_smtp(&account, &id).is_err());
        assert!(
            store
                .resend_task(&account, &id, ResendAuthorization::UserConfirmed)
                .is_err()
        );
    }
    assert!(
        store
            .finish_copy(
                &account,
                &id,
                CopyOutcome::Failed {
                    code: "server said: password rejected",
                },
            )
            .is_err(),
        "only fixed diagnostic codes may be stored"
    );

    // 查重命中也只写副本位置，不产生第二个 SMTP 许可。
    store
        .finish_copy(
            &account,
            &id,
            CopyOutcome::Saved {
                folder: "Sent Items".into(),
                uidvalidity: 42,
                uid: Some(9),
                existed: true,
            },
        )
        .unwrap();
    let task = store.task(&account, &id).unwrap();
    assert_eq!(task.state, SendState::Sent);
    assert_eq!(task.detail, "copy_already_present");
    assert_eq!(
        task.copy,
        Some(SentCopy {
            folder: "Sent Items".into(),
            uidvalidity: 42,
            uid: Some(9),
        })
    );
    assert!(store.copy_pending_tasks(&account).unwrap().is_empty());
    assert!(store.copy_ready(&account, &id).is_err());
    assert!(store.begin_copy(&account, &id).is_err());

    // 副本保存成功后，编辑草稿仍不能再次提交同一版本。
    assert!(store.begin_smtp(&account, &id).is_err());
}

#[test]
fn v5_upgrade_adds_copy_columns_and_preserves_copy_pending_tasks() {
    let (root, mut store, account) = setup();
    let id = task(&mut store, &account);
    let attempt = store.begin_smtp(&account, &id).unwrap();
    store.finish_smtp(attempt, SmtpOutcome::Accepted).unwrap();
    let path = root.path().join("qingmail");
    drop(store);
    let conn = Connection::open(path.join("state.sqlite3")).unwrap();
    conn.execute_batch(
        "DROP TABLE server_draft_sources; DROP TABLE server_draft_folders; DROP TABLE part_resources; DROP TABLE message_structures; DROP VIEW visible_folder_messages; DROP TABLE mail_operations; DROP TRIGGER account_default_options; DROP TABLE account_options; DROP TRIGGER account_legacy_credential; DROP TABLE account_credentials; DROP TABLE mutation_results; ALTER TABLE send_tasks DROP COLUMN copy_folder; \
         ALTER TABLE send_tasks DROP COLUMN copy_uidvalidity; \
         ALTER TABLE send_tasks DROP COLUMN copy_uid; \
         ALTER TABLE folder_messages DROP COLUMN seen; ALTER TABLE folder_messages DROP COLUMN flagged; ALTER TABLE messages DROP COLUMN has_attachments; PRAGMA user_version=5;",
    )
    .unwrap();
    drop(conn);
    let store = Store::open(&path).unwrap();
    let columns = store
        .connection
        .prepare("PRAGMA table_info(send_tasks)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    for name in ["copy_folder", "copy_uidvalidity", "copy_uid"] {
        assert!(columns.iter().any(|column| column == name), "{name}");
    }
    let task = store.task(&account, &id).unwrap();
    assert_eq!(task.state, SendState::CopyPending);
    assert!(task.copy.is_none());
    assert!(store.begin_copy(&account, &id).is_ok());
    assert!(fs::read_dir(&path).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("migration-v5-")
    }));
}

#[test]
fn task_list_exposes_frozen_headers_for_the_task_entry() {
    let (_root, mut store, account) = setup();
    let id = task(&mut store, &account);
    let list = store.task_list(&account).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].recipients.subject, "主题🙂");
    assert_eq!(list[0].sender, settings().address);
    assert_eq!(list[0].recipients.to, vec!["to@example.org".to_string()]);
    assert_eq!(list[0].recipients.cc, vec!["cc@example.org".to_string()]);
    assert!(list[0].created_at > 0);
    assert_eq!(list[0].state, SendState::Pending);
}
