use crate::{MAX_CONTROL_BYTES, PROTOCOL_VERSION, Request};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fmt,
    io::{self, BufRead, Write},
};

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    Json(serde_json::Error),
    TooLarge,
    Truncated,
    Version,
    Invalid,
}
impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Io(_) => "IPC 连接失败或读取超时",
            Self::Json(_) => "IPC 消息格式无效",
            Self::TooLarge => "IPC 消息超过限制",
            Self::Truncated => "IPC 消息未完整接收",
            Self::Version => "前后台协议版本不兼容，请更新并重启 QingMail",
            Self::Invalid => "IPC 请求参数无效",
        })
    }
}
impl std::error::Error for ProtocolError {}
impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for ProtocolError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// 不使用无界 read_line；也不吞掉紧跟控制帧的二进制资源数据。
pub fn read_frame(reader: &mut impl BufRead) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Err(ProtocolError::Truncated);
        }
        let end = available.iter().position(|b| *b == b'\n');
        let take = end.map_or(available.len(), |i| i + 1);
        if bytes.len() + take > MAX_CONTROL_BYTES {
            return Err(ProtocolError::TooLarge);
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if end.is_some() {
            return Ok(bytes);
        }
    }
}
pub fn read_json<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<T, ProtocolError> {
    Ok(serde_json::from_slice(&read_frame(reader)?)?)
}
pub fn read_request(reader: &mut impl BufRead) -> Result<Request, ProtocolError> {
    let raw: serde_json::Value = read_json(reader)?;
    // 即使新版本带有旧版本未知命令，也首先报告版本问题，不执行任何写操作。
    if raw.get("version").and_then(|v| v.as_u64()) != Some(u64::from(PROTOCOL_VERSION)) {
        return Err(ProtocolError::Version);
    }
    let request: Request = serde_json::from_value(raw)?;
    request.validate()?;
    Ok(request)
}
pub fn write_json(writer: &mut impl Write, message: &impl Serialize) -> Result<(), ProtocolError> {
    let bytes = serde_json::to_vec(message)?;
    if bytes.len() + 1 > MAX_CONTROL_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use std::io::{Cursor, Read};
    #[test]
    fn version_checked_before_command_dispatch() {
        let mut data = Cursor::new(b"{\"version\":99,\"command\":{\"type\":\"future_write\"}}\n");
        assert!(matches!(
            read_request(&mut data),
            Err(ProtocolError::Version)
        ));
        let request = Request::new("a".repeat(32), Command::Accounts);
        let mut data = Vec::new();
        write_json(&mut data, &request).unwrap();
        assert!(matches!(
            read_request(&mut Cursor::new(data)).unwrap().command,
            Command::Accounts
        ));
    }
    #[test]
    fn control_frame_does_not_consume_resource_bytes() {
        let mut data = Cursor::new(b"{}\n\x00\xff\n");
        let _: serde_json::Value = read_json(&mut data).unwrap();
        let mut bytes = Vec::new();
        data.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"\x00\xff\n");
        assert!(matches!(
            read_frame(&mut Cursor::new(vec![b'a'; MAX_CONTROL_BYTES + 1])),
            Err(ProtocolError::TooLarge)
        ));
        assert!(matches!(
            read_frame(&mut Cursor::new(b"{}")),
            Err(ProtocolError::Truncated)
        ));
    }
    #[test]
    fn arbitrary_paths_credentials_unknown_fields_and_unbounded_reads_rejected() {
        let id = "a".repeat(32);
        let mut raw = serde_json::to_value(Request::new(id.clone(), Command::Accounts)).unwrap();
        raw["password"] = "secret".into();
        assert!(serde_json::from_value::<Request>(raw).is_err());
        for resource in ["/etc/passwd", "https://host/private", "../escape"] {
            assert!(
                Request::new(
                    id.clone(),
                    Command::ReadResource {
                        resource: resource.into(),
                        offset: 0,
                        length: 1
                    }
                )
                .validate()
                .is_err()
            );
        }
        assert!(
            Request::new(
                id,
                Command::UnifiedInbox {
                    before: None,
                    limit: 201
                }
            )
            .validate()
            .is_err()
        );
    }
}
