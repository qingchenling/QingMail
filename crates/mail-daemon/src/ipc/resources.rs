//! 有界进程间资源：只接受不透明标识，不接受客户端路径。
use super::rpc_error;
use mail_core::AccountId;
use mail_protocol::*;
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 1024;
const TTL: Duration = Duration::from_secs(600);
struct Entry {
    touched: Instant,
    data: Data,
}
#[derive(Clone)]
pub(super) struct PartSpec {
    pub location: mail_core::MailUid,
    pub folder: String,
    pub part: mail_core::MimePart,
}
pub(super) struct DownloadProgress {
    pub cancel: AtomicBool,
    pub received: AtomicU64,
}
enum Data {
    Download {
        spec: PartSpec,
        progress: Arc<DownloadProgress>,
        file: Option<File>,
        state: ResourceState,
        error: Option<String>,
    },
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
            Data::Download { .. } => 0,
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
    pub fn downloading_bytes(&self) -> u64 {
        self.entries
            .values()
            .filter_map(|entry| match &entry.data {
                Data::Download {
                    progress,
                    state: ResourceState::Downloading,
                    ..
                } => Some(progress.received.load(Ordering::Acquire)),
                _ => None,
            })
            .sum()
    }

    fn reserve(&mut self, size: u64) -> Result<String, RpcError> {
        self.entries.retain(|_, entry| {
            let keep = entry.touched.elapsed() < TTL;
            if !keep && let Data::Download { progress, .. } = &entry.data {
                progress.cancel.store(true, Ordering::Release);
            }
            keep
        });
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
            && let Some(Entry {
                data: Data::Download { progress, .. },
                ..
            }) = self.entries.remove(id)
        {
            progress.cancel.store(true, Ordering::Release);
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
        if length == 0 || length > MAX_CHUNK_BYTES {
            return Err(rpc_error(ErrorCode::InvalidRequest, "资源分块范围无效"));
        }
        let (total, data) = match &mut entry.data {
            Data::Ready(bytes) => {
                if offset > bytes.len() as u64 {
                    return Err(rpc_error(ErrorCode::InvalidRequest, "偏移无效"));
                }
                let end = (offset + u64::from(length)).min(bytes.len() as u64) as usize;
                (bytes.len() as u64, bytes[offset as usize..end].to_vec())
            }
            Data::Download {
                file: Some(file),
                state: ResourceState::Ready,
                ..
            } => {
                let total = file
                    .metadata()
                    .map_err(|e| rpc_error(ErrorCode::Storage, e.to_string()))?
                    .len();
                if offset > total {
                    return Err(rpc_error(ErrorCode::InvalidRequest, "偏移无效"));
                }
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| rpc_error(ErrorCode::Storage, e.to_string()))?;
                let mut bytes = vec![0; u64::from(length).min(total - offset) as usize];
                file.read_exact(&mut bytes)
                    .map_err(|e| rpc_error(ErrorCode::Storage, e.to_string()))?;
                (total, bytes)
            }
            _ => return Err(rpc_error(ErrorCode::Conflict, "资源尚未完整下载")),
        };
        Ok((
            ResourceChunk {
                resource: id.into(),
                offset,
                length: data.len() as u32,
                total,
                complete: offset + data.len() as u64 == total,
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
        if let Some(Entry {
            data:
                Data::Download {
                    progress,
                    state,
                    error,
                    ..
                },
            ..
        }) = self.entries.get_mut(id)
        {
            if *state != ResourceState::Ready {
                progress.cancel.store(true, Ordering::Release);
                *state = ResourceState::Failed;
                *error = Some("下载已取消，可重试".into());
            }
        } else {
            self.entries.remove(id);
        }
    }
    pub fn lazy(&mut self, spec: PartSpec, file: Option<File>) -> Result<ResourceRef, RpcError> {
        let id = self.reserve(0)?;
        let received = if file.is_some() {
            u64::from(spec.part.encoded_size)
        } else {
            0
        };
        let state = if file.is_some() {
            ResourceState::Ready
        } else {
            ResourceState::NotCached
        };
        self.entries.insert(
            id.clone(),
            Entry {
                touched: Instant::now(),
                data: Data::Download {
                    spec,
                    progress: Arc::new(DownloadProgress {
                        cancel: AtomicBool::new(false),
                        received: AtomicU64::new(received),
                    }),
                    file,
                    state,
                    error: None,
                },
            },
        );
        Ok(self.download_status(&id)?.resource)
    }
    pub fn download_status(&mut self, id: &str) -> Result<DownloadInfo, RpcError> {
        let entry = self.entry(id)?;
        match &entry.data {
            Data::Download {
                spec,
                progress,
                file,
                state,
                error,
            } => Ok(DownloadInfo {
                resource: ResourceRef {
                    id: id.into(),
                    size: file
                        .as_ref()
                        .and_then(|f| f.metadata().ok())
                        .map(|m| m.len())
                        .unwrap_or(u64::from(spec.part.encoded_size)),
                    media_type: spec.part.media_type.clone(),
                    sha256: None,
                    state: *state,
                },
                received: progress.received.load(Ordering::Acquire),
                total: u64::from(spec.part.encoded_size),
                error: error.clone(),
            }),
            Data::Ready(bytes) => Ok(DownloadInfo {
                resource: ResourceRef {
                    id: id.into(),
                    size: bytes.len() as u64,
                    media_type: "application/octet-stream".into(),
                    sha256: None,
                    state: ResourceState::Ready,
                },
                received: bytes.len() as u64,
                total: bytes.len() as u64,
                error: None,
            }),
            _ => Err(rpc_error(ErrorCode::Conflict, "上传会话不是接收附件")),
        }
    }
    pub fn begin_download(
        &mut self,
        id: &str,
    ) -> Result<Option<(PartSpec, Arc<DownloadProgress>)>, RpcError> {
        let entry = self.entry(id)?;
        let Data::Download {
            spec,
            progress,
            state,
            error,
            ..
        } = &mut entry.data
        else {
            return Ok(None);
        };
        if matches!(*state, ResourceState::Ready | ResourceState::Downloading) {
            return Ok(None);
        }
        *progress = Arc::new(DownloadProgress {
            cancel: AtomicBool::new(false),
            received: AtomicU64::new(0),
        });
        *error = None;
        *state = ResourceState::Downloading;
        Ok(Some((spec.clone(), progress.clone())))
    }
    pub fn finish_download_if_current(
        &mut self,
        id: &str,
        expected: &Arc<DownloadProgress>,
        result: Result<File, String>,
    ) {
        if matches!(self.entries.get(id),Some(Entry{data:Data::Download{progress,..},..}) if Arc::ptr_eq(progress,expected))
        {
            self.finish_download(id, result);
        }
    }
    pub fn finish_download(&mut self, id: &str, result: Result<File, String>) {
        if let Ok(Entry {
            data:
                Data::Download {
                    progress,
                    file,
                    state,
                    error,
                    ..
                },
            ..
        }) = self.entry(id)
        {
            if progress.cancel.load(Ordering::Acquire) {
                *state = ResourceState::Failed;
                *error = Some("下载已取消".into());
                return;
            }
            match result {
                Ok(value) => {
                    *file = Some(value);
                    *state = ResourceState::Ready;
                    *error = None;
                }
                Err(e) => {
                    *state = ResourceState::Failed;
                    *error = Some(e);
                }
            }
        }
    }
    pub fn cancel_downloads(&mut self) {
        for entry in self.entries.values() {
            if let Data::Download { progress, .. } = &entry.data {
                progress.cancel.store(true, Ordering::Release);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> PartSpec {
        PartSpec {
            location: mail_core::MailUid {
                account: AccountId("a".into()),
                folder_id: "inbox".into(),
                uidvalidity: 42,
                uid: 7,
            },
            folder: "INBOX".into(),
            part: mail_core::MimePart {
                section: "2".into(),
                media_type: "application/octet-stream".into(),
                charset: None,
                encoding: "base64".into(),
                encoded_size: 50 * 1024 * 1024,
                name: Some("large.bin".into()),
                cid: None,
                attachment: true,
            },
        }
    }
    #[test]
    fn deferred_parts_are_unreadable_until_complete_and_cancelled_generations_cannot_publish() {
        use std::io::Write;
        let mut r = Resources::default();
        let reference = r.lazy(spec(), None).unwrap();
        assert_eq!(reference.state, ResourceState::NotCached);
        assert!(r.read(&reference.id, 0, 1).is_err());
        let (_, old) = r.begin_download(&reference.id).unwrap().unwrap();
        old.received.store(5, Ordering::Release);
        assert_eq!(r.download_status(&reference.id).unwrap().received, 5);
        assert!(r.begin_download(&reference.id).unwrap().is_none());
        r.cancel(&reference.id);
        assert!(old.cancel.load(Ordering::Acquire));
        let (_, current) = r.begin_download(&reference.id).unwrap().unwrap();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"complete").unwrap();
        r.finish_download_if_current(&reference.id, &old, Ok(file));
        assert!(r.read(&reference.id, 0, 1).is_err());
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"complete").unwrap();
        r.finish_download_if_current(&reference.id, &current, Ok(file));
        assert_eq!(r.download_status(&reference.id).unwrap().resource.size, 8);
        assert_eq!(r.read(&reference.id, 0, 3).unwrap().1, b"com");
        let (chunk, bytes) = r.read(&reference.id, 3, 65536).unwrap();
        assert!(chunk.complete);
        assert_eq!(bytes, b"plete");
        assert!(r.read(&reference.id, 9, 1).is_err());
        assert!(r.read(&reference.id, 0, 65537).is_err());
    }

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
