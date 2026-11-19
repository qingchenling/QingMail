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
    let folders = vec![
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
    println!("合成目录与240封头部索引已写入隔离目录；未创建系统凭据或访问网络");
}
