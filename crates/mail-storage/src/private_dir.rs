use rustix::fs::{self as rfs, FlockOperation, Mode, OFlags};
use std::{
    fs::File,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Component, Path, PathBuf},
};

use crate::{Error, Result};

pub(crate) struct PrivateDir(pub File);

impl PrivateDir {
    pub fn child(&self, name: &str) -> Result<Self> {
        match rfs::mkdirat(&self.0, name, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => (),
            Err(e) => return Err(e.into()),
        }
        let file: File = rfs::openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        let m = file.metadata()?;
        if m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o777 != 0o700 {
            return Err(Error::Invalid("对象目录须属于当前用户且权限为 0700"));
        }
        self.0.sync_all()?;
        Ok(Self(file))
    }

    pub fn read(&self, name: &str) -> Result<File> {
        let file: File = rfs::openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        Self::validate_file(&file)?;
        Ok(file)
    }

    pub fn remove(&self, name: &str) {
        let _ = rfs::unlinkat(&self.0, name, rfs::AtFlags::empty());
    }

    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_absolute() || path.parent().is_none() {
            return Err(Error::Invalid("数据目录必须是非根绝对路径"));
        }
        let mut directory = rfs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        for part in path.components() {
            let name = match part {
                Component::RootDir => continue,
                Component::Normal(name) => name,
                _ => return Err(Error::Invalid("目录不能包含相对路径组件")),
            };
            match rfs::mkdirat(&directory, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => (),
                Err(e) => return Err(e.into()),
            }
            directory = rfs::openat(
                &directory,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
        }
        let directory = File::from(directory);
        let m = directory.metadata()?;
        if m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o777 != 0o700 {
            return Err(Error::Invalid("数据目录须属于当前用户且权限为 0700"));
        }
        Ok(Self(directory))
    }

    pub fn path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd())).join(name)
    }

    pub fn file(&self, name: &str, exclusive: bool) -> Result<File> {
        let mut flags =
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        if exclusive {
            flags |= OFlags::EXCL;
        }
        let file: File = rfs::openat(&self.0, name, flags, Mode::from_raw_mode(0o600))?.into();
        Self::validate_file(&file)?;
        Ok(file)
    }

    pub fn validate_file(file: &File) -> Result<()> {
        let m = file.metadata()?;
        if !m.is_file()
            || m.uid() != rustix::process::geteuid().as_raw()
            || m.mode() & 0o777 != 0o600
            || m.nlink() != 1
        {
            return Err(Error::Invalid(
                "数据库及锁文件须为当前用户的 0600 单链接普通文件",
            ));
        }
        Ok(())
    }

    pub fn lock(&self) -> Result<File> {
        let lock = self.file("writer.lock", false)?;
        // fork/exec 在 CLOEXEC 生效前可能短暂持有继承的锁 fd；只作有界等待。
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        loop {
            match rfs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(lock)
    }

    pub fn check_sidecars(&self) -> Result<()> {
        for name in [
            "state.sqlite3-journal",
            "state.sqlite3-wal",
            "state.sqlite3-shm",
        ] {
            match rfs::openat(
                &self.0,
                name,
                OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => Self::validate_file(&File::from(fd))?,
                Err(rustix::io::Errno::NOENT) => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}
