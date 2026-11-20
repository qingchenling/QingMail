//! 仅显式隔离目录的合成验收数据，不访问系统密钥库或网络。
use mail_core::{
    AccountSettings, ConnectionSecurity, FolderBatch, MessageEnvelope, RemoteFolder, ServerSettings,
};
use mail_storage::Store;
fn main() {
    let path = std::env::args_os()
        .nth(1)
        .expect("explicit isolated directory required");
    let mut store = Store::open(std::path::Path::new(&path)).unwrap();
    assert!(
        store.accounts().unwrap().is_empty(),
        "fixture requires empty account directory"
    );
    let server = ServerSettings {
        authentication: Default::default(),
        host: "example.invalid".into(),
        port: 993,
        username: "synthetic-only".into(),
        security: ConnectionSecurity::Tls,
    };
    let account = store
        .create_account(&AccountSettings {
            display_name: "本地合成账号（无凭据）".into(),
            address: "fixture@example.invalid".into(),
            imap: server.clone(),
            smtp: server,
        })
        .unwrap();
    let mut folders = vec![
        RemoteFolder {
            name: "INBOX".into(),
            display_name: "INBOX".into(),
            delimiter: Some("/".into()),
            selectable: true,
        },
        RemoteFolder {
            name: "Projects".into(),
            display_name: "Projects".into(),
            delimiter: Some("/".into()),
            selectable: false,
        },
        RemoteFolder {
            name: "Projects/2026".into(),
            display_name: "Projects/2026".into(),
            delimiter: Some("/".into()),
            selectable: true,
        },
        RemoteFolder {
            name: "&g0l6P3ux-".into(),
            display_name: "草稿箱".into(),
            delimiter: Some("/".into()),
            selectable: true,
        },
    ];
    let sent_confirmation = std::env::args().any(|arg| arg == "--sent-confirmation");
    if sent_confirmation {
        folders.push(RemoteFolder {
            name: "Sent".into(),
            display_name: "Sent".into(),
            delimiter: Some("/".into()),
            selectable: true,
        });
    }
    store
        .replace_remote_folders(&account, &folders, 1794816000)
        .unwrap();
    for range in [1..=40, 41..=240] {
        let messages = range
            .map(|uid| {
                (
                    uid,
                    MessageEnvelope {
                        subject: format!("合成主题 {uid}"),
                        sender: "测试 <fixture@example.invalid>".into(),
                        received_at: 1794816000,
                    },
                )
            })
            .collect();
        store
            .apply_folder_batch(
                &account,
                &FolderBatch {
                    remote_name: "INBOX".into(),
                    uidvalidity: 42,
                    since: 1787040000,
                    has_more: true,
                    complete: true,
                    kind: mail_core::BatchKind::Recent,
                    flags: Default::default(),
                    messages,
                },
                1794816000,
            )
            .unwrap();
    }
    store
        .apply_folder_batch(
            &account,
            &FolderBatch {
                remote_name: "Projects/2026".into(),
                uidvalidity: 43,
                since: 1787040000,
                has_more: false,
                complete: true,
                kind: mail_core::BatchKind::Recent,
                flags: Default::default(),
                messages: vec![],
            },
            1794816000,
        )
        .unwrap();
    if std::env::args().any(|arg| arg == "--bodies") {
        let cache = store
            .folder_cache(&account, "INBOX", None)
            .unwrap()
            .unwrap();
        let samples: &[&[u8]] = &[
            include_bytes!("../../../tests/fixtures/mime/03-alternative-related.eml"),
            include_bytes!("../../../tests/fixtures/mime/04-adversarial.eml"),
            include_bytes!("../../../tests/fixtures/mime/06-broken.eml"),
        ];
        for (message, raw) in cache.messages.iter().zip(samples) {
            store.save_message_body(&message.location, raw).unwrap();
        }
    }
    if sent_confirmation {
        seed_sent_confirmation(&mut store, &account);
    }
    println!("合成目录与240封头部索引已写入隔离目录；未创建系统凭据或访问网络");
}

fn seed_sent_confirmation(store: &mut Store, account: &mail_core::AccountId) {
    use mail_core::SmtpOutcome;
    use mail_storage::{CopyOutcome, DraftDocument};
    for (name, save_copy) in [
        ("已保存副本", true),
        ("仅本地记录", false),
        ("副本待保存", true),
    ] {
        let mut preferences = store.desktop_preferences().unwrap();
        preferences.save_sent_copy = save_copy;
        store.update_desktop_preferences(&preferences).unwrap();
        let draft = store
            .create_draft(
                account,
                &DraftDocument {
                    headers: mail_core::DraftHeaders {
                        subject: format!("发送确认测试 · {name}"),
                        to: vec!["recipient@example.invalid".into()],
                        ..Default::default()
                    },
                    body: mail_content::draft::Draft::plain(
                        &"陈雨，你好：\n\n评审资料已整理好。\n".repeat(80),
                    )
                    .unwrap(),
                    attachments: vec![],
                },
            )
            .unwrap();
        let id = store
            .enqueue_draft(account, &draft.id, draft.revision)
            .unwrap();
        let permit = store.begin_smtp(account, &id).unwrap();
        // Store-only simulated outcome: no network transport or credentials.
        store
            .finish_smtp(permit, SmtpOutcome::Accepted, None)
            .unwrap();
        if name == "已保存副本" {
            store
                .finish_copy(
                    account,
                    &id,
                    CopyOutcome::Saved {
                        folder: "Sent".into(),
                        uidvalidity: 52,
                        uid: Some(11),
                        existed: false,
                    },
                )
                .unwrap();
            let task = store.task(account, &id).unwrap();
            store
                .apply_folder_batch(
                    account,
                    &FolderBatch {
                        remote_name: "Sent".into(),
                        uidvalidity: 52,
                        since: 0,
                        has_more: false,
                        complete: true,
                        kind: mail_core::BatchKind::Recent,
                        flags: Default::default(),
                        messages: vec![(
                            11,
                            MessageEnvelope {
                                subject: task.recipients.subject,
                                sender: task.sender,
                                received_at: task.created_at,
                            },
                        )],
                    },
                    task.created_at,
                )
                .unwrap();
        }
    }
}
