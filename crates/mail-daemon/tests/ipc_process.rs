//! 真实独立进程 + 私有临时目录；无账号、密钥库或外部邮件网络。
use mail_protocol::{transport::Client, *};
use std::{
    fs,
    io::{BufReader, Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt, symlink},
        net::UnixStream,
    },
    path::Path,
    process::{Child, Command as ProcessCommand, Stdio},
    time::{Duration, Instant},
};
struct Daemon(Child);
fn private_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}
impl Daemon {
    fn start(root: &Path) -> Self {
        Self(
            ProcessCommand::new(env!("CARGO_BIN_EXE_mail-daemon"))
                .arg("--isolated-root")
                .arg(root)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }
    fn ready(&mut self, root: &Path) -> Client {
        let client = Client::new(root.join("runtime"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(client.request(Command::Hello), Ok(Reply::Hello { .. })) {
                return client;
            }
            if let Some(status) = self.0.try_wait().unwrap() {
                let mut error = String::new();
                self.0
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut error)
                    .unwrap();
                panic!("daemon exited {status}: {error}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("daemon startup deadline");
    }
    fn exited(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "daemon shutdown deadline");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn document(text: &str) -> DraftDocument {
    DraftDocument {
        headers: Default::default(),
        body: mail_content::draft::Draft::plain(text).unwrap(),
        attachments: vec![],
    }
}
fn get_draft(reply: Reply) -> StoredDraft {
    match reply {
        Reply::Draft(draft) => draft,
        _ => panic!("not a draft"),
    }
}

#[test]
fn multi_account_inbox_search_read_drafts_and_options_survive_daemon_restart() {
    use mail_core::*;
    let root = private_root();
    let mut ids = vec![];
    let mut locations = vec![];
    {
        let mut store = mail_storage::Store::open(&root.path().join("data")).unwrap();
        for n in 0..2 {
            let server = ServerSettings {
                host: "example.invalid".into(),
                port: 993,
                username: "fixture".into(),
                security: ConnectionSecurity::Tls,
            };
            let id = store
                .create_account(&AccountSettings {
                    display_name: format!("Account {n}"),
                    address: format!("{n}@example.invalid"),
                    imap: server.clone(),
                    smtp: server,
                })
                .unwrap();
            let folder = store.upsert_folder(&id, "INBOX", 42).unwrap();
            let location = MailUid {
                account: id.clone(),
                folder_id: folder.id.0,
                uidvalidity: 42,
                uid: 7,
            };
            store
                .cache_message(
                    &location,
                    &MessageEnvelope {
                        subject: format!("account-{n}"),
                        sender: "same@example.invalid".into(),
                        received_at: 100,
                    },
                )
                .unwrap();
            // 相同 UID/UIDVALIDITY/Message-ID，正文仍必须按完整身份读取。
            store
                .save_message_body(
                    &location,
                    format!(
                        "Message-ID: <same@example.invalid>\r\nSubject: account-{n}\r\n\r\nbody-{n}"
                    )
                    .as_bytes(),
                )
                .unwrap();
            ids.push(id);
            locations.push(location);
        }
    }
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let mut cursor = None;
    let mut listed = vec![];
    loop {
        let Reply::Inbox(page) = client
            .request(Command::UnifiedInbox {
                before: cursor,
                limit: 1,
            })
            .unwrap()
        else {
            panic!()
        };
        listed.extend(page.messages.into_iter().map(|m| m.location));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(listed.len(), 2);
    assert!(locations.iter().all(|loc| listed.contains(loc)));
    for (n, location) in locations.iter().enumerate() {
        let content = mail_daemon::client::content(
            &client,
            BusinessOperation::Read {
                location: location.clone(),
                folder: "INBOX".into(),
            },
        )
        .unwrap();
        assert_eq!(content.subject, format!("account-{n}"));
    }
    let Reply::Search(hits) = client
        .request(Command::Search {
            account: None,
            query: SearchQuery {
                text: "account".into(),
                ..Default::default()
            },
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(hits.len(), 2);
    let Reply::Search(hits) = client
        .request(Command::Search {
            account: Some(ids[0].clone()),
            query: SearchQuery::default(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message.location.account, ids[0]);
    let a = get_draft(
        client
            .request(Command::NewDraft {
                account: Some(ids[0].clone()),
                document: document("first-account"),
            })
            .unwrap(),
    );
    let b = get_draft(
        client
            .request(Command::NewDraft {
                account: Some(ids[1].clone()),
                document: document("second-account"),
            })
            .unwrap(),
    );
    assert_ne!(a.account, b.account);
    assert!(
        client
            .request(Command::SaveDraft {
                account: ids[1].clone(),
                draft: a.id.clone(),
                revision: a.revision,
                document: document("wrong-account")
            })
            .is_err()
    );
    let default = Request::new(
        "e".repeat(32),
        Command::SetDefaultAccount {
            account: ids[1].clone(),
        },
    );
    client.call(&default).unwrap();
    client.call(&default).unwrap();
    let pause = Request::new(
        "f".repeat(32),
        Command::PauseSync {
            account: Some(ids[0].clone()),
            paused: true,
        },
    );
    client.call(&pause).unwrap();
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
    let mut restarted = Daemon::start(root.path());
    let client = restarted.ready(root.path());
    let Reply::Snapshot(snapshot) = client.request(Command::Snapshot).unwrap() else {
        panic!()
    };
    assert_eq!(snapshot.default_account, Some(ids[1].clone()));
    assert_eq!(snapshot.accounts.len(), 2);
    let Reply::AccountOptions { default, paused } =
        client.request(Command::AccountOptions).unwrap()
    else {
        panic!()
    };
    assert_eq!(default, Some(ids[1].clone()));
    assert_eq!(paused, vec![ids[0].clone()]);
    client.call(&pause).unwrap(); // 已完成的持久请求只返回原结果。
    let remote = mail_daemon::storage::StorageWorker::connect(client.clone()).unwrap();
    assert_eq!(
        mail_daemon::storage::receive(remote.client().account_drafts(ids[0].clone())).unwrap(),
        vec![a]
    );
    assert_eq!(
        mail_daemon::storage::receive(remote.client().account_drafts(ids[1].clone())).unwrap(),
        vec![b]
    );
    drop(remote);
    client.request(Command::PrepareShutdown).unwrap();
    restarted.exited();
}
#[test]
fn independent_process_lost_reply_restart_and_existing_instance() {
    let root = private_root();
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    match client.request(Command::Status).unwrap() {
        Reply::Status(s) => {
            assert_eq!(s.pid, daemon.0.id());
            assert_ne!(s.pid, std::process::id());
            assert!(s.sync_paused);
            assert!(!s.sending);
        }
        _ => panic!(),
    }
    let capabilities = client.request(Command::Hello).unwrap();
    assert!(
        matches!(capabilities,Reply::Hello {commands,..} if commands.contains(&CommandName::SaveDraft) && !commands.contains(&CommandName::ConfirmSend) && !commands.contains(&CommandName::CheckAccount))
    );
    let socket = root.path().join("runtime/qingmail/daemon.sock");
    assert_eq!(fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
    assert_eq!(
        fs::metadata(socket.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    let second = ProcessCommand::new(env!("CARGO_BIN_EXE_mail-daemon"))
        .arg("--isolated-root")
        .arg(root.path())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(
        matches!(client.request(Command::Status).unwrap(),Reply::Status(s) if s.pid==daemon.0.id())
    );
    let request = Request::new(
        "a".repeat(32),
        Command::NewDraft {
            account: None,
            document: document("草稿🙂"),
        },
    );
    // 客户端在读响应前消失；后台仍须完成已接收操作，并保留原请求结果。
    {
        let mut stream = UnixStream::connect(&socket).unwrap();
        write_json(&mut stream, &request).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let first = loop {
        match client
            .request(Command::MutationResult {
                mutation: request.id.clone(),
            })
            .unwrap()
        {
            Reply::Mutation(MutationStatus::Finished(result)) => {
                break get_draft((*result).unwrap());
            }
            Reply::Mutation(MutationStatus::NotFound | MutationStatus::InProgress) => (),
            other => panic!("unexpected result: {other:?}"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(get_draft(client.call(&request).unwrap()), first);
    let save = Request::new(
        "b".repeat(32),
        Command::SaveDraft {
            account: first.account.clone(),
            draft: first.id.clone(),
            revision: first.revision,
            document: document("最新修改🙂"),
        },
    );
    let saved = get_draft(client.call(&save).unwrap());
    assert_eq!(saved.revision, 2);
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
    assert!(!socket.exists());
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    assert_eq!(get_draft(client.call(&save).unwrap()), saved);
    assert_eq!(
        get_draft(
            client
                .request(Command::OpenDraft {
                    account: None,
                    initial: mail_content::draft::Draft::plain("must not overwrite").unwrap()
                })
                .unwrap()
        ),
        saved
    );
    assert!(
        matches!(client.request(Command::Drafts {account:None}).unwrap(),Reply::Drafts(d) if d==vec![saved])
    );
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
}
#[test]
fn killed_daemon_reopens_saved_draft_and_reconciles_original_request() {
    let root = private_root();
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let request = Request::new(
        "c".repeat(32),
        Command::NewDraft {
            account: None,
            document: document("强制终止前已确认保存🙂"),
        },
    );
    let saved = get_draft(client.call(&request).unwrap());
    // SIGKILL 没有清理 socket，也没有运行 Rust 的 Drop。
    daemon.0.kill().unwrap();
    assert!(!daemon.0.wait().unwrap().success());
    let mut restarted = Daemon::start(root.path());
    let client = restarted.ready(root.path());
    assert_eq!(get_draft(client.call(&request).unwrap()), saved);
    assert!(matches!(
        client.request(Command::Drafts { account: None }).unwrap(),
        Reply::Drafts(drafts) if drafts == vec![saved]
    ));
    client.request(Command::PrepareShutdown).unwrap();
    restarted.exited();
}
#[test]
fn wrong_version_unknown_fields_and_client_disconnect_never_write() {
    let root = private_root();
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let path = root.path().join("runtime/qingmail/daemon.sock");
    let cases = [
        (
            "{\"version\":99,\"id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"command\":{\"type\":\"future_write\"}}\n",
            ErrorCode::VersionMismatch,
        ),
        (
            "{\"version\":1,\"id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"password\":\"forbidden\",\"command\":{\"type\":\"accounts\"}}\n",
            ErrorCode::InvalidRequest,
        ),
    ];
    for (raw, code) in cases {
        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let response: Response = read_json(&mut BufReader::new(stream)).unwrap();
        assert_eq!(response.result.unwrap_err().code, code);
    }
    // 未完成的控制帧断开，不是退出信号。
    let mut stream = UnixStream::connect(path).unwrap();
    stream.write_all(b"{\"version\":1").unwrap();
    drop(stream);
    assert!(
        matches!(client.request(Command::Accounts).unwrap(),Reply::Accounts(accounts) if accounts.is_empty())
    );
    assert!(matches!(
        client
            .request(Command::MutationResult {
                mutation: "a".repeat(32)
            })
            .unwrap(),
        Reply::Mutation(MutationStatus::NotFound)
    ));
    assert!(matches!(
        client.request(Command::ConfirmSend {
            review: "a".repeat(32),
            allow_empty_subject: true
        }),
        Err(transport::ClientError::Remote(RpcError {
            code: ErrorCode::Unsupported,
            ..
        }))
    ));
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
}
#[test]
fn refuses_unsafe_paths_and_cleans_only_owned_stale_socket() {
    use mail_daemon::ipc::Endpoint;
    let root = private_root();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Endpoint::bind(root.path()).is_err());
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = Endpoint::bind(root.path()).unwrap();
    assert!(Endpoint::bind(root.path()).is_err());
    drop(endpoint);
    let socket = root.path().join("qingmail/daemon.sock");
    let stale = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    let endpoint = Endpoint::bind(root.path()).unwrap();
    drop(endpoint);
    fs::write(&socket, b"keep").unwrap();
    assert!(Endpoint::bind(root.path()).is_err());
    assert_eq!(fs::read(&socket).unwrap(), b"keep");
    fs::remove_file(&socket).unwrap();
    symlink("/tmp/must-not-follow", &socket).unwrap();
    assert!(Endpoint::bind(root.path()).is_err());
    assert!(socket.symlink_metadata().unwrap().file_type().is_symlink());
}
#[test]
fn dev_entrypoint_requires_explicit_isolation() {
    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_mail-daemon"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--isolated-root"));
}

#[test]
fn frontend_lease_events_and_save_failure_keep_daemon_alive() {
    let root = private_root();
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let first = "1".repeat(32);
    let second = "2".repeat(32);
    assert!(matches!(
        client
            .request(Command::AttachUi {
                session: first.clone()
            })
            .unwrap(),
        Reply::UiAttached { attached: true }
    ));
    assert!(matches!(
        client
            .request(Command::AttachUi {
                session: second.clone()
            })
            .unwrap(),
        Reply::UiAttached { attached: false }
    ));
    let Reply::UiState {
        activation: Some(activation),
        ..
    } = client
        .request(Command::UiHeartbeat {
            session: first.clone(),
            acknowledged: 0,
        })
        .unwrap()
    else {
        panic!()
    };
    assert!(matches!(activation.target, WindowTarget::Mailbox));
    assert!(
        client
            .request(Command::DetachUi { session: second })
            .is_err()
    );
    let Reply::Snapshot(snapshot) = client.request(Command::Snapshot).unwrap() else {
        panic!()
    };
    client
        .request(Command::NewDraft {
            account: None,
            document: document("退出前保留"),
        })
        .unwrap();
    let Reply::Events(events) = client
        .request(Command::Events {
            after: snapshot.revision,
        })
        .unwrap()
    else {
        panic!()
    };
    assert!(!events.resnapshot);
    assert!(events.revision > snapshot.revision);
    assert!(
        events
            .events
            .iter()
            .any(|e| matches!(e.topic, EventTopic::Drafts))
    );
    client.request(Command::PrepareShutdown).unwrap();
    assert!(matches!(
        client
            .request(Command::UiHeartbeat {
                session: first.clone(),
                acknowledged: activation.sequence
            })
            .unwrap(),
        Reply::UiState {
            shutdown_requested: true,
            ..
        }
    ));
    client
        .request(Command::FinishShutdown {
            session: first.clone(),
            saved: false,
        })
        .unwrap();
    assert!(matches!(
        client.request(Command::Status).unwrap(),
        Reply::Status(_)
    ));
    client
        .request(Command::DetachUi {
            session: first.clone(),
        })
        .unwrap();
    // 关闭前台后后台与草稿仍在；重开复用后台，而不是再启动一个数据库写者。
    assert!(
        matches!(client.request(Command::Drafts {account:None}).unwrap(),Reply::Drafts(d) if d.len()==1)
    );
    client
        .request(Command::AttachUi {
            session: first.clone(),
        })
        .unwrap();
    client.request(Command::PrepareShutdown).unwrap();
    client
        .request(Command::FinishShutdown {
            session: first,
            saved: true,
        })
        .unwrap();
    daemon.exited();
}

#[test]
fn binary_resources_roundtrip_without_json_bytes_and_incomplete_upload_never_imports() {
    let root = private_root();
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let body = "中文🙂\n".repeat(1000);
    let original = document(&body);
    let draft = get_draft(
        client
            .request(Command::NewDraft {
                account: None,
                document: original.clone(),
            })
            .unwrap(),
    );
    let Reply::OptionalResource(Some(resource)) = client
        .request(Command::SelectedDraftBody { account: None })
        .unwrap()
    else {
        panic!()
    };
    assert!(resource.size > u64::from(MAX_CHUNK_BYTES));
    let bytes = client.read_resource(&resource, 1024 * 1024).unwrap();
    assert_eq!(
        mail_content::draft::Draft::from_json(&bytes).unwrap(),
        original.body
    );
    assert!(client.read_resource(&resource, 1).is_err());
    let Reply::ResourceCreated(upload) = client
        .request(Command::CreateUpload {
            account: draft.account.clone(),
            name: "测试.bin".into(),
            media_type: "application/octet-stream".into(),
            size: 70000,
        })
        .unwrap()
    else {
        panic!()
    };
    let bytes: Vec<_> = (0..70000).map(|n| (n % 256) as u8).collect();
    client.write_chunk(&upload.id, 0, &bytes[..65536]).unwrap();
    // 一个新确认请求记录明确失败；资源仍可补全，再由另一个明确确认请求导入。
    assert!(
        client
            .request(Command::FinishUpload {
                resource: upload.id.clone()
            })
            .is_err()
    );
    assert!(
        client
            .write_chunk(&upload.id, 65536, &vec![0; 5000])
            .is_err()
    );
    client
        .write_chunk(&upload.id, 65536, &bytes[65536..])
        .unwrap();
    let finish = Request::new(
        "f".repeat(32),
        Command::FinishUpload {
            resource: upload.id.clone(),
        },
    );
    let Reply::Attachment(attachment) = client.call(&finish).unwrap() else {
        panic!()
    };
    let Reply::Attachment(replayed) = client.call(&finish).unwrap() else {
        panic!()
    };
    assert_eq!(attachment, replayed);
    assert_eq!(attachment.size, 70000);
    assert!(client.write_chunk(&upload.id, 70000, b"x").is_err());
    let Reply::Attachments(infos) = client
        .request(Command::AttachmentInfos {
            account: draft.account,
            ids: vec![attachment.id.clone()],
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(infos, vec![attachment]);
    client
        .request(Command::CancelResource {
            resource: resource.id.clone(),
        })
        .unwrap();
    assert!(client.read_chunk(&resource.id, 0, 1).is_err());
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
}

#[test]
fn existing_editor_storage_api_works_through_ipc_without_another_database_owner() {
    use mail_daemon::storage::{StorageWorker, receive};
    let root = private_root();
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let mut frontend = StorageWorker::connect(client.clone()).unwrap();
    let saved = receive(
        frontend
            .client()
            .open_editor(mail_content::draft::Draft::plain("前台草稿🙂").unwrap()),
    )
    .unwrap();
    let selected = root.path().join("附件.txt");
    fs::write(&selected, "附件内容🙂").unwrap();
    let info = receive(
        frontend
            .client()
            .import_attachment(saved.account.clone(), selected.clone()),
    )
    .unwrap();
    fs::remove_file(&selected).unwrap();
    let mut document = saved.document.clone();
    document.attachments.push(info.id.clone());
    document.headers.subject = "IPC保存".into();
    let saved = receive(frontend.client().save_document(saved, document)).unwrap();
    let raw = receive(frontend.client().selected_draft_body(None))
        .unwrap()
        .unwrap();
    assert_eq!(
        mail_content::draft::Draft::from_json(&raw).unwrap(),
        saved.document.body
    );
    frontend.stop();
    assert!(matches!(
        client.request(Command::Status).unwrap(),
        Reply::Status(_)
    ));
    let mut reopened = StorageWorker::connect(client.clone()).unwrap();
    assert_eq!(
        receive(
            reopened
                .client()
                .open_editor(mail_content::draft::Draft::plain("不得覆盖").unwrap())
        )
        .unwrap(),
        saved
    );
    assert_eq!(
        receive(
            reopened
                .client()
                .attachment_infos(saved.account, vec![info.id.clone()])
        )
        .unwrap(),
        vec![info]
    );
    reopened.stop();
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
}

#[test]
fn business_cached_reader_resource_cleanup_identity_and_removal_replay() {
    use mail_core::*;
    let root = private_root();
    let location = {
        let mut store = mail_storage::Store::open(&root.path().join("data")).unwrap();
        let server = ServerSettings {
            host: "example.invalid".into(),
            port: 993,
            username: "synthetic".into(),
            security: ConnectionSecurity::Tls,
        };
        let account = store
            .create_account(&AccountSettings {
                display_name: "IPC reader".into(),
                address: "reader@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        let folder = store.upsert_folder(&account, "INBOX", 42).unwrap();
        let location = MailUid {
            account,
            folder_id: folder.id.0,
            uidvalidity: 42,
            uid: 7,
        };
        store
            .cache_message(
                &location,
                &MessageEnvelope {
                    subject: "cached".into(),
                    sender: "synthetic".into(),
                    received_at: 0,
                },
            )
            .unwrap();
        store
            .save_message_body(
                &location,
                include_bytes!("../../../tests/fixtures/mime/03-alternative-related.eml"),
            )
            .unwrap();
        location
    };
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let Reply::Snapshot(before_read) = client.request(Command::Snapshot).unwrap() else {
        panic!()
    };
    let content = mail_daemon::client::content(
        &client,
        BusinessOperation::Read {
            location: location.clone(),
            folder: "INBOX".into(),
        },
    )
    .unwrap();
    let Reply::Snapshot(after_read) = client.request(Command::Snapshot).unwrap() else {
        panic!()
    };
    assert_eq!(
        before_read.revision, after_read.revision,
        "read resources must not trigger mailbox reload loops"
    );
    assert!(content.is_html);
    assert!(!content.images.is_empty());
    assert!(!content.attachments.is_empty());
    let handle = content.display_handle.unwrap();
    // Materialization releases binary transport IDs, but the bounded display handle remains usable.
    assert!(client.read_chunk(&handle, 0, 1).is_err());
    let copy = mail_daemon::client::run(
        &client,
        BusinessOperation::BrowserCopy {
            content: handle.clone(),
        },
    )
    .unwrap();
    assert!(
        matches!(copy,Reply::BrowserCopy {url,..} if url.starts_with("file://") && url.ends_with(".html"))
    );
    assert!(
        mail_daemon::client::content(&client, BusinessOperation::RemoteImages { content: handle })
            .err()
            .unwrap()
            .contains("隔离模式")
    );
    assert!(
        mail_daemon::client::run(
            &client,
            BusinessOperation::Read {
                location: location.clone(),
                folder: "OTHER".into()
            }
        )
        .unwrap_err()
        .contains("定位")
    );
    let mut stale = location.clone();
    stale.uidvalidity += 1;
    assert!(
        mail_daemon::client::run(
            &client,
            BusinessOperation::Read {
                location: stale,
                folder: "INBOX".into()
            }
        )
        .is_err()
    );
    // More than the result-ring capacity must not exhaust normal reading indefinitely.
    for _ in 0..70 {
        mail_daemon::client::run(&client, BusinessOperation::ClearCopies).unwrap();
    }
    let request = Request::new(
        "9".repeat(32),
        Command::Business {
            operation: BusinessOperation::RemoveAccount {
                account: location.account.clone(),
            },
        },
    );
    assert!(matches!(
        client.call(&request).unwrap(),
        Reply::Pending { .. }
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Reply::Job {
            result: Some(result),
        } = client
            .request(Command::JobResult {
                task: request.id.clone(),
            })
            .unwrap()
        {
            assert!(result.is_ok());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    // Repeating the original ID returns its durable result, never removes a newly selected account.
    assert!(matches!(
        client.call(&request).unwrap(),
        Reply::AccountImpact(_)
    ));
    assert!(
        matches!(client.request(Command::Accounts).unwrap(),Reply::Accounts(v) if v.is_empty())
    );
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
}

#[test]
fn large_draft_chunked_write_lost_response_query_and_keyset_pages() {
    let root = private_root();
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let body = "x".repeat(mail_content::draft::MAX_TEXT);
    let mut large_document = document(&body);
    large_document
        .body
        .set_link(
            0..body.len(),
            Some("https://example.invalid/large-rich-text-link"),
        )
        .unwrap();
    let request = Request::new(
        "8".repeat(32),
        Command::NewDraft {
            account: None,
            document: large_document,
        },
    );
    assert!(serde_json::to_vec(&request).unwrap().len() > MAX_CONTROL_BYTES);
    let draft = get_draft(client.call(&request).unwrap());
    assert_eq!(draft.document.body.text(), body);
    assert_eq!(get_draft(client.call(&request).unwrap()), draft);
    assert!(
        matches!(client.request(Command::MutationResult{mutation:request.id.clone()}).unwrap(),Reply::Mutation(MutationStatus::Finished(result)) if matches!(&*result,Ok(Reply::Draft(d)) if d==&draft))
    );
    let second = get_draft(
        client
            .request(Command::NewDraft {
                account: None,
                document: document("second"),
            })
            .unwrap(),
    );
    let mut after = None;
    let mut ids = vec![];
    loop {
        let Reply::DraftPage { items, next } = client
            .request(Command::DraftPage {
                account: None,
                after,
            })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(items.len(), 1);
        ids.push(items[0].id.clone());
        if next.is_none() {
            break;
        }
        after = next;
    }
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&draft.id) && ids.contains(&second.id));
    // The existing editor API receives the same complete list through paginated IPC.
    let remote = mail_daemon::storage::StorageWorker::connect(client.clone()).unwrap();
    let listed = mail_daemon::storage::receive(remote.client().list()).unwrap();
    assert_eq!(listed.len(), 2);
    drop(remote);
    // Binary storage envelopes cannot smuggle a network/recursive command.
    let inner = Request::new("7".repeat(32), Command::PrepareShutdown);
    let bytes = serde_json::to_vec(&inner).unwrap();
    let Reply::ResourceCreated(resource) = client
        .request(Command::CreateStorageRequest {
            size: bytes.len() as u64,
        })
        .unwrap()
    else {
        panic!()
    };
    client.write_chunk(&resource.id, 0, &bytes).unwrap();
    assert!(
        client
            .call(&Request::new(
                inner.id,
                Command::StorageRequest {
                    resource: resource.id
                }
            ))
            .is_err()
    );
    assert!(client.request(Command::Status).is_ok());
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
}

#[test]
fn offline_operations_lost_reply_restart_overlay_and_explicit_resolution() {
    use mail_core::*;
    let root = private_root();
    let (loc, trash) = {
        let mut store = mail_storage::Store::open(&root.path().join("data")).unwrap();
        let server = ServerSettings {
            host: "example.invalid".into(),
            port: 993,
            username: "fixture".into(),
            security: ConnectionSecurity::Tls,
        };
        let account = store
            .create_account(&AccountSettings {
                display_name: "Offline fixture".into(),
                address: "offline@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        store
            .replace_remote_folders(
                &account,
                &[
                    RemoteFolder {
                        name: "INBOX".into(),
                        display_name: "INBOX".into(),
                        delimiter: None,
                        selectable: true,
                    },
                    RemoteFolder {
                        name: "Trash".into(),
                        display_name: "Trash".into(),
                        delimiter: None,
                        selectable: true,
                    },
                ],
                1,
            )
            .unwrap();
        let inbox = store.upsert_folder(&account, "INBOX", 42).unwrap();
        let trash = store.upsert_folder(&account, "Trash", 42).unwrap();
        let loc = MailUid {
            account,
            folder_id: inbox.id.0,
            uidvalidity: 42,
            uid: 7,
        };
        store
            .cache_message(
                &loc,
                &MessageEnvelope {
                    subject: "offline".into(),
                    sender: "fixture@example.invalid".into(),
                    received_at: 1,
                },
            )
            .unwrap();
        (loc, trash.id)
    };
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let request = Request::new(
        "e".repeat(32),
        Command::EnqueueOperation {
            locations: vec![loc.clone()],
            operation: MailOperation::Flags {
                seen: Some(true),
                flagged: Some(true),
            },
        },
    );
    // 客户端丢弃响应；只查询原ID，不以新ID重新执行。
    let mut socket = UnixStream::connect(root.path().join("runtime/qingmail/daemon.sock")).unwrap();
    let mut bytes = serde_json::to_vec(&request).unwrap();
    bytes.push(b'\n');
    socket.write_all(&bytes).unwrap();
    drop(socket);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            client
                .request(Command::MutationResult {
                    mutation: request.id.clone()
                })
                .unwrap(),
            Reply::Mutation(MutationStatus::Finished(_))
        ) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(matches!(client.call(&request).unwrap(),Reply::Operations(v) if v.len()==1));
    assert!(
        matches!(client.request(Command::UnifiedInbox{before:None,limit:10}).unwrap(),Reply::Inbox(p) if p.messages[0].seen && p.messages[0].flagged)
    );
    // 模拟崩溃，后台重启不得丢失已确认入队的意图。
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    drop(daemon);
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    assert!(
        matches!(client.request(Command::Operations{account:None}).unwrap(),Reply::Operations(v) if v.len()==1 && v[0].state==OperationState::Pending)
    );
    assert!(
        matches!(client.request(Command::UnifiedInbox{before:None,limit:10}).unwrap(),Reply::Inbox(p) if p.messages[0].seen)
    );
    let moved = match client
        .request(Command::EnqueueOperation {
            locations: vec![loc.clone()],
            operation: MailOperation::Move { destination: trash },
        })
        .unwrap()
    {
        Reply::Operations(v) => v[0].id.clone(),
        _ => panic!(),
    };
    assert!(
        matches!(client.request(Command::UnifiedInbox{before:None,limit:10}).unwrap(),Reply::Inbox(p) if p.messages.is_empty())
    );
    // 待同步移动不能以“重试”绕过持久顺序；SMTP不会被队列触发。
    assert!(
        client
            .request(Command::RetryOperation { operation: moved })
            .is_err()
    );
    assert!(
        matches!(client.request(Command::Tasks{account:loc.account}).unwrap(),Reply::Tasks(v) if v.is_empty())
    );
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
}

#[test]
fn parts_deferred_large_attachment_offline_body_browser_and_file_resource_stream() {
    use mail_core::*;
    let root = private_root();
    let (location, large) = {
        let mut store = mail_storage::Store::open(&root.path().join("data")).unwrap();
        let server = ServerSettings {
            host: "example.invalid".into(),
            port: 993,
            username: "fixture".into(),
            security: ConnectionSecurity::Tls,
        };
        let account = store
            .create_account(&AccountSettings {
                display_name: "parts".into(),
                address: "parts@example.invalid".into(),
                imap: server.clone(),
                smtp: server,
            })
            .unwrap();
        let inbox = store.upsert_folder(&account, "INBOX", 42).unwrap();
        let location = MailUid {
            account,
            folder_id: inbox.id.0,
            uidvalidity: 42,
            uid: 7,
        };
        store
            .cache_message(
                &location,
                &MessageEnvelope {
                    subject: "sections".into(),
                    sender: "fixture@example.invalid".into(),
                    received_at: 1,
                },
            )
            .unwrap();
        let small = MimePart {
            section: "1".into(),
            media_type: "text/html".into(),
            charset: Some("utf-8".into()),
            encoding: "binary".into(),
            encoded_size: 160,
            name: None,
            cid: None,
            attachment: false,
        };
        let large = MimePart {
            section: "2".into(),
            media_type: "application/octet-stream".into(),
            charset: None,
            encoding: "base64".into(),
            encoded_size: 12 * 1024 * 1024,
            name: Some("large.bin".into()),
            cid: None,
            attachment: true,
        };
        store
            .save_mime_structure(
                &location,
                &MimeStructure {
                    total_size: 13 * 1024 * 1024,
                    headers: b"From: fixture@example.invalid\r\nSubject: sections\r\n\r\n".to_vec(),
                    parts: vec![small.clone(), large.clone()],
                },
            )
            .unwrap();
        let mut file = store.part_temporary().unwrap();
        file.write_all("<p>小正文可离线阅读</p><script>evil()</script><img src=\"https://example.invalid/tracker\">".as_bytes()).unwrap();
        store.save_part(&location, &small, file).unwrap();
        assert!(store.part_file(&location, &large).unwrap().is_none());
        (location, large)
    };
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let content = mail_daemon::client::content(
        &client,
        BusinessOperation::Read {
            location: location.clone(),
            folder: "INBOX".into(),
        },
    )
    .unwrap();
    assert!(content.plain.contains("小正文"));
    assert!(!content.document.native_html().contains("<script"));
    assert_eq!(content.attachments.len(), 1);
    assert!(content.attachments[0].bytes.is_empty());
    let id = content.attachments[0].remote.clone().unwrap();
    assert!(
        matches!(client.request(Command::ResourceStatus{resource:id.clone()}).unwrap(),Reply::Download(s) if s.resource.state==ResourceState::NotCached)
    );
    assert!(client.read_chunk(&id, 0, 65536).is_err());
    assert!(
        client
            .request(Command::DownloadAttachment {
                resource: id.clone()
            })
            .is_err()
    );
    let Reply::BrowserCopy { url } = mail_daemon::client::run(
        &client,
        BusinessOperation::BrowserCopy {
            content: content.display_handle.unwrap(),
        },
    )
    .unwrap() else {
        panic!()
    };
    let html = fs::read_to_string(url.strip_prefix("file://").unwrap()).unwrap();
    assert!(html.contains("小正文"));
    assert!(html.contains("connect-src 'none'"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("src=\"https://example.invalid/tracker\""));
    assert!(!html.contains("large.bin"));
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
    drop(daemon);
    {
        let mut store = mail_storage::Store::open(&root.path().join("data")).unwrap();
        assert!(store.part_file(&location, &large).unwrap().is_none());
        let mut file = store.part_temporary().unwrap();
        let chunk = [0x5a; 65536];
        for _ in 0..144 {
            file.write_all(&chunk).unwrap();
        }
        store.save_part(&location, &large, file).unwrap();
    }
    let mut daemon = Daemon::start(root.path());
    let client = daemon.ready(root.path());
    let content = mail_daemon::client::content(
        &client,
        BusinessOperation::Read {
            location: location.clone(),
            folder: "INBOX".into(),
        },
    )
    .unwrap();
    assert!(content.attachments[0].bytes.is_empty());
    let id = content.attachments[0].remote.clone().unwrap();
    let Reply::Download(status) = client
        .request(Command::DownloadAttachment {
            resource: id.clone(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(status.resource.state, ResourceState::Ready);
    assert_eq!(status.resource.size, 9 * 1024 * 1024);
    let (first, bytes) = client.read_chunk(&id, 0, 65536).unwrap();
    assert_eq!(first.total, 9 * 1024 * 1024);
    assert_eq!(bytes, vec![0x5a; 65536]);
    let (last, bytes) = client.read_chunk(&id, 9 * 1024 * 1024 - 5, 65536).unwrap();
    assert!(last.complete);
    assert_eq!(bytes, vec![0x5a; 5]);
    assert!(client.read_chunk(&id, 0, 65537).is_err());
    let Reply::Pending { task } = client.request(Command::ReadMessage { location }).unwrap() else {
        panic!()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            client
                .request(Command::JobResult { task: task.clone() })
                .unwrap(),
            Reply::Job { result: Some(_) }
        ) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    client.request(Command::PrepareShutdown).unwrap();
    daemon.exited();
}
