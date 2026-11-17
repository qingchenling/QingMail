//! 服务端 MIME 结构描述；section 与完整邮件身份共同定位资源。
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MimePart {
    pub section: String,
    pub media_type: String,
    pub charset: Option<String>,
    pub encoding: String,
    pub encoded_size: u32,
    pub name: Option<String>,
    pub cid: Option<String>,
    pub attachment: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MimeStructure {
    pub total_size: u32,
    pub headers: Vec<u8>,
    pub parts: Vec<MimePart>,
}
impl MimePart {
    pub fn valid(&self) -> bool {
        self.section.len() <= 128
            && !self.section.is_empty()
            && self
                .section
                .split('.')
                .all(|s| s.parse::<u32>().is_ok_and(|n| n > 0))
            && self.media_type.len() <= 128
            && self.media_type.split('/').count() == 2
            && self.media_type.split('/').all(token)
            && token(&self.encoding)
            && self.charset.as_deref().is_none_or(token)
            && self.name.as_ref().is_none_or(|s| s.len() <= 1024)
            && self
                .cid
                .as_ref()
                .is_none_or(|s| s.len() <= 1024 && !s.chars().any(char::is_control))
    }
}
fn token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
}
impl MimeStructure {
    pub fn valid(&self) -> bool {
        self.headers.len() <= 64 * 1024
            && !self.parts.is_empty()
            && self.parts.len() <= 256
            && self.parts.iter().all(MimePart::valid)
            && {
                let mut seen = std::collections::HashSet::new();
                self.parts.iter().all(|p| seen.insert(&p.section))
            }
    }
}
