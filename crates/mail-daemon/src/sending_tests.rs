use super::*;
use crate::storage::StorageWorker;
use mail_content::draft::Draft;
use mail_core::{AccountSettings, ConnectionSecurity, SendState, ServerSettings, SmtpOutcome};
use mail_service::credentials::{Credentials, VaultError};
fn setup() -> (tempfile::TempDir, Store, StoredDraft) {
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
        .finish_smtp(ready.attempt, SmtpOutcome::Uncertain)
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
                |_, _, _| panic!("no SMTP without credentials")
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
        )
        .unwrap();
        let expected = match outcome {
            SmtpOutcome::Accepted => SendState::CopyPending,
            SmtpOutcome::DefinitelyRejected => SendState::Failed,
            SmtpOutcome::Uncertain => SendState::Uncertain,
        };
        assert_eq!(task.state, expected);
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
            let result = dispatch(client, preview, false, &Vault(true), |_, _, _| {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
                outgoing::Delivery {
                    outcome: SmtpOutcome::Accepted,
                    detail: outgoing::DeliveryDetail::Accepted,
                }
            });
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
    assert_eq!(tasks[0].state, SendState::CopyPending);
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
