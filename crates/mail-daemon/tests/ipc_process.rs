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
        matches!(capabilities,Reply::Hello {commands,..} if commands.contains(&CommandName::SaveDraft) && !commands.contains(&CommandName::ConfirmSend))
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
