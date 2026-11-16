//! 使用目录 fd 锚定操作，避免跟随副本文件/目录的符号链接。
use rustix::fs::{self as rfs, AtFlags, Mode, OFlags};
use std::{
    fs::{self, File},
    io::{self, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::MailContent;

pub struct CopyStore {
    directory: File,
    path: PathBuf,
}

impl CopyStore {
    /// parent 是可信应用缓存根目录；只在其下管理固定的 reading-copies 子目录。
    pub fn open(parent: &Path) -> io::Result<Self> {
        let parent_fd = rfs::open(
            parent,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        match rfs::mkdirat(&parent_fd, "reading-copies", Mode::from_raw_mode(0o700)) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        let directory: File = rfs::openat(
            &parent_fd,
            "reading-copies",
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        let metadata = directory.metadata()?;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o777 != 0o700
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "阅读副本目录必须属于当前用户且权限为 0700",
            ));
        }
        let store = Self {
            directory,
            path: parent.join("reading-copies"),
        };
        store.cleanup_at(SystemTime::now(), false)?;
        Ok(store)
    }

    pub fn write(&self, content: &MailContent) -> io::Result<PathBuf> {
        self.write_html(&content.browser_html())
    }

    fn write_html(&self, html: &str) -> io::Result<PathBuf> {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let name = format!(
            "qingmail-{}.html",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let mut file: File = rfs::openat(
            &self.directory,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?
        .into();
        let written = file
            .write_all(html.as_bytes())
            .and_then(|()| file.sync_all());
        if let Err(error) = written {
            let _ = rfs::unlinkat(&self.directory, &name, AtFlags::empty());
            return Err(error);
        }
        Ok(self.path.join(name))
    }

    /// 用户主动清理；仅删除本应用命名的常规文件，不递归、不跟随链接。
    pub fn clear(&self) -> io::Result<usize> {
        self.cleanup_at(SystemTime::now(), true)
    }

    pub fn cleanup_at(&self, now: SystemTime, immediately: bool) -> io::Result<usize> {
        let mut removed = 0;
        // /proc/self/fd 将枚举锚定在已打开的目录；重命名或路径替换不会转向其他目录。
        use std::os::fd::AsRawFd;
        for entry in fs::read_dir(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| owned_name(name)) else {
                continue;
            };
            let file: File = match rfs::openat(
                &self.directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd.into(),
                Err(_) => continue,
            };
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.uid() != rustix::process::geteuid().as_raw() {
                continue;
            }
            let expired = now
                .duration_since(metadata.modified()?)
                .is_ok_and(|age| age >= Duration::from_secs(24 * 60 * 60));
            if immediately || expired {
                match rfs::unlinkat(&self.directory, name, AtFlags::empty()) {
                    Ok(()) => removed += 1,
                    Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(removed)
    }
}

fn owned_name(name: &str) -> bool {
    name.strip_prefix("qingmail-")
        .and_then(|name| name.strip_suffix(".html"))
        .is_some_and(|random| {
            random.len() == 32 && random.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{PermissionsExt, symlink},
    };

    #[test]
    fn permissions_expiry_and_symlinks_are_bounded_to_managed_files() {
        let root = tempfile::tempdir().unwrap();
        let store = CopyStore::open(root.path()).unwrap();
        let content = crate::parse(b"Subject: copy\r\n\r\nHello").unwrap();
        let path = store.write(&content).unwrap();
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let outside = root.path().join("keep.txt");
        fs::write(&outside, "keep").unwrap();
        let link = store
            .path
            .join("qingmail-00000000000000000000000000000000.html");
        symlink(&outside, &link).unwrap();
        let unrelated = store.path.join("unrelated.html");
        fs::write(&unrelated, "keep").unwrap();
        assert_eq!(store.cleanup_at(SystemTime::now(), false).unwrap(), 0);
        assert_eq!(
            store
                .cleanup_at(SystemTime::now() + Duration::from_secs(86_401), false)
                .unwrap(),
            1
        );
        assert!(
            outside.exists() && unrelated.exists() && link.symlink_metadata().unwrap().is_symlink()
        );
        let first = store.write(&content).unwrap();
        let second = store.write(&content).unwrap();
        assert_ne!(first.as_os_str().as_bytes(), second.as_os_str().as_bytes());
        assert_eq!(store.clear().unwrap(), 2);
        drop(store);
        // Simulates a leftover file after an interrupted process; open() expires it on next launch.
        let store = CopyStore::open(root.path()).unwrap();
        let stale = store.write(&content).unwrap();
        File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
        drop(store);
        CopyStore::open(root.path()).unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn refuses_symlink_directory_and_insecure_existing_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("reading-copies")).unwrap();
        assert!(CopyStore::open(root.path()).is_err());
        fs::remove_file(root.path().join("reading-copies")).unwrap();
        fs::create_dir(root.path().join("reading-copies")).unwrap();
        fs::set_permissions(
            root.path().join("reading-copies"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(CopyStore::open(root.path()).is_err());
    }
}
