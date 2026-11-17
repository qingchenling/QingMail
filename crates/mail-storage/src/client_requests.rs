//! 前台请求恢复书签，独立于后台 SQLite；只含请求 ID 与操作类别，无秘密。
use crate::{Error, Result, private_dir::PrivateDir};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    sync::Mutex,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingAccountRequest {
    pub id: String,
    pub updated: bool,
}
pub struct ClientRequests {
    directory: PrivateDir,
    _writer: File,
    serial: Mutex<()>,
}
impl ClientRequests {
    pub fn open(path: &Path) -> Result<Self> {
        let directory = PrivateDir::open(path)?;
        let writer = directory.lock()?;
        Ok(Self {
            directory,
            _writer: writer,
            serial: Mutex::new(()),
        })
    }
    pub fn load(&self) -> Result<Option<PendingAccountRequest>> {
        let _guard = self.serial.lock().unwrap_or_else(|e| e.into_inner());
        self.read()
    }
    fn read(&self) -> Result<Option<PendingAccountRequest>> {
        let file = match self.directory.read("account-request") {
            Ok(file) => file,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if file.metadata()?.len() > 64 {
            return Err(Error::Invalid("账号请求恢复记录过大"));
        }
        let mut text = String::new();
        file.take(65).read_to_string(&mut text)?;
        if text == "none\n" {
            return Ok(None);
        }
        let parts: Vec<_> = text.trim_end_matches('\n').split(':').collect();
        match parts.as_slice() {
            ["v1", kind @ ("new" | "update"), id] if mail_protocol::valid_id(id) => {
                Ok(Some(PendingAccountRequest {
                    id: (*id).into(),
                    updated: *kind == "update",
                }))
            }
            _ => Err(Error::Invalid("账号请求恢复记录损坏，已保留原件")),
        }
    }
    pub fn remember(&self, pending: &PendingAccountRequest) -> Result<()> {
        if !mail_protocol::valid_id(&pending.id) {
            return Err(Error::Invalid("请求标识无效"));
        }
        let _guard = self.serial.lock().unwrap_or_else(|e| e.into_inner());
        if self.read()?.is_some() {
            return Err(Error::Conflict);
        }
        self.write(&format!(
            "v1:{}:{}\n",
            if pending.updated { "update" } else { "new" },
            pending.id
        ))
    }
    /// 已确认结果或用户明确核对后才能结束；未知结果不能自动清除。
    pub fn clear(&self, id: &str) -> Result<()> {
        let _guard = self.serial.lock().unwrap_or_else(|e| e.into_inner());
        if self.read()?.is_some_and(|p| p.id != id) {
            return Err(Error::Conflict);
        }
        self.write("none\n")
    }
    fn write(&self, text: &str) -> Result<()> {
        use rustix::fs::renameat;
        let name = format!("request-{}.tmp", mail_protocol::transport::request_id()?);
        let mut file = self.directory.file(&name, true)?;
        let result = (|| {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            renameat(
                &self.directory.0,
                &name,
                &self.directory.0,
                "account-request",
            )?;
            self.directory.0.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            self.directory.remove(&name);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_bookmark_survives_restart_and_refuses_overwrite_wrong_clear_or_corruption() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("client");
        let store = ClientRequests::open(&path).unwrap();
        let pending = PendingAccountRequest {
            id: "a".repeat(32),
            updated: true,
        };
        assert!(store.load().unwrap().is_none());
        store.remember(&pending).unwrap();
        assert!(store.remember(&pending).is_err());
        assert!(store.clear(&"b".repeat(32)).is_err());
        drop(store);
        let store = ClientRequests::open(&path).unwrap();
        assert_eq!(store.load().unwrap(), Some(pending.clone()));
        store.clear(&pending.id).unwrap();
        assert!(store.load().unwrap().is_none());
        let mut file = store.directory.file("account-request", false).unwrap();
        file.set_len(0).unwrap();
        file.write_all(b"corrupt").unwrap();
        assert!(store.load().is_err());
        assert!(store.remember(&pending).is_err());
        assert!(store.clear(&pending.id).is_err());
        assert_eq!(
            std::fs::read(path.join("account-request")).unwrap(),
            b"corrupt"
        );
    }
}
