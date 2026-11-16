//! 只有显式的单封邮件授权操作才调用；独立客户端不继承邮箱凭据、cookie 或代理。
use reqwest::{blocking::Client, redirect::Policy};
use std::{
    io::Read,
    net::{IpAddr, ToSocketAddrs},
    time::Duration,
};
use url::Url;

use crate::{
    ContentError, MAX_IMAGE_BYTES, MAX_IMAGES, MAX_MESSAGE_BYTES, MailContent, validate_image,
};

impl MailContent {
    /// 调用者必须在用户明确授权当前邮件后于后台执行，避免阻塞 UI。
    /// 仅加载本封 HTML img 的 HTTP(S) 地址；禁止脚本、CSS URL、字体以及私有网络。
    pub fn load_remote_images_after_consent(&mut self) -> Result<usize, ContentError> {
        let mut loaded = 0;
        let mut failures = 0;
        let mut total = self
            .images
            .iter()
            .map(|image| image.png.len())
            .sum::<usize>();
        for source in self.remote_images.clone() {
            if self.images.len() >= MAX_IMAGES || total >= MAX_MESSAGE_BYTES {
                break;
            }
            if self.images.iter().any(|image| image.key == source) {
                continue;
            }
            match fetch(&source)
                .and_then(|(bytes, media_type)| validate_image(&bytes, &media_type, source))
            {
                Ok(image) if total + image.png.len() <= MAX_MESSAGE_BYTES => {
                    total += image.png.len();
                    self.images.push(image);
                    loaded += 1;
                }
                _ => failures += 1,
            }
        }
        if let Some(html) = &self.original_html {
            let cleaned = crate::sanitize::sanitize(html, &self.images)
                .map_err(|reason| ContentError(reason.into()))?;
            self.document = cleaned.document;
            self.browser_body = cleaned.body;
            self.browser_styles = cleaned.styles;
        }
        if failures > 0 {
            self.warnings.push(format!(
                "{failures} 张远程图片因地址、类型或资源限制加载失败"
            ));
        }
        Ok(loaded)
    }
}

fn fetch(source: &str) -> Result<(Vec<u8>, String), ContentError> {
    let failure = || ContentError("远程图片加载失败".into());
    let mut url = Url::parse(source).map_err(|_| failure())?;
    for _ in 0..=3 {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(failure());
        }
        let host = url.host_str().ok_or_else(failure)?;
        let port = url.port_or_known_default().ok_or_else(failure)?;
        let addresses = (host.trim_matches(['[', ']']), port)
            .to_socket_addrs()
            .map_err(|_| failure())?
            .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
            return Err(failure());
        }
        // 每跳都重新验证 DNS 并固定解析结果，避免检查与连接之间的 DNS 重绑定。
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(8))
            .connect_timeout(Duration::from_secs(3))
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| failure())?;
        let mut response = client
            .get(url.clone())
            .header("Accept", "image/png,image/jpeg,image/gif,image/webp")
            .send()
            .map_err(|_| failure())?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(failure)?;
            let next = url.join(location).map_err(|_| failure())?;
            if url.scheme() == "https" && next.scheme() != "https" {
                return Err(failure());
            }
            url = next;
            continue;
        }
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|size| size > MAX_IMAGE_BYTES as u64)
        {
            return Err(failure());
        }
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(
            media_type.split(';').next().unwrap_or("").trim(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            return Err(failure());
        }
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take(MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| failure())?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(failure());
        }
        return Ok((bytes, media_type));
    }
    Err(failure())
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_unspecified()
                && !ip.is_broadcast()
                && !ip.is_documentation()
                && a != 0
                && a < 224
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 192 && b == 0)
                && !(a == 198 && (18..=19).contains(&b))
        }
        IpAddr::V6(ip) => {
            // 仅全局单播，拒绝 IPv4 映射、NAT64、6to4 与保留/文档地址。
            let [first, second, ..] = ip.segments();
            (first & 0xe000) == 0x2000
                && first != 0x2002
                && !(first == 0x2001 && (second <= 0x1ff || second == 0xdb8))
                && first != 0x3fff
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_and_special_networks_are_not_fetch_targets() {
        for ip in [
            "127.0.0.1",
            "0.0.0.0",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.1.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "2001:db8::1",
            "2002:7f00:1::",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
