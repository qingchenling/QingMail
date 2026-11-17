use rustix::fs::{self as rfs, FlockOperation, Mode, OFlags};
use std::{
    fs::{self, File},
    io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
};

pub struct Endpoint {
    _directory: File,
    _lock: File,
    listener: UnixListener,
    socket: PathBuf,
    inode: u64,
}
pub(super) fn check_directory(path: &Path) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.mode() & 0o777 != 0o700 || m.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "运行目录须为当前用户所有、无符号链接且权限0700",
        ));
    }
    Ok(())
}
impl Endpoint {
    pub fn bind(root: &Path) -> io::Result<Self> {
        if !root.is_absolute()
            || root
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "运行目录须为无相对组件的绝对路径",
            ));
        }
        check_directory(root)?;
        let parent: File = rfs::open(
            root,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        match rfs::mkdirat(&parent, "qingmail", Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => (),
            Err(e) => return Err(e.into()),
        }
        let directory_path = root.join("qingmail");
        check_directory(&directory_path)?;
        let directory: File = rfs::openat(
            &parent,
            "qingmail",
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        let lock: File = rfs::openat(
            &directory,
            "daemon.lock",
            OFlags::CREATE | OFlags::RDWR | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?
        .into();
        let m = lock.metadata()?;
        if !m.is_file()
            || m.mode() & 0o777 != 0o600
            || m.nlink() != 1
            || m.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "后台实例锁不安全",
            ));
        }
        rfs::flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "QingMail 后台已运行，请连接现有后台",
                )
            } else {
                e.into()
            }
        })?;
        let socket = directory_path.join("daemon.sock");
        match fs::symlink_metadata(&socket) {
            Ok(m)
                if m.file_type().is_socket()
                    && m.uid() == rustix::process::geteuid().as_raw()
                    && m.mode() & 0o777 == 0o600 =>
            {
                fs::remove_file(&socket)?
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "拒绝替换未知 socket 路径",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let inode = fs::symlink_metadata(&socket)?.ino();
        Ok(Self {
            _directory: directory,
            _lock: lock,
            listener,
            socket,
            inode,
        })
    }
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }
    pub fn accept(&self) -> io::Result<Option<UnixStream>> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                if rustix::net::sockopt::socket_peercred(&stream)?.uid != rustix::process::geteuid()
                {
                    return Ok(None);
                }
                Ok(Some(stream))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Ok(m) = fs::symlink_metadata(&self.socket)
            && m.ino() == self.inode
            && m.file_type().is_socket()
            && m.uid() == rustix::process::geteuid().as_raw()
        {
            let _ = fs::remove_file(&self.socket);
        }
        // fork 与 exec 之间可能短暂继承锁描述符；显式解锁避免旧实例退出后
        // 无关子进程暂时延长锁的生命周期。此时 socket 已清理，服务已停止。
        let _ = rfs::flock(&self._lock, FlockOperation::Unlock);
    }
}
