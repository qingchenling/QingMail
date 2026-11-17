//! 只读 BODYSTRUCTURE/section 读取；每个literal有界，附件增量解码到调用方writer。
use crate::{
    accounts::{CheckError, ImapStream, authenticated, quoted},
    sync::{command, ensure_epoch, logout, until_done},
};
use imap_proto::{
    AttributeValue as Attr, BodyStructure as Body, ContentEncoding, MessageSection, Response,
    ResponseCode, SectionPath,
};
use mail_core::{MimePart, MimeStructure, ServerSettings};
use std::io::Write;
const CHUNK: u32 = 64 * 1024;
pub const MAX_RECEIVED_PART: u64 = 1024 * 1024 * 1024;
fn connector() -> Result<native_tls::TlsConnector, CheckError> {
    native_tls::TlsConnector::builder()
        .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
        .build()
        .map_err(|_| CheckError::Tls)
}
fn open(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    epoch: u32,
    uid: u32,
    tls: &native_tls::TlsConnector,
) -> Result<ImapStream, CheckError> {
    if folder.is_empty()
        || folder.len() > 4096
        || folder.chars().any(char::is_control)
        || epoch == 0
        || uid == 0
    {
        return Err(CheckError::Invalid);
    }
    let mut s = authenticated(server, password, tls)?;
    let mut cmd = b"QM3 EXAMINE ".to_vec();
    quoted(&mut cmd, folder);
    cmd.extend_from_slice(b"\r\n");
    command(&mut s, &cmd)?;
    let mut seen = false;
    until_done(&mut s, "QM3", |r| {
        ensure_epoch(&r, epoch)?;
        if matches!(
            r,
            Response::Data {
                outcome: imap_proto::Outcome {
                    code: Some(ResponseCode::UidValidity(_)),
                    ..
                },
                ..
            }
        ) {
            seen = true;
        }
        Ok(())
    })?;
    if !seen {
        return Err(CheckError::Protocol);
    }
    Ok(s)
}
/// 结构解析失败时仍有独立大小证据，只有小邮件可回退完整MIME。
pub fn size(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    epoch: u32,
    uid: u32,
) -> Result<u32, CheckError> {
    size_using(server, password, folder, epoch, uid, &connector()?)
}
fn size_using(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    epoch: u32,
    uid: u32,
    tls: &native_tls::TlsConnector,
) -> Result<u32, CheckError> {
    let mut s = open(server, password, folder, epoch, uid, tls)?;
    command(
        &mut s,
        format!("QM4 UID FETCH {uid} (UID RFC822.SIZE)\r\n").as_bytes(),
    )?;
    let mut size = None;
    until_done(&mut s, "QM4", |r| {
        ensure_epoch(&r, epoch)?;
        if let Response::Fetch(_, attrs) = r {
            let mut id = None;
            let mut n = None;
            for a in attrs {
                match a {
                    Attr::Uid(v) => id = Some(v),
                    Attr::Rfc822Size(v) => n = Some(v),
                    _ => (),
                }
            }
            if n.is_some() {
                if id != Some(uid) || size.is_some() {
                    return Err(CheckError::Protocol);
                }
                size = n;
            }
        }
        Ok(())
    })?;
    logout(&mut s);
    size.ok_or(CheckError::Protocol)
}
pub fn structure(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    epoch: u32,
    uid: u32,
    total_size: u32,
) -> Result<MimeStructure, CheckError> {
    structure_using(
        server,
        password,
        folder,
        epoch,
        uid,
        total_size,
        &connector()?,
    )
}
fn structure_using(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    epoch: u32,
    uid: u32,
    total_size: u32,
    tls: &native_tls::TlsConnector,
) -> Result<MimeStructure, CheckError> {
    let mut s = open(server, password, folder, epoch, uid, tls)?;
    command(
        &mut s,
        format!("QM4 UID FETCH {uid} (UID BODYSTRUCTURE BODY.PEEK[HEADER])\r\n").as_bytes(),
    )?;
    let mut result = None;
    until_done(&mut s, "QM4", |r| {
        ensure_epoch(&r, epoch)?;
        if let Response::Fetch(_, attrs) = r {
            let mut id = None;
            let mut body = None;
            let mut header = None;
            for a in attrs {
                match a {
                    Attr::Uid(v) => id = Some(v),
                    Attr::BodyStructure(b) => {
                        if body.replace(b).is_some() {
                            return Err(CheckError::Protocol);
                        }
                    }
                    Attr::BodySection {
                        section: Some(SectionPath::Full(MessageSection::Header)),
                        index: None,
                        data: Some(bytes),
                    } => header = Some(bytes.into_owned()),
                    _ => return Err(CheckError::Protocol),
                }
            }
            if let Some(body) = body {
                if id != Some(uid) || result.is_some() {
                    return Err(CheckError::Protocol);
                }
                let mut parts = vec![];
                flatten(&body, "", 0, &mut parts)?;
                let value = MimeStructure {
                    total_size,
                    headers: header.ok_or(CheckError::Protocol)?,
                    parts,
                };
                if !value.valid() {
                    return Err(CheckError::Protocol);
                }
                result = Some(value);
            }
        }
        Ok(())
    })?;
    logout(&mut s);
    result.ok_or(CheckError::Protocol)
}
fn flatten(
    body: &Body<'_>,
    prefix: &str,
    depth: usize,
    out: &mut Vec<MimePart>,
) -> Result<(), CheckError> {
    if depth > 12 || out.len() >= 256 {
        return Err(CheckError::Limit);
    }
    match body {
        Body::Multipart { common, bodies, .. } => {
            if common
                .disposition
                .as_ref()
                .is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"))
            {
                return Err(CheckError::Protocol);
            }
            if bodies.is_empty() {
                return Err(CheckError::Protocol);
            }
            for (i, b) in bodies.iter().enumerate() {
                let section = if prefix.is_empty() {
                    (i + 1).to_string()
                } else {
                    format!("{prefix}.{}", i + 1)
                };
                flatten(b, &section, depth + 1, out)?;
            }
        }
        Body::Basic { common, other, .. }
        | Body::Text { common, other, .. }
        | Body::Message { common, other, .. } => {
            let param = |params: &imap_proto::BodyParams<'_>, key: &str| {
                params
                    .as_ref()
                    .and_then(|p| p.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)))
                    .map(|(_, v)| (*v).to_string())
            };
            let name = common
                .disposition
                .as_ref()
                .and_then(|d| param(&d.params, "filename"))
                .or_else(|| param(&common.ty.params, "name"));
            let encoding = match &other.transfer_encoding {
                ContentEncoding::Base64 => "base64",
                ContentEncoding::QuotedPrintable => "quoted-printable",
                ContentEncoding::SevenBit => "7bit",
                ContentEncoding::EightBit => "8bit",
                ContentEncoding::Binary => "binary",
                ContentEncoding::Other(s) => s,
            }
            .to_lowercase();
            let p = MimePart {
                section: if prefix.is_empty() {
                    "1".into()
                } else {
                    prefix.into()
                },
                media_type: format!(
                    "{}/{}",
                    common.ty.ty.to_lowercase(),
                    common.ty.subtype.to_lowercase()
                ),
                charset: param(&common.ty.params, "charset"),
                encoding,
                encoded_size: other.octets,
                name: name.clone(),
                cid: other.id.as_deref().map(str::to_string),
                attachment: name.is_some()
                    || common
                        .disposition
                        .as_ref()
                        .is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"))
                    || matches!(body, Body::Message { .. }),
            };
            if !p.valid() {
                return Err(CheckError::Protocol);
            }
            out.push(p);
            // message/rfc822是附件边界，不把内层CID/正文提升到外层邮件。
        }
    }
    Ok(())
}
/// progress返回false取消。encoded/decoded均有上限；调用方只有完整成功才发布缓存。
#[allow(clippy::too_many_arguments)]
pub fn fetch(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    epoch: u32,
    uid: u32,
    part: &MimePart,
    writer: &mut impl Write,
    max: u64,
    progress: impl FnMut(u64) -> bool,
) -> Result<u64, CheckError> {
    fetch_using(
        server,
        password,
        folder,
        epoch,
        uid,
        part,
        writer,
        max,
        progress,
        &connector()?,
    )
}
#[allow(clippy::too_many_arguments)]
fn fetch_using(
    server: &ServerSettings,
    password: &str,
    folder: &str,
    epoch: u32,
    uid: u32,
    part: &MimePart,
    writer: &mut impl Write,
    max: u64,
    mut progress: impl FnMut(u64) -> bool,
    tls: &native_tls::TlsConnector,
) -> Result<u64, CheckError> {
    if !part.valid()
        || u64::from(part.encoded_size) > MAX_RECEIVED_PART.saturating_mul(2)
        || max > MAX_RECEIVED_PART
    {
        return Err(CheckError::Limit);
    }
    let mut decoder = Decoder::new(&part.encoding)?;
    let mut s = open(server, password, folder, epoch, uid, tls)?;
    let path = part
        .section
        .split('.')
        .map(|s| s.parse::<u32>().map_err(|_| CheckError::Invalid))
        .collect::<Result<Vec<_>, _>>()?;
    let mut offset = 0;
    let mut written = 0;
    while offset < part.encoded_size {
        if !progress(u64::from(offset)) {
            return Err(CheckError::Cancelled);
        }
        let length = CHUNK.min(part.encoded_size - offset);
        s.get_mut().reset_budget(192 * 1024, 30);
        command(
            &mut s,
            format!(
                "QM4 UID FETCH {uid} (UID BODY.PEEK[{}]<{offset}.{length}>)\r\n",
                part.section
            )
            .as_bytes(),
        )?;
        let mut bytes = None;
        until_done(&mut s, "QM4", |r| {
            ensure_epoch(&r, epoch)?;
            if let Response::Fetch(_, attrs) = r {
                let mut id = None;
                let mut body = None;
                for a in attrs {
                    match a {
                        Attr::Uid(v) => id = Some(v),
                        Attr::BodySection {
                            section: Some(SectionPath::Part(ref p, None)),
                            index: Some(i),
                            data: Some(data),
                        } if p == &path && i == offset => {
                            if body.replace(data.into_owned()).is_some() {
                                return Err(CheckError::Protocol);
                            }
                        }
                        _ => return Err(CheckError::Protocol),
                    }
                }
                if body.is_some() {
                    if id != Some(uid) || bytes.is_some() {
                        return Err(CheckError::Protocol);
                    }
                    bytes = body;
                }
            }
            Ok(())
        })?;
        let bytes = bytes.ok_or(CheckError::Protocol)?;
        if bytes.len() != length as usize {
            return Err(CheckError::Protocol);
        }
        let decoded = decoder.push(&bytes, false)?;
        written += decoded.len() as u64;
        if written > max {
            return Err(CheckError::Limit);
        }
        writer
            .write_all(&decoded)
            .map_err(|_| CheckError::Storage)?;
        offset += length;
    }
    let decoded = decoder.push(&[], true)?;
    written += decoded.len() as u64;
    if written > max {
        return Err(CheckError::Limit);
    }
    writer
        .write_all(&decoded)
        .map_err(|_| CheckError::Storage)?;
    if !progress(u64::from(offset)) {
        return Err(CheckError::Cancelled);
    }
    logout(&mut s);
    Ok(written)
}
struct Decoder {
    encoding: String,
    carry: Vec<u8>,
    ended: bool,
}
impl Decoder {
    fn new(encoding: &str) -> Result<Self, CheckError> {
        if !matches!(
            encoding,
            "base64" | "quoted-printable" | "7bit" | "8bit" | "binary"
        ) {
            return Err(CheckError::Protocol);
        }
        Ok(Self {
            encoding: encoding.into(),
            carry: vec![],
            ended: false,
        })
    }
    fn push(&mut self, bytes: &[u8], last: bool) -> Result<Vec<u8>, CheckError> {
        use base64::Engine;
        if self.encoding == "base64" {
            let mut result = vec![];
            for b in bytes.iter().copied().filter(|b| !b.is_ascii_whitespace()) {
                if self.ended {
                    return Err(CheckError::Protocol);
                }
                self.carry.push(b);
                if self.carry.len() == 4 {
                    result.extend(
                        base64::engine::general_purpose::STANDARD
                            .decode(&self.carry)
                            .map_err(|_| CheckError::Protocol)?,
                    );
                    self.ended = self.carry.contains(&b'=');
                    self.carry.clear();
                }
            }
            if last && !self.carry.is_empty() {
                return Err(CheckError::Protocol);
            }
            return Ok(result);
        }
        if self.encoding != "quoted-printable" {
            return Ok(bytes.to_vec());
        }
        self.carry.extend_from_slice(bytes);
        let mut out = vec![];
        let mut i = 0;
        while i < self.carry.len() {
            if self.carry[i] != b'=' {
                out.push(self.carry[i]);
                i += 1;
                continue;
            }
            if i + 1 >= self.carry.len() {
                break;
            }
            if self.carry[i + 1] == b'\n' {
                i += 2;
                continue;
            }
            if i + 2 >= self.carry.len() {
                break;
            }
            if &self.carry[i + 1..i + 3] == b"\r\n" {
                i += 3;
                continue;
            }
            let hex = |b: u8| (b as char).to_digit(16).map(|v| v as u8);
            out.push(
                hex(self.carry[i + 1]).ok_or(CheckError::Protocol)? * 16
                    + hex(self.carry[i + 2]).ok_or(CheckError::Protocol)?,
            );
            i += 3;
        }
        self.carry.drain(..i);
        if last && !self.carry.is_empty() {
            return Err(CheckError::Protocol);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::tests::{acceptor, certificate, line, listener, server, socket};
    use std::{
        io::{BufReader, Write},
        thread,
    };
    fn tls(cert: &[u8]) -> native_tls::TlsConnector {
        native_tls::TlsConnector::builder()
            .add_root_certificate(native_tls::Certificate::from_pem(cert).unwrap())
            .build()
            .unwrap()
    }
    fn part(encoded_size: u32, encoding: &str) -> MimePart {
        MimePart {
            section: "2.1".into(),
            media_type: "application/octet-stream".into(),
            charset: None,
            encoding: encoding.into(),
            encoded_size,
            name: Some("large.bin".into()),
            cid: None,
            attachment: true,
        }
    }
    #[test]
    fn incremental_decoders_preserve_split_quartets_soft_breaks_and_reject_corruption() {
        let mut d = Decoder::new("base64").unwrap();
        assert!(d.push(b"5L2", false).unwrap().is_empty());
        let mut result = d.push(b"g5aW9", false).unwrap();
        result.extend(d.push(b"", true).unwrap());
        assert_eq!(String::from_utf8(result).unwrap(), "你好");
        let mut d = Decoder::new("quoted-printable").unwrap();
        let mut result = d.push(b"a=E4=BD", false).unwrap();
        result.extend(d.push(b"=A0=\r", false).unwrap());
        result.extend(d.push(b"\nb=3", false).unwrap());
        result.extend(d.push(b"D", true).unwrap());
        assert_eq!(String::from_utf8(result).unwrap(), "a你b=");
        assert!(Decoder::new("x-unknown").is_err());
        assert!(
            Decoder::new("base64")
                .unwrap()
                .push(b"YQ==X", true)
                .is_err()
        );
        assert!(
            Decoder::new("quoted-printable")
                .unwrap()
                .push(b"a=", true)
                .is_err()
        );
    }
    #[test]
    fn bodystructure_large_attachment_is_metadata_only_and_nested_messages_are_opaque() {
        let (cert, key) = certificate();
        let accept = acceptor(&cert, &key);
        let listener = listener();
        let settings = server(&listener, mail_core::ConnectionSecurity::Tls);
        let worker = thread::spawn(move || {
            let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
            r.get_mut().write_all(b"* OK fixture\r\n").unwrap();
            assert!(line(&mut r).starts_with("QM2 LOGIN "));
            r.get_mut().write_all(b"QM2 OK login\r\n").unwrap();
            assert_eq!(line(&mut r), "QM3 EXAMINE \"INBOX\"\r\n");
            r.get_mut()
                .write_all(b"* OK [UIDVALIDITY 42] epoch\r\nQM3 OK selected\r\n")
                .unwrap();
            assert_eq!(
                line(&mut r),
                "QM4 UID FETCH 7 (UID BODYSTRUCTURE BODY.PEEK[HEADER])\r\n"
            );
            let headers = b"From: fixture@example.invalid\r\nSubject: large attachment\r\n\r\n";
            let structure = "((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 5 1)(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BASE64\" 52428800 NIL (\"ATTACHMENT\" (\"FILENAME\" \"large.bin\"))) \"MIXED\")";
            r.get_mut()
                .write_all(
                    format!(
                        "* 1 FETCH (UID 7 BODYSTRUCTURE {structure} BODY[HEADER] {{{}}}\r\n",
                        headers.len()
                    )
                    .as_bytes(),
                )
                .unwrap();
            r.get_mut().write_all(headers).unwrap();
            r.get_mut().write_all(b")\r\nQM4 OK metadata\r\n").unwrap();
            // 不应出现BODY[2]或BODY[]，即使附件远大于原完整MIME上限。
            assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
        });
        let m = structure_using(
            &settings,
            "fixture",
            "INBOX",
            42,
            7,
            60 * 1024 * 1024,
            &tls(&cert),
        )
        .unwrap();
        assert_eq!(m.parts.len(), 2);
        assert_eq!(m.parts[0].section, "1");
        assert_eq!(m.parts[1].encoded_size, 50 * 1024 * 1024);
        assert!(m.parts[1].attachment);
        assert_eq!(m.parts[1].name.as_deref(), Some("large.bin"));
        worker.join().unwrap();
        let raw=b"* 1 FETCH (UID 7 BODYSTRUCTURE (\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7BIT\" 40 (NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL) (\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 9 1) 1))\r\n";
        let (_, response) = imap_proto::Response::parse(raw).unwrap();
        let Response::Fetch(_, attrs) = response else {
            panic!()
        };
        let Attr::BodyStructure(body) = &attrs[1] else {
            panic!()
        };
        let mut parts = vec![];
        flatten(body, "", 0, &mut parts).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].media_type, "message/rfc822");
        assert!(parts[0].attachment);
    }
    #[test]
    fn section_fetch_streams_bounded_chunks_and_never_requests_other_parts() {
        for mode in [0, 1, 2, 3] {
            let (cert, key) = certificate();
            let accept = acceptor(&cert, &key);
            let listener = listener();
            let settings = server(&listener, mail_core::ConnectionSecurity::Tls);
            let data = vec![b'x'; CHUNK as usize + 17];
            let wire = data.clone();
            let worker = thread::spawn(move || {
                let mut r = BufReader::new(accept.accept(socket(listener)).unwrap());
                r.get_mut().write_all(b"* OK fixture\r\n").unwrap();
                assert!(line(&mut r).starts_with("QM2 LOGIN "));
                r.get_mut().write_all(b"QM2 OK login\r\n").unwrap();
                assert!(line(&mut r).starts_with("QM3 EXAMINE "));
                r.get_mut()
                    .write_all(b"* OK [UIDVALIDITY 42] epoch\r\nQM3 OK selected\r\n")
                    .unwrap();
                assert_eq!(
                    line(&mut r),
                    "QM4 UID FETCH 7 (UID BODY.PEEK[2.1]<0.65536>)\r\n"
                );
                let section = if mode == 2 { "2.2" } else { "2.1" };
                let uid = if mode == 3 { 8 } else { 7 };
                r.get_mut()
                    .write_all(
                        format!("* 1 FETCH (UID {uid} BODY[{section}]<0> {{65536}}\r\n").as_bytes(),
                    )
                    .unwrap();
                r.get_mut().write_all(&wire[..CHUNK as usize]).unwrap();
                r.get_mut().write_all(b")\r\nQM4 OK chunk\r\n").unwrap();
                if mode != 0 {
                    return;
                }
                assert_eq!(
                    line(&mut r),
                    "QM4 UID FETCH 7 (UID BODY.PEEK[2.1]<65536.17>)\r\n"
                );
                r.get_mut()
                    .write_all(b"* 1 FETCH (UID 7 BODY[2.1]<65536> {17}\r\n")
                    .unwrap();
                r.get_mut().write_all(&wire[CHUNK as usize..]).unwrap();
                r.get_mut().write_all(b")\r\nQM4 OK chunk\r\n").unwrap();
                assert_eq!(line(&mut r), "QM9 LOGOUT\r\n");
            });
            let mut out = vec![];
            let mut progress = vec![];
            let result = fetch_using(
                &settings,
                "fixture",
                "INBOX",
                42,
                7,
                &part(data.len() as u32, "binary"),
                &mut out,
                1024 * 1024,
                |n| {
                    progress.push(n);
                    mode != 1 || n == 0
                },
                &tls(&cert),
            );
            if mode == 0 {
                assert_eq!(result, Ok(data.len() as u64));
                assert_eq!(out, data);
                assert_eq!(progress, vec![0, 65536, 65553]);
            } else if mode == 1 {
                assert_eq!(result, Err(CheckError::Cancelled));
                assert_eq!(out.len(), 65536);
            } else {
                assert_eq!(result, Err(CheckError::Protocol));
                assert!(out.is_empty());
            }
            worker.join().unwrap();
        }
    }
}
