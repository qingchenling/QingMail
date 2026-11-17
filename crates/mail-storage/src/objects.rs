use std::io::{Read, Write};

use mail_core::AccountId;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use crate::{Error, PrivateDir, Result, new_id};

pub(crate) struct Object {
    pub id: String,
    pub account: AccountId,
    pub kind: &'static str,
    pub size: u32,
    pub hash: String,
}

impl Object {
    /// 独占创建不可变对象；先同步文件与目录，再允许数据库关联它。
    pub fn write(
        directory: &PrivateDir,
        account: &AccountId,
        kind: &'static str,
        reader: &mut impl Read,
        max: u64,
    ) -> Result<Self> {
        let id = new_id()?;
        let mut file = directory.file(&id, true)?;
        let result = (|| {
            let mut size = 0;
            let mut hash = Sha256::new();
            let mut buffer = [0; 64 * 1024];
            loop {
                let n = reader.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                if size > max {
                    return Err(Error::Invalid("对象超过大小上限"));
                }
                file.write_all(&buffer[..n])?;
                hash.update(&buffer[..n]);
            }
            file.sync_all()?;
            directory.0.sync_all()?;
            Ok(Self {
                id: id.clone(),
                account: account.clone(),
                kind,
                size: size as u32,
                hash: format!("{:x}", hash.finalize()),
            })
        })();
        if result.is_err() {
            directory.remove(&id);
        }
        result
    }

    pub fn insert(&self, connection: &Connection) -> Result<()> {
        connection.execute(
            "INSERT INTO objects VALUES (?1,?2,?3,?4,?5)",
            params![self.id, self.account.0, self.kind, self.size, self.hash],
        )?;
        Ok(())
    }
}

pub(crate) fn read(
    directory: &PrivateDir,
    connection: &Connection,
    account: &AccountId,
    id: &str,
    kind: &str,
    max: u64,
) -> Result<Vec<u8>> {
    // ID 来自数据库也必须检查，绝不把它当任意路径。
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Invalid("无效对象 ID"));
    }
    let (size, hash): (u32, String) = connection.query_row(
        "SELECT size,sha256 FROM objects WHERE id=?1 AND account_id=?2 AND kind=?3",
        params![id, account.0, kind],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if u64::from(size) > max {
        return Err(Error::Invalid("对象超过读取上限"));
    }
    let file = directory.read(id)?;
    if file.metadata()?.len() != u64::from(size) {
        return Err(Error::Invalid("对象大小不符，保留原文件"));
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != u64::from(size) || format!("{:x}", Sha256::digest(&bytes)) != hash {
        return Err(Error::Invalid("对象完整性校验失败，保留原文件"));
    }
    connection.execute(
        "UPDATE cache_access SET accessed_at=unixepoch() WHERE object_id=?1",
        [id],
    )?;
    Ok(bytes)
}
