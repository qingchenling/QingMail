//! 单封邮件图片加载；独立客户端不继承邮箱凭据、cookie 或代理，退出可取消在途读取。
mod dns;

use reqwest::{Client, Response, redirect::Policy};
use std::{
    collections::BTreeMap,
    future::Future,
    net::{IpAddr, SocketAddr},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use url::Url;

use crate::{
    ContentError, MAX_IMAGE_BYTES, MAX_IMAGES, MAX_MESSAGE_BYTES, MailContent, validate_image,
};

const REMOTE_WARNING: &str = "远程图片加载失败：";

impl MailContent {
    /// 调用者必须在自动加载偏好开启或用户明确授权当前邮件后于后台执行，避免阻塞 UI。
    /// 仅加载本封 HTML img 的 HTTP(S) 地址；禁止脚本、CSS URL、字体以及私有网络。
    pub fn load_remote_images_after_consent(&mut self) -> Result<usize, ContentError> {
        self.load_remote_images_with_cancel(&AtomicBool::new(false))
    }

    /// 取消时丢弃在途连接/响应，不等待各张图片的网络超时。
    pub fn load_remote_images_with_cancel(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<usize, ContentError> {
        check_cancel(cancel)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ContentError("图片加载线程初始化失败".into()))?;
        let result = runtime.block_on(self.load_images(cancel));
        // lookup_host 可能已经投递系统 DNS 查询；退出不等待系统解析器返回。
        runtime.shutdown_timeout(Duration::from_millis(25));
        result
    }

    async fn load_images(&mut self, cancel: &AtomicBool) -> Result<usize, ContentError> {
        let mut loaded = 0;
        let mut failures = BTreeMap::<String, usize>::new();
        let mut dns = dns::ImageDns::default();
        let mut total = self
            .images
            .iter()
            .map(|image| image.png.len())
            .sum::<usize>();
        for source in self.remote_images.clone() {
            check_cancel(cancel)?;
            if self.images.len() >= MAX_IMAGES || total >= MAX_MESSAGE_BYTES {
                break;
            }
            if self.images.iter().any(|image| image.key == source) {
                continue;
            }
            let result = cancellable(fetch(&source, &mut dns), cancel).await?;
            let result =
                result.and_then(|(bytes, media_type)| validate_image(&bytes, &media_type, source));
            check_cancel(cancel)?;
            match result {
                Ok(image) if total + image.png.len() <= MAX_MESSAGE_BYTES => {
                    total += image.png.len();
                    self.images.push(image);
                    loaded += 1;
                }
                Ok(_) => {
                    *failures
                        .entry("图片总大小超过本封邮件的加载限制".into())
                        .or_default() += 1
                }
                Err(reason) => *failures.entry(reason.0).or_default() += 1,
            }
        }
        check_cancel(cancel)?;
        // Keep only unresolved URLs so the reader can offer a retry after partial failure.
        self.remote_images
            .retain(|source| !self.images.iter().any(|image| image.key == *source));
        if let Some(html) = &self.original_html {
            let cleaned = crate::sanitize::sanitize(html, &self.images)
                .map_err(|reason| ContentError(reason.into()))?;
            self.document = cleaned.document;
            self.browser_body = cleaned.body;
            self.browser_styles = cleaned.styles;
        }
        self.warnings.retain(|warning| {
            !warning.starts_with(REMOTE_WARNING)
                && !warning.ends_with("张远程图片因地址、类型或资源限制加载失败")
        });
        if !failures.is_empty() {
            let count = failures.values().sum::<usize>();
            let reasons = failures
                .into_iter()
                .take(4)
                .map(|(reason, count)| format!("{reason}（{count} 张）"))
                .collect::<Vec<_>>()
                .join("；");
            self.warnings
                .push(format!("{REMOTE_WARNING}{count} 张。{reasons}"));
        }
        Ok(loaded)
    }
}

async fn fetch(source: &str, dns: &mut dns::ImageDns) -> Result<(Vec<u8>, String), ContentError> {
    let failure = || ContentError("图片地址或重定向无效".into());
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
        let addresses = dns.resolve(host, port).await?;
        // 每跳都重新验证 DNS 并固定解析结果，避免检查与连接之间的 DNS 重绑定。
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(8))
            .connect_timeout(Duration::from_secs(3))
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| failure())?;
        let response = client
            .get(url.clone())
            .header("Accept", "image/png,image/jpeg,image/gif,image/webp")
            .send()
            .await
            .map_err(network_failure)?;
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
        return read_image_response(response).await;
    }
    Err(failure())
}

async fn read_image_response(mut response: Response) -> Result<(Vec<u8>, String), ContentError> {
    if !response.status().is_success() {
        return Err(ContentError(format!(
            "图片服务器返回 HTTP {}",
            response.status().as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_IMAGE_BYTES as u64)
    {
        return Err(ContentError("图片文件超过 2 MiB 限制".into()));
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
        return Err(ContentError("服务器返回的内容不是受支持的图片格式".into()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_failure)? {
        if chunk.len() > MAX_IMAGE_BYTES.saturating_sub(bytes.len()) {
            return Err(ContentError("图片文件超过 2 MiB 限制".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, media_type))
}

fn network_failure(error: reqwest::Error) -> ContentError {
    ContentError(
        if error.is_timeout() {
            "图片服务器连接或响应超时"
        } else if error.is_connect() {
            "无法连接图片服务器，请检查网络或证书"
        } else {
            "图片数据未完整接收，请重试"
        }
        .into(),
    )
}

fn check_addresses(host: &str, addresses: &[SocketAddr]) -> Result<(), ContentError> {
    if addresses.is_empty() {
        return Err(ContentError("图片域名没有可用地址".into()));
    }
    if addresses.iter().any(|a| dns::fake_ip(a.ip())) {
        // Fake-IP is not proof of a trusted proxy: retain the private-network boundary.
        return Err(ContentError(format!(
            "图片域名 {} 仍为代理 Fake-IP，未取得安全的公网地址，请重试",
            host.chars().take(253).collect::<String>()
        )));
    }
    if addresses.iter().any(|address| !public_ip(address.ip())) {
        return Err(ContentError(
            "图片地址指向本机或私有网络，已阻止访问".into(),
        ));
    }
    Ok(())
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), ContentError> {
    if cancel.load(Ordering::Acquire) {
        Err(ContentError("图片加载已取消".into()))
    } else {
        Ok(())
    }
}

async fn cancellable<F: Future>(future: F, cancel: &AtomicBool) -> Result<F::Output, ContentError> {
    let mut future = std::pin::pin!(future);
    loop {
        check_cancel(cancel)?;
        if let Ok(result) = tokio::time::timeout(Duration::from_millis(25), future.as_mut()).await {
            check_cancel(cancel)?;
            return Ok(result);
        }
    }
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
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Instant,
    };

    #[test]
    fn fake_ip_has_actionable_reason_without_allowing_private_or_mixed_addresses() {
        let error = check_addresses(
            "images.example.invalid",
            &["198.18.0.77:443".parse().unwrap()],
        )
        .unwrap_err();
        assert!(error.0.contains("Fake-IP"));
        assert!(error.0.contains("images.example.invalid"));
        assert!(error.0.contains("请重试"));
        assert!(
            check_addresses("images.example.invalid", &["1.1.1.1:443".parse().unwrap()]).is_ok()
        );
        assert!(
            check_addresses(
                "images.example.invalid",
                &[
                    "1.1.1.1:443".parse().unwrap(),
                    "127.0.0.1:443".parse().unwrap()
                ]
            )
            .is_err()
        );
        assert!(
            check_addresses("images.example.invalid", &["10.0.0.1:443".parse().unwrap()])
                .unwrap_err()
                .0
                .contains("私有网络")
        );
    }

    fn cancel_stalled_image(body: bool) {
        // Loopback exists only in this transport fixture; fetch() still rejects private IPs.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/image.png", listener.local_addr().unwrap());
        let cancel = AtomicBool::new(false);
        let (entered, waiting) = mpsc::channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        thread::scope(|scope| {
            let headers = entered.clone();
            let server = scope.spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut stream = BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    stream.read_line(&mut line).unwrap();
                    if line == "\r\n" { break; }
                }
                if body {
                    stream.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 100\r\n\r\nx").unwrap();
                } else {
                    headers.send(()).unwrap();
                }
                // Dropping the future must close the connection, rather than detach a blocking fetch.
                assert_eq!(stream.read(&mut [0u8; 1]).unwrap(), 0);
            });
            let signal = &cancel;
            scope.spawn(move || {
                waiting.recv_timeout(Duration::from_secs(2)).unwrap();
                thread::sleep(Duration::from_millis(25));
                signal.store(true, Ordering::Release);
            });
            let started = Instant::now();
            let result = runtime.block_on(async {
                let client = Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(8))
                    .build()
                    .unwrap();
                if body {
                    let response = client.get(&url).send().await.unwrap();
                    entered.send(()).unwrap();
                    cancellable(read_image_response(response), &cancel).await
                } else {
                    cancellable(
                        async {
                            let response = client.get(&url).send().await.unwrap();
                            read_image_response(response).await
                        },
                        &cancel,
                    )
                    .await
                }
            });
            runtime.shutdown_timeout(Duration::from_millis(25));
            assert!(result.unwrap_err().0.contains("取消"));
            eprintln!("image cancellation body={body}: {:?}", started.elapsed());
            assert!(started.elapsed() < Duration::from_secs(1));
            server.join().unwrap();
        });
    }

    #[test]
    fn shutdown_cancels_stalled_image_headers_and_body() {
        cancel_stalled_image(false);
        cancel_stalled_image(true);
    }

    #[test]
    fn cancelled_image_load_leaves_pending_urls_and_content_intact() {
        let mut content = crate::parse(include_bytes!(
            "../../../tests/fixtures/mime/03-alternative-related.eml"
        ))
        .unwrap();
        content.remote_images = vec!["https://example.invalid/image.png".into()];
        let before = (
            content.remote_images.clone(),
            content.images.len(),
            content.browser_body.clone(),
        );
        let error = content
            .load_remote_images_with_cancel(&AtomicBool::new(true))
            .unwrap_err();
        assert!(error.0.contains("取消"));
        assert_eq!(
            before,
            (
                content.remote_images,
                content.images.len(),
                content.browser_body
            )
        );
    }
    #[test]
    fn loaded_images_are_removed_from_pending_urls_but_blocked_images_remain_retryable() {
        let mut content = crate::parse(include_bytes!(
            "../../../tests/fixtures/mime/03-alternative-related.eml"
        ))
        .unwrap();
        let loaded = "https://example.invalid/already-loaded.png".to_string();
        let blocked = "http://127.0.0.1/private.png".to_string();
        content.images[0].key = loaded.clone();
        content.remote_images = vec![loaded, blocked.clone()];
        assert_eq!(content.load_remote_images_after_consent().unwrap(), 0);
        assert_eq!(content.remote_images, vec![blocked]);
        assert_eq!(content.images.len(), 1);
        assert!(!content.warnings.is_empty());
        assert!(content.warnings.iter().any(|w| w.contains("私有网络")));
        let warnings = content.warnings.len();
        content.load_remote_images_after_consent().unwrap();
        assert_eq!(
            content.warnings.len(),
            warnings,
            "retry must replace stale failures"
        );
        content.remote_images.clear();
        content.load_remote_images_after_consent().unwrap();
        assert!(
            !content
                .warnings
                .iter()
                .any(|w| w.starts_with(REMOTE_WARNING))
        );
    }

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
