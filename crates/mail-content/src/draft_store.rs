//! P0 单草稿槽：目录 fd 锚定、单写者锁、临时文件 + fsync + 原子替换。
use crate::draft::{Draft, MAX_JSON};
use rustix::fs::{self as rfs, AtFlags, FlockOperation, Mode, OFlags};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

pub struct DraftStore {
    directory: File,
    _lock: File,
}

impl DraftStore {
    /// parent 为调用者提供的已存在可信根；P0 使用工作区 target。
    pub fn open(parent: &Path) -> io::Result<Self> {
        let parent = rfs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        match rfs::mkdirat(&parent, "p0-editor", Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => (),
            Err(e) => return Err(e.into()),
        }
        let directory: File = rfs::openat(
            &parent,
            "p0-editor",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        let m = directory.metadata()?;
        if m.uid() != rustix::process::geteuid().as_raw() || m.mode() & 0o777 != 0o700 {
            return Err(io::Error::other("草稿目录必须属于当前用户且权限为 0700"));
        }
        let lock: File = rfs::openat(
            &directory,
            "writer.lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?
        .into();
        if !lock.metadata()?.is_file() {
            return Err(io::Error::other("无效草稿锁"));
        }
        rfs::flock(&lock, FlockOperation::NonBlockingLockExclusive)?;
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    pub fn load(&self) -> io::Result<Option<Draft>> {
        let file: File = match rfs::openat(
            &self.directory,
            "draft.json",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd.into(),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let m = file.metadata()?;
        if !m.is_file()
            || m.uid() != rustix::process::geteuid().as_raw()
            || m.mode() & 0o777 != 0o600
            || m.len() > MAX_JSON as u64
        {
            return Err(io::Error::other("草稿类型、所有者、权限或大小无效"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_JSON as u64 + 1).read_to_end(&mut bytes)?;
        Draft::from_json(&bytes).map(Some).map_err(io::Error::other)
    }

    pub fn save(&self, draft: &Draft) -> io::Result<()> {
        let bytes = draft.to_json().map_err(io::Error::other)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let name = format!("save-{:032x}.tmp", u128::from_ne_bytes(random));
        let mut file: File = rfs::openat(
            &self.directory,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?
        .into();
        let result = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            rfs::renameat(&self.directory, &name, &self.directory, "draft.json")?;
            self.directory.sync_all()
        })();
        if result.is_err() {
            let _ = rfs::unlinkat(&self.directory, &name, AtFlags::empty());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn durable_reopen_lock_and_symlink_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let store = DraftStore::open(root.path()).unwrap();
        assert!(DraftStore::open(root.path()).is_err());
        assert_eq!(store.load().unwrap(), None);
        let draft = Draft::plain("已确认保存的中文🙂").unwrap();
        store.save(&draft).unwrap();
        drop(store);
        let store = DraftStore::open(root.path()).unwrap();
        assert_eq!(store.load().unwrap().unwrap(), draft);
        let outside = root.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        fs::remove_file(root.path().join("p0-editor/draft.json")).unwrap();
        symlink(&outside, root.path().join("p0-editor/draft.json")).unwrap();
        assert!(store.load().is_err());
        store.save(&draft).unwrap(); // 替换链接本身，不跟随。
        assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
        assert_eq!(store.load().unwrap().unwrap(), draft);
        fs::write(root.path().join("p0-editor/draft.json"), "broken").unwrap();
        assert!(store.load().is_err());
    }
}
