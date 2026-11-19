use super::*;
use crate::storage::StorageWorker;
use mail_content::draft::Draft;
use mail_core::{AccountSettings, ConnectionSecurity, SendState, ServerSettings, SmtpOutcome};
use mail_service::credentials::{Credentials, VaultError};
fn setup() -> (tempfile::TempDir, Store, StoredDraft) {
    let root = tempfile::tempdir().unwrap();
    let mut store = Store::open(&root.path().join("data")).unwrap();
    let server = ServerSettings {
        authentication: Default::default(),
        host: "example.invalid".into(),
        port: 993,
        username: "synthetic".into(),
        security: ConnectionSecurity::Tls,
    };
    let mut preferences = store.desktop_preferences().unwrap();
    preferences.save_sent_copy = true;
    store.update_desktop_preferences(&preferences).unwrap();
    let account = store
        .create_account(&AccountSettings {
            display_name: "synthetic".into(),
            address: "sender@example.invalid".into(),
            imap: server.clone(),
            smtp: server,
        })
        .unwrap();
    let draft = store
        .create_draft(
            &account,
            &mail_storage::DraftDocument {
                headers: DraftHeaders {
                    subject: "主题".into(),
                    to: vec!["to@example.invalid".into()],
                    ..Default::default()
                },
                body: Draft::plain("正文🙂").unwrap(),
                attachments: vec![],
            },
        )
        .unwrap();
    (root, store, draft)
}
struct Vault(bool);
impl CredentialVault for Vault {
    fn status(&self) -> Result<(), VaultError> {
        unreachable!()
    }
    fn save(&self, _: &AccountId, _: &Credentials) -> Result<(), VaultError> {
        unreachable!()
    }
    fn delete(&self, _: &AccountId) -> Result<(), VaultError> {
        unreachable!()
    }
    fn load(&self, _: &AccountId) -> Result<Credentials, VaultError> {
        if self.0 {
            Credentials::new("synthetic".into(), "synthetic".into())
        } else {
            Err(VaultError::Missing)
        }
    }
}
#[test]
fn review_does_not_create_task_and_stale_confirmation_is_rejected() {
    let (_root, mut store, draft) = setup();
    let preview = review(&store, &draft).unwrap();
    assert_eq!(preview.sender, "sender@example.invalid");
    assert!(
        store
            .draft_tasks(&draft.account, &draft.id)
            .unwrap()
            .is_empty()
    );
    let mut changed = draft.document.clone();
    changed.headers.subject = "changed".into();
    store
        .save_draft(&draft.account, &draft.id, draft.revision, &changed)
        .unwrap();
    assert!(begin(&mut store, preview, false).is_err());
    assert!(review(&store, &draft).is_err());
    assert!(store.task_ids(&draft.account).unwrap().is_empty());
}
#[test]
fn empty_subject_confirmation_and_duplicate_versions_do_not_create_second_permission() {
    let (_root, mut store, draft) = setup();
    let mut changed = draft.document.clone();
    changed.headers.subject.clear();
    let draft = store
        .save_draft(&draft.account, &draft.id, draft.revision, &changed)
        .unwrap();
    let preview = review(&store, &draft).unwrap();
    assert!(begin(&mut store, preview, false).is_err());
    assert!(store.task_ids(&draft.account).unwrap().is_empty());
    let first = review(&store, &draft).unwrap();
    let duplicate = review(&store, &draft).unwrap();
    let ready = begin(&mut store, first, true).unwrap();
    assert!(begin(&mut store, duplicate, true).is_err());
    store
        .finish_smtp(ready.attempt, SmtpOutcome::Uncertain, None)
        .unwrap();
    let changed = store
        .save_draft(&draft.account, &draft.id, draft.revision, &changed)
        .unwrap();
    assert!(
        review(&store, &changed).is_err(),
        "editing must not bypass duplicate-send protection"
    );
    assert_eq!(store.task_ids(&draft.account).unwrap().len(), 1);
}
#[test]
fn missing_credential_creates_no_task_and_outcomes_persist_without_retry() {
    for outcome in [
        SmtpOutcome::Accepted,
        SmtpOutcome::DefinitelyRejected,
        SmtpOutcome::Uncertain,
    ] {
        let (root, store, draft) = setup();
        drop(store);
        let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
        let client = worker.client();
        let preview = receive(client.review_send(draft.clone())).unwrap();
        assert!(
            dispatch(
                client.clone(),
                preview,
                false,
                &Vault(false),
                |_, _, _| panic!("no SMTP without credentials"),
                |_, _, _, _, _| panic!("no copy without credentials"),
            )
            .is_err()
        );
        assert!(
            receive(client.draft_tasks(draft.account.clone(), draft.id.clone()))
                .unwrap()
                .is_empty()
        );
        let preview = receive(client.review_send(draft.clone())).unwrap();
        let task = dispatch(
            client.clone(),
            preview,
            false,
            &Vault(true),
            |server, password, message| {
                assert_eq!(server.host, "example.invalid");
                assert_eq!(password, "synthetic");
                assert!(!message.raw().is_empty());
                outgoing::Delivery {
                    outcome,
                    detail: outgoing::DeliveryDetail::ResultUnknown,
                }
            },
            |server, password, message_id, raw, preferred| {
                assert!(matches!(outcome, SmtpOutcome::Accepted));
                assert_eq!(server.host, "example.invalid");
                assert_eq!(password, "synthetic");
                assert!(message_id.starts_with('<'));
                assert!(!raw.is_empty());
                assert!(preferred.is_none());
                sent::CopyResult::Saved(sent::CopiedMessage {
                    folder: "Sent".into(),
                    uidvalidity: 42,
                    uid: Some(7),
                    existed: false,
                })
            },
        )
        .unwrap();
        // SMTP 接受后才保存副本；其余结果不触碰 IMAP。
        let expected = match outcome {
            SmtpOutcome::Accepted => SendState::Sent,
            SmtpOutcome::DefinitelyRejected => SendState::Failed,
            SmtpOutcome::Uncertain => SendState::Uncertain,
        };
        assert_eq!(task.state, expected);
        if matches!(outcome, SmtpOutcome::Accepted) {
            assert_eq!(
                task.copy,
                Some(mail_storage::SentCopy {
                    folder: "Sent".into(),
                    uidvalidity: 42,
                    uid: Some(7),
                })
            );
        }
        worker.stop();
        let store = Store::open(&root.path().join("data")).unwrap();
        assert_eq!(
            store.task(&draft.account, &task.id).unwrap().state,
            expected
        );
        assert!(review(&store, &draft).is_err());
    }
}
#[test]
fn coordinator_rejects_concurrent_clicks_and_survives_view_receiver_drop() {
    let (root, store, draft) = setup();
    drop(store);
    let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
    let client = worker.client();
    let preview = receive(client.review_send(draft.clone())).unwrap();
    let coordinator = SendCoordinator::default();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let reply = coordinator
        .start(move || {
            let result = dispatch(
                client,
                preview,
                false,
                &Vault(true),
                |_, _, _| {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .unwrap();
                    outgoing::Delivery {
                        outcome: SmtpOutcome::Accepted,
                        detail: outgoing::DeliveryDetail::Accepted,
                    }
                },
                |_, _, _, _, _| {
                    sent::CopyResult::Saved(sent::CopiedMessage {
                        folder: "Sent".into(),
                        uidvalidity: 42,
                        uid: Some(7),
                        existed: false,
                    })
                },
            );
            done_tx.send(()).unwrap();
            result
        })
        .unwrap();
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    assert!(coordinator.active());
    assert!(
        coordinator
            .start(|| panic!("concurrent click must not start"))
            .is_err()
    );
    drop(reply);
    release_tx.send(()).unwrap();
    done_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    let tasks = receive(worker.client().draft_tasks(draft.account, draft.id)).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, SendState::Sent);
    worker.stop();
}

#[test]
fn changed_attachment_after_review_never_creates_send_task() {
    let (root, mut store, draft) = setup();
    let info = store
        .import_attachment(
            &draft.account,
            "fixture.bin",
            "application/octet-stream",
            &mut b"original".as_slice(),
        )
        .unwrap();
    let mut document = draft.document.clone();
    document.attachments.push(info.id.clone());
    let draft = store
        .save_draft(&draft.account, &draft.id, draft.revision, &document)
        .unwrap();
    let preview = review(&store, &draft).unwrap();
    std::fs::write(root.path().join("data/objects").join(info.id.0), b"changed").unwrap();
    assert!(begin(&mut store, preview, false).is_err());
    assert!(store.task_ids(&draft.account).unwrap().is_empty());
}

#[test]
fn copy_failure_keeps_task_pending_and_retry_only_saves_the_copy() {
    let (root, store, draft) = setup();
    drop(store);
    let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
    let client = worker.client();
    let preview = receive(client.review_send(draft.clone())).unwrap();
    let mut smtp_calls = 0;
    let task = dispatch(
        client.clone(),
        preview,
        false,
        &Vault(true),
        |_, _, _| {
            smtp_calls += 1;
            outgoing::Delivery {
                outcome: SmtpOutcome::Accepted,
                detail: outgoing::DeliveryDetail::Accepted,
            }
        },
        |_, _, _, _, _| sent::CopyResult::Failed(sent::CopyError::Rejected),
    )
    .unwrap();
    assert_eq!(smtp_calls, 1);
    assert_eq!(task.state, SendState::CopyPending);
    assert_eq!(task.detail, "copy_append_rejected");
    assert!(task.copy.is_none());
    // 重试只提交副本：不经过 SMTP，也不创建第二个任务。
    let saved = retry_copy_once(
        client.clone(),
        &Vault(true),
        draft.account.clone(),
        task.id.clone(),
        |server, password, message_id, raw, preferred| {
            assert_eq!(server.host, "example.invalid");
            assert_eq!(password, "synthetic");
            assert!(message_id.starts_with('<'));
            assert!(!raw.is_empty());
            assert!(preferred.is_none());
            sent::CopyResult::Saved(sent::CopiedMessage {
                folder: "Sent".into(),
                uidvalidity: 42,
                uid: Some(7),
                existed: false,
            })
        },
    )
    .unwrap();
    assert_eq!(saved.state, SendState::Sent);
    assert_eq!(
        saved.copy,
        Some(mail_storage::SentCopy {
            folder: "Sent".into(),
            uidvalidity: 42,
            uid: Some(7),
        })
    );
    assert_eq!(smtp_calls, 1);
    assert_eq!(
        receive(client.draft_tasks(draft.account, draft.id))
            .unwrap()
            .len(),
        1
    );
    worker.stop();
}

#[test]
fn uncertain_copy_is_retried_by_finding_the_existing_copy() {
    let (root, store, draft) = setup();
    drop(store);
    let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
    let client = worker.client();
    let preview = receive(client.review_send(draft.clone())).unwrap();
    let task = dispatch(
        client.clone(),
        preview,
        false,
        &Vault(true),
        |_, _, _| outgoing::Delivery {
            outcome: SmtpOutcome::Accepted,
            detail: outgoing::DeliveryDetail::Accepted,
        },
        |_, _, _, _, _| sent::CopyResult::Failed(sent::CopyError::ResultUnknown),
    )
    .unwrap();
    assert_eq!(task.state, SendState::CopyPending);
    assert_eq!(task.detail, "copy_result_unknown");
    let saved = retry_copy_once(
        client.clone(),
        &Vault(true),
        draft.account.clone(),
        task.id.clone(),
        |_, _, _, _, _| {
            sent::CopyResult::Saved(sent::CopiedMessage {
                folder: "Sent".into(),
                uidvalidity: 42,
                uid: Some(9),
                existed: true,
            })
        },
    )
    .unwrap();
    assert_eq!(saved.state, SendState::Sent);
    assert_eq!(saved.detail, "copy_already_present");
    worker.stop();
}

#[test]
fn retry_copy_rejects_tasks_that_already_have_a_saved_copy() {
    let (root, store, draft) = setup();
    drop(store);
    let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
    let client = worker.client();
    let preview = receive(client.review_send(draft.clone())).unwrap();
    let task = dispatch(
        client.clone(),
        preview,
        false,
        &Vault(true),
        |_, _, _| outgoing::Delivery {
            outcome: SmtpOutcome::Accepted,
            detail: outgoing::DeliveryDetail::Accepted,
        },
        |_, _, _, _, _| {
            sent::CopyResult::Saved(sent::CopiedMessage {
                folder: "Sent".into(),
                uidvalidity: 42,
                uid: None,
                existed: false,
            })
        },
    )
    .unwrap();
    assert_eq!(task.state, SendState::Sent);
    assert!(
        retry_copy_once(
            client,
            &Vault(true),
            draft.account,
            task.id,
            |_, _, _, _, _| panic!("a saved copy must not be appended again"),
        )
        .is_err()
    );
    worker.stop();
}

#[test]
fn failed_history_allows_editing_but_blocks_same_version_and_uncertain() {
    let (_root, mut store, draft) = setup();
    let preview = review(&store, &draft).unwrap();
    let ready = begin(&mut store, preview, false).unwrap();
    store
        .finish_smtp(ready.attempt, SmtpOutcome::DefinitelyRejected, None)
        .unwrap();
    // 未修改的同一版本仍被拒绝，失败任务只能通过任务入口重发。
    assert!(review(&store, &draft).is_err());
    let mut changed = draft.document.clone();
    changed.body = Draft::plain("修改后的正文").unwrap();
    let changed = store
        .save_draft(&draft.account, &draft.id, draft.revision, &changed)
        .unwrap();
    let preview = review(&store, &changed).unwrap();
    let ready = begin(&mut store, preview, false).unwrap();
    store
        .finish_smtp(ready.attempt, SmtpOutcome::Uncertain, None)
        .unwrap();
    // 结果不确定后，即使再改版本也不能绕过原任务。
    let mut again = changed.document.clone();
    again.body = Draft::plain("再改一次").unwrap();
    let again = store
        .save_draft(&changed.account, &changed.id, changed.revision, &again)
        .unwrap();
    assert!(review(&store, &again).is_err());
    assert_eq!(store.task_list(&draft.account).unwrap().len(), 2);
}

#[test]
fn resend_keeps_the_old_task_and_submits_once_with_a_new_identity() {
    let (root, store, draft) = setup();
    drop(store);
    let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
    let client = worker.client();
    let preview = receive(client.review_send(draft.clone())).unwrap();
    let first = dispatch(
        client.clone(),
        preview,
        false,
        &Vault(true),
        |_, _, _| outgoing::Delivery {
            outcome: SmtpOutcome::DefinitelyRejected,
            detail: outgoing::DeliveryDetail::DataRejected,
        },
        |_, _, _, _, _| panic!("a rejected submission has no copy"),
    )
    .unwrap();
    assert_eq!(first.state, SendState::Failed);
    let mut smtp_calls = 0;
    let resent = resend_once(
        client.clone(),
        &Vault(true),
        draft.account.clone(),
        first.id.clone(),
        |server, password, message| {
            smtp_calls += 1;
            assert_eq!(server.host, "example.invalid");
            assert_eq!(password, "synthetic");
            assert!(String::from_utf8_lossy(message.raw()).contains("Subject:"));
            outgoing::Delivery {
                outcome: SmtpOutcome::Accepted,
                detail: outgoing::DeliveryDetail::Accepted,
            }
        },
        |_, _, message_id, raw, preferred| {
            assert!(message_id.starts_with('<'));
            assert!(!raw.is_empty());
            assert!(preferred.is_none());
            sent::CopyResult::Saved(sent::CopiedMessage {
                folder: "Sent".into(),
                uidvalidity: 42,
                uid: Some(7),
                existed: false,
            })
        },
    )
    .unwrap();
    assert_eq!(smtp_calls, 1);
    assert_eq!(resent.state, SendState::Sent);
    assert_eq!(resent.resend_of, Some(first.id.clone()));
    assert_ne!(resent.id, first.id);
    // 旧记录保留；同一旧任务再次重发不会获得第二个提交。
    let tasks = receive(client.tasks(draft.account.clone())).unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(
        tasks.iter().find(|t| t.id == first.id).unwrap().state,
        SendState::Failed
    );
    assert!(
        resend_once(
            client.clone(),
            &Vault(true),
            draft.account.clone(),
            first.id.clone(),
            |_, _, _| panic!("must not submit twice"),
            |_, _, _, _, _| panic!("must not copy twice"),
        )
        .is_err()
    );
    assert_eq!(smtp_calls, 1);
    worker.stop();
}

#[test]
fn resend_without_credentials_creates_no_new_task() {
    let (root, store, draft) = setup();
    drop(store);
    let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
    let client = worker.client();
    let preview = receive(client.review_send(draft.clone())).unwrap();
    let first = dispatch(
        client.clone(),
        preview,
        false,
        &Vault(true),
        |_, _, _| outgoing::Delivery {
            outcome: SmtpOutcome::Uncertain,
            detail: outgoing::DeliveryDetail::ResultUnknown,
        },
        |_, _, _, _, _| panic!("no copy after uncertain submission"),
    )
    .unwrap();
    assert_eq!(first.state, SendState::Uncertain);
    assert!(
        resend_once(
            client.clone(),
            &Vault(false),
            draft.account.clone(),
            first.id.clone(),
            |_, _, _| panic!("no SMTP without credentials"),
            |_, _, _, _, _| panic!("no copy without credentials"),
        )
        .is_err()
    );
    assert_eq!(receive(client.tasks(draft.account)).unwrap().len(), 1);
    worker.stop();
}

#[test]
fn disabled_sent_copy_does_not_contact_imap_after_smtp_acceptance() {
    let (root, mut store, draft) = setup();
    let mut p = store.desktop_preferences().unwrap();
    p.save_sent_copy = false;
    store.update_desktop_preferences(&p).unwrap();
    drop(store);
    let mut worker = StorageWorker::start(Some(root.path().join("data"))).unwrap();
    let client = worker.client();
    let preview = receive(client.review_send(draft.clone())).unwrap();
    let mut smtp_calls = 0;
    let task = dispatch(
        client.clone(),
        preview,
        false,
        &Vault(true),
        |_, _, _| {
            smtp_calls += 1;
            outgoing::Delivery {
                outcome: SmtpOutcome::Accepted,
                detail: outgoing::DeliveryDetail::Accepted,
            }
        },
        |_, _, _, _, _| panic!("disabled copy must never call IMAP APPEND or its lookup"),
    )
    .unwrap();
    assert_eq!(smtp_calls, 1);
    assert_eq!(task.state, SendState::Sent);
    assert!(task.copy.is_none());
    assert_eq!(
        receive(client.draft_tasks(draft.account, draft.id))
            .unwrap()
            .len(),
        1
    );
    worker.stop();
}
