use rustix::fs::{self as rfs, FlockOperation, Mode, OFlags};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Open,
    Editor,
    Reader,
    Accounts,
    Quit,
    Status,
}
impl Command {
    fn wire(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Editor => "EDITOR",
            Self::Reader => "READER",
            Self::Accounts => "ACCOUNTS",
            Self::Quit => "QUIT",
            Self::Status => "STATUS",
        }
    }
    fn parse(line: &[u8]) -> io::Result<Self> {
        match line {
            b"QMP0 OPEN\n" => Ok(Self::Open),
            b"QMP0 EDITOR\n" => Ok(Self::Editor),
            b"QMP0 READER\n" => Ok(Self::Reader),
            b"QMP0 ACCOUNTS\n" => Ok(Self::Accounts),
            b"QMP0 QUIT\n" => Ok(Self::Quit),
            b"QMP0 STATUS\n" => Ok(Self::Status),
            _ => Err(io::Error::other("不支持的 P0 控制命令")),
        }
    }
}

pub enum Launch {
    Primary(Instance),
    Forwarded(String),
}
pub struct Instance {
    _directory: File,
    _lock: File,
    listener: UnixListener,
    socket: PathBuf,
}

fn private_metadata(path: &Path, directory: bool) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    let valid = if directory {
        m.is_dir() && m.mode() & 0o777 == 0o700
    } else {
        m.is_file() && m.mode() & 0o777 == 0o600 && m.nlink() == 1
    };
    if !valid || m.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::other("运行目录/锁的类型、所有者或权限不安全"));
    }
    Ok(())
}

impl Instance {
    pub fn launch(root: &Path, command: Command) -> io::Result<Launch> {
        private_metadata(root, true)?;
        let parent: File = rfs::open(
            root,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        match rfs::mkdirat(&parent, "qingmail-p0", Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => (),
            Err(e) => return Err(e.into()),
        }
        let directory_path = root.join("qingmail-p0");
        private_metadata(&directory_path, true)?;
        let directory: File = rfs::openat(
            &parent,
            "qingmail-p0",
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        let lock: File = rfs::openat(
            &directory,
            "instance.lock",
            OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?
        .into();
        private_metadata(&directory_path.join("instance.lock"), false)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match rfs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK) => match control(root, command) {
                    Ok(reply) => return Ok(Launch::Forwarded(reply)),
                    Err(error) if std::time::Instant::now() >= deadline => {
                        return Err(io::Error::other(format!(
                            "实例锁仍被占用且激活失败：{error}"
                        )));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(40)),
                },
                Err(e) => return Err(e.into()),
            }
        }
        if matches!(command, Command::Quit | Command::Status) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "没有正在运行的 QingMail P0 实例",
            ));
        }
        let socket = directory_path.join("activate.sock");
        match fs::symlink_metadata(&socket) {
            Ok(m)
                if m.file_type().is_socket() && m.uid() == rustix::process::geteuid().as_raw() =>
            {
                fs::remove_file(&socket)?
            }
            Ok(_) => return Err(io::Error::other("拒绝替换非本用户 socket 的路径")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Launch::Primary(Self {
            _directory: directory,
            _lock: lock,
            listener,
            socket,
        }))
    }

    /// 仅后台线程调用。一次请求最多 32 bytes，200 ms 超时；验证同 UID。
    pub fn poll(
        &self,
        status: &str,
        queue: &std::sync::mpsc::SyncSender<Command>,
    ) -> io::Result<()> {
        let (mut stream, _) = match self.listener.accept() {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        };
        if rustix::net::sockopt::socket_peercred(&stream)?.uid != rustix::process::geteuid() {
            return Err(io::Error::other("拒绝不同用户的连接"));
        }
        stream.set_read_timeout(Some(Duration::from_millis(200)))?;
        stream.set_write_timeout(Some(Duration::from_millis(200)))?;
        let mut bytes = Vec::new();
        for _ in 0..32 {
            let mut byte = [0];
            stream.read_exact(&mut byte)?;
            bytes.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        let command = match Command::parse(&bytes) {
            Ok(v) => v,
            Err(_) => {
                stream.write_all(b"ERROR invalid command\n")?;
                return Ok(());
            }
        };
        if command != Command::Status && queue.try_send(command).is_err() {
            stream.write_all(b"ERROR busy\n")?;
        } else {
            stream.write_all(format!("OK {status}\n").as_bytes())?;
        }
        Ok(())
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

pub fn control(root: &Path, command: Command) -> io::Result<String> {
    private_metadata(root, true)?;
    private_metadata(&root.join("qingmail-p0"), true)?;
    let path = root.join("qingmail-p0/activate.sock");
    let m = fs::symlink_metadata(&path)?;
    if !m.file_type().is_socket()
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o777 != 0o600
    {
        return Err(io::Error::other("不安全的激活 socket"));
    }
    let mut stream = UnixStream::connect(path)?;
    if rustix::net::sockopt::socket_peercred(&stream)?.uid != rustix::process::geteuid() {
        return Err(io::Error::other("激活服务用户不匹配"));
    }
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(format!("QMP0 {}\n", command.wire()).as_bytes())?;
    let mut bytes = Vec::new();
    for _ in 0..512 {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    let reply = String::from_utf8(bytes).map_err(io::Error::other)?;
    if !reply.starts_with("OK ") || !reply.ends_with('\n') {
        return Err(io::Error::other(reply));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lock_forward_stale_socket_and_paths() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let Launch::Primary(instance) = Instance::launch(root.path(), Command::Open).unwrap()
        else {
            panic!()
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        std::thread::scope(|scope| {
            let instance = &instance;
            scope.spawn(move || {
                loop {
                    instance.poll("pid=42 ticks=7", &tx).unwrap();
                    if rx.try_recv().is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            assert!(
                matches!(Instance::launch(root.path(), Command::Editor).unwrap(), Launch::Forwarded(s) if s.contains("pid=42"))
            );
        });
        drop(instance);
        assert!(!root.path().join("qingmail-p0/activate.sock").exists());
        let stale = UnixListener::bind(root.path().join("qingmail-p0/activate.sock")).unwrap();
        drop(stale);
        let instance = Instance::launch(root.path(), Command::Open).unwrap();
        drop(instance);
        fs::write(root.path().join("qingmail-p0/activate.sock"), "keep").unwrap();
        assert!(Instance::launch(root.path(), Command::Open).is_err());
        assert_eq!(
            fs::read_to_string(root.path().join("qingmail-p0/activate.sock")).unwrap(),
            "keep"
        );
    }
    #[test]
    fn startup_retries_lock_after_previous_owner_disappears() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let previous = Instance::launch(root.path(), Command::Open).unwrap();
        let path = root.path().to_owned();
        let replacement = std::thread::spawn(move || Instance::launch(&path, Command::Open));
        std::thread::sleep(Duration::from_millis(100));
        drop(previous);
        assert!(matches!(
            replacement.join().unwrap().unwrap(),
            Launch::Primary(_)
        ));
    }
    #[test]
    fn rejects_permissions_and_protocol() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Instance::launch(root.path(), Command::Open).is_err());
        assert!(Command::parse(b"QMP0 EXEC /bin/sh\n").is_err());
        assert!(Command::parse(b"QMP1 OPEN\n").is_err());
    }
}
