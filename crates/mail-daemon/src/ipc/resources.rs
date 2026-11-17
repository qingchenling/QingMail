//! 有界进程间资源：只接受不透明标识，不接受客户端路径。
use super::rpc_error;
use mail_core::AccountId;
use mail_protocol::*;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;
const TTL: Duration = Duration::from_secs(600);
struct Entry {
    touched: Instant,
    data: Data,
}
enum Data {
    Ready(Arc<[u8]>),
    Upload {
        account: AccountId,
        name: String,
        media_type: String,
        size: u64,
        bytes: Vec<u8>,
    },
}
impl Entry {
    fn size(&self) -> u64 {
        match &self.data {
            Data::Ready(bytes) => bytes.len() as u64,
            Data::Upload { size, .. } => *size,
        }
    }
}
pub(super) struct Upload {
    pub account: AccountId,
    pub name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}
#[derive(Default)]
pub(super) struct Resources {
    entries: HashMap<String, Entry>,
}
impl Resources {
    fn reserve(&mut self, size: u64) -> Result<String, RpcError> {
        self.entries
            .retain(|_, entry| entry.touched.elapsed() < TTL);
        if self.entries.len() >= MAX_ENTRIES
            || size > MAX_BYTES.saturating_sub(self.entries.values().map(Entry::size).sum())
        {
            return Err(rpc_error(
                ErrorCode::Busy,
                "资源通道已满，请取消不再使用的资源或稍后重试",
            ));
        }
        transport::request_id().map_err(|e| rpc_error(ErrorCode::Internal, e.to_string()))
    }
    pub fn insert(
        &mut self,
        bytes: Arc<[u8]>,
        media_type: String,
    ) -> Result<ResourceRef, RpcError> {
        let size = bytes.len() as u64;
        let id = self.reserve(size)?;
        self.entries.insert(
            id.clone(),
            Entry {
                touched: Instant::now(),
                data: Data::Ready(bytes),
            },
        );
        Ok(ResourceRef {
            id,
            size,
            media_type,
            sha256: None,
            state: ResourceState::Ready,
        })
    }
    pub fn upload(
        &mut self,
        account: AccountId,
        name: String,
        media_type: String,
        size: u64,
    ) -> Result<Reply, RpcError> {
        let id = self.reserve(size)?;
        self.entries.insert(
            id.clone(),
            Entry {
                touched: Instant::now(),
                data: Data::Upload {
                    account,
                    name,
                    media_type: media_type.clone(),
                    size,
                    bytes: Vec::new(),
                },
            },
        );
        Ok(Reply::ResourceCreated(ResourceRef {
            id,
            size,
            media_type,
            sha256: None,
            state: ResourceState::Downloading,
        }))
    }
    fn entry(&mut self, id: &str) -> Result<&mut Entry, RpcError> {
        if self
            .entries
            .get(id)
            .is_some_and(|e| e.touched.elapsed() >= TTL)
        {
            self.entries.remove(id);
        }
        let entry = self
            .entries
            .get_mut(id)
            .ok_or_else(|| rpc_error(ErrorCode::NotFound, "资源已失效，请重新读取或上传"))?;
        entry.touched = Instant::now();
        Ok(entry)
    }
    pub fn read(
        &mut self,
        id: &str,
        offset: u64,
        length: u32,
    ) -> Result<(ResourceChunk, Vec<u8>), RpcError> {
        let entry = self.entry(id)?;
        let Data::Ready(bytes) = &entry.data else {
            return Err(rpc_error(ErrorCode::Conflict, "资源尚未完成"));
        };
        if length == 0 || length > MAX_CHUNK_BYTES || offset > bytes.len() as u64 {
            return Err(rpc_error(ErrorCode::InvalidRequest, "资源分块范围无效"));
        }
        let end = (offset + u64::from(length)).min(bytes.len() as u64) as usize;
        let data = bytes[offset as usize..end].to_vec();
        Ok((
            ResourceChunk {
                resource: id.into(),
                offset,
                length: data.len() as u32,
                total: bytes.len() as u64,
                complete: end == bytes.len(),
            },
            data,
        ))
    }
    pub fn write(&mut self, id: &str, offset: u64, input: &[u8]) -> Result<Reply, RpcError> {
        let entry = self.entry(id)?;
        let Data::Upload { size, bytes, .. } = &mut entry.data else {
            return Err(rpc_error(ErrorCode::Conflict, "资源不可写"));
        };
        if input.is_empty()
            || input.len() > MAX_CHUNK_BYTES as usize
            || offset != bytes.len() as u64
            || offset
                .checked_add(input.len() as u64)
                .is_none_or(|end| end > *size)
        {
            return Err(rpc_error(
                ErrorCode::InvalidRequest,
                "上传偏移或长度无效，未修改已接收数据",
            ));
        }
        bytes.extend_from_slice(input);
        Ok(Reply::Acknowledged)
    }
    pub fn complete(&mut self, id: &str) -> Result<Upload, RpcError> {
        let entry = self.entry(id)?;
        if !matches!(&entry.data, Data::Upload{size,bytes,..} if *size==bytes.len() as u64) {
            return Err(rpc_error(ErrorCode::Conflict, "附件尚未完整上传"));
        }
        let Entry {
            data:
                Data::Upload {
                    account,
                    name,
                    media_type,
                    bytes,
                    ..
                },
            ..
        } = self.entries.remove(id).unwrap()
        else {
            unreachable!()
        };
        Ok(Upload {
            account,
            name,
            media_type,
            bytes,
        })
    }
    pub fn cancel(&mut self, id: &str) {
        self.entries.remove(id);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_wrong_offset_and_overflow_never_finish_attachment() {
        let mut r = Resources::default();
        let Reply::ResourceCreated(resource) = r
            .upload(
                AccountId("a".into()),
                "a.txt".into(),
                "text/plain".into(),
                3,
            )
            .unwrap()
        else {
            panic!()
        };
        assert!(r.complete(&resource.id).is_err());
        assert!(r.write(&resource.id, u64::MAX, b"a").is_err());
        r.write(&resource.id, 0, b"ab").unwrap();
        assert!(r.write(&resource.id, 0, b"x").is_err());
        assert!(r.write(&resource.id, 2, b"cd").is_err());
        assert!(r.complete(&resource.id).is_err());
        r.write(&resource.id, 2, b"c").unwrap();
        assert_eq!(r.complete(&resource.id).unwrap().bytes, b"abc");
        assert!(r.complete(&resource.id).is_err());
    }
    #[test]
    fn reads_are_bounded_and_expired_or_cancelled_ids_are_not_reused() {
        let mut r = Resources::default();
        let v = r
            .insert(Arc::from(&b"abc"[..]), "text/plain".into())
            .unwrap();
        let (chunk, bytes) = r.read(&v.id, 1, MAX_CHUNK_BYTES).unwrap();
        assert_eq!(bytes, b"bc");
        assert!(chunk.complete);
        assert!(r.read(&v.id, u64::MAX, 1).is_err());
        r.entries.get_mut(&v.id).unwrap().touched = Instant::now() - TTL;
        assert!(r.read(&v.id, 0, 1).is_err());
        let v = r.insert(Arc::from(&b"x"[..]), "text/plain".into()).unwrap();
        r.cancel(&v.id);
        assert!(r.read(&v.id, 0, 1).is_err());
        assert!(r.reserve(MAX_BYTES + 1).is_err());
    }
}
