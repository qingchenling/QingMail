//! Explicit empty data directory only. Optional credentials require this fixture's private bus.
use mail_core::*;
use mail_service::credentials::{CredentialVault, Credentials, SystemVault};
use mail_storage::Store;
use std::{collections::BTreeMap, path::PathBuf};
fn main() {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(args.next().expect("absolute fresh data directory"));
    assert!(path.is_absolute());
    assert!(!path.exists(), "fixture requires a new data directory");
    let count: u32 = args
        .next()
        .expect("message count per account")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(count <= 10_000);
    let port: Option<u16> = args.next().map(|v| v.to_str().unwrap().parse().unwrap());
    if port.is_some() {
        let expected = format!("unix:path={}/bus", path.parent().unwrap().display());
        assert_eq!(
            std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap(),
            expected,
            "refuse non-fixture keyring"
        );
        assert_eq!(
            std::env::var_os("QINGMAIL_PERF_PRIVATE_ROOT")
                .map(PathBuf::from)
                .as_deref(),
            path.parent()
        );
    }
    let mut store = Store::open(&path).unwrap();
    assert!(store.accounts().unwrap().is_empty());
    for n in 0..5 {
        let server = ServerSettings {
            authentication: Default::default(),
            host: if port.is_some() {
                "localhost"
            } else {
                "example.invalid"
            }
            .into(),
            port: port.unwrap_or(993),
            username: format!("fixture-{n}"),
            security: ConnectionSecurity::Tls,
        };
        let account = store
            .create_account(&AccountSettings {
                display_name: format!("Performance fixture {n}"),
                address: format!("fixture-{n}@example.invalid"),
                imap: server.clone(),
                smtp: ServerSettings {
                    authentication: Default::default(),
                    port: 9,
                    ..server
                },
            })
            .unwrap();
        store
            .replace_remote_folders(
                &account,
                &[RemoteFolder {
                    name: "INBOX".into(),
                    display_name: "INBOX".into(),
                    delimiter: Some("/".into()),
                    selectable: true,
                }],
                1794960000,
            )
            .unwrap();
        for start in (1..=count).step_by(200) {
            store
                .apply_folder_batch(
                    &account,
                    &FolderBatch {
                        remote_name: "INBOX".into(),
                        uidvalidity: 42,
                        since: 0,
                        has_more: true,
                        complete: false,
                        kind: BatchKind::Recent,
                        messages: (start..=(start + 199).min(count))
                            .map(|uid| {
                                (
                                    uid,
                                    MessageEnvelope {
                                        subject: format!("邮件 {uid} needle-{n} 中文性能样本"),
                                        sender: "Fixture <fixture@example.invalid>".into(),
                                        received_at: 1794960000 + i64::from(uid),
                                    },
                                )
                            })
                            .collect(),
                        flags: BTreeMap::new(),
                    },
                    1794960000,
                )
                .unwrap();
        }
        if count > 0 {
            for message in store
                .folder_cache(&account, "INBOX", None)
                .unwrap()
                .unwrap()
                .messages
            {
                store.save_message_body(&message.location,b"Subject: Synthetic performance\r\nFrom: fixture@example.invalid\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nCached performance fixture body.").unwrap();
            }
        }
        if port.is_some() {
            SystemVault
                .save(
                    &account,
                    &Credentials::new("fixture-only".into(), "fixture-only".into()).unwrap(),
                )
                .unwrap();
        }
    }
    println!(
        "five isolated accounts seeded; messages_per_account={count}; private_credentials={}",
        port.is_some()
    );
}
