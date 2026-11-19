//! Fake-IP fallback only. HTTPS bootstrap addresses are pinned so querying the
//! resolver itself never relies on the system's potentially synthetic DNS.
use super::{ContentError, check_addresses, public_ip};
use reqwest::{Client, redirect::Policy};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

const MAX_RESPONSE: usize = 32 * 1024;
const MAX_TTL: u32 = 60;
struct Endpoint {
    host: &'static str,
    path: &'static str,
    bootstrap: &'static [&'static str],
}
const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        host: "dns.alidns.com",
        path: "/resolve",
        bootstrap: &["223.5.5.5:443", "223.6.6.6:443"],
    },
    Endpoint {
        host: "cloudflare-dns.com",
        path: "/dns-query",
        bootstrap: &["1.1.1.1:443", "1.0.0.1:443"],
    },
];

#[derive(Default)]
pub(super) struct ImageDns {
    // Memory only, scoped to one image-loading operation and bounded by DNS TTL.
    real: HashMap<String, (Instant, Vec<IpAddr>)>,
}

pub(super) fn fake_ip(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(ip) if ip.octets()[0] == 198 && matches!(ip.octets()[1], 18 | 19))
}

fn needs_fallback(host: &str, addresses: &[SocketAddr]) -> bool {
    host.parse::<IpAddr>().is_err()
        && addresses.iter().any(|a| fake_ip(a.ip()))
        && addresses
            .iter()
            .all(|a| fake_ip(a.ip()) || public_ip(a.ip()))
}

impl ImageDns {
    pub async fn resolve(
        &mut self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, ContentError> {
        let host = host.trim_matches(['[', ']']);
        if let Some((until, addresses)) = self.real.get(host)
            && *until > Instant::now()
        {
            return Ok(addresses
                .iter()
                .map(|ip| SocketAddr::new(*ip, port))
                .collect());
        }
        let addresses = tokio::time::timeout(
            Duration::from_secs(8),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| ContentError("图片域名解析超时".into()))?
        .map_err(|_| ContentError("图片域名解析失败".into()))?
        .collect::<Vec<_>>();
        if !needs_fallback(host, &addresses) {
            check_addresses(host, &addresses)?;
            return Ok(addresses);
        }
        let answer = tokio::time::timeout(Duration::from_secs(8), lookup_real(host))
            .await
            .map_err(|_| ContentError("独立图片 DNS 解析超时，可以重试".into()))??;
        let addresses = answer
            .ips
            .iter()
            .map(|ip| SocketAddr::new(*ip, port))
            .collect::<Vec<_>>();
        check_addresses(host, &addresses)?;
        self.real.insert(
            host.into(),
            (
                Instant::now() + Duration::from_secs(u64::from(answer.ttl)),
                answer.ips,
            ),
        );
        Ok(addresses)
    }
}

struct Answer {
    ips: Vec<IpAddr>,
    ttl: u32,
}

async fn lookup_real(host: &str) -> Result<Answer, ContentError> {
    for endpoint in ENDPOINTS {
        if let Ok(answer) = lookup_at(endpoint, host).await {
            // Never connect or cache an empty or unsafe answer.
            check_addresses(
                host,
                &answer
                    .ips
                    .iter()
                    .map(|ip| SocketAddr::new(*ip, 443))
                    .collect::<Vec<_>>(),
            )?;
            return Ok(answer);
        }
    }
    Err(ContentError("独立图片 DNS 服务暂时不可用，可以重试".into()))
}

async fn lookup_at(endpoint: &Endpoint, host: &str) -> Result<Answer, ContentError> {
    let failure = || ContentError("独立图片 DNS 响应无效".into());
    let bootstrap = endpoint
        .bootstrap
        .iter()
        .map(|s| s.parse::<SocketAddr>().map_err(|_| failure()))
        .collect::<Result<Vec<_>, _>>()?;
    check_addresses(endpoint.host, &bootstrap)?;
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .resolve_to_addrs(endpoint.host, &bootstrap)
        .build()
        .map_err(|_| failure())?;
    let mut answer = Answer {
        ips: vec![],
        ttl: MAX_TTL,
    };
    for kind in [1, 28] {
        let part = query(&client, endpoint, host, kind).await?;
        answer.ips.extend(part.ips);
        answer.ttl = answer.ttl.min(part.ttl);
    }
    answer.ips.sort();
    answer.ips.dedup();
    Ok(answer)
}

async fn query(
    client: &Client,
    endpoint: &Endpoint,
    host: &str,
    kind: u16,
) -> Result<Answer, ContentError> {
    let failure = || ContentError("独立图片 DNS 响应无效".into());
    let mut url = url::Url::parse(&format!("https://{}{}", endpoint.host, endpoint.path))
        .map_err(|_| failure())?;
    url.query_pairs_mut()
        .append_pair("name", host)
        .append_pair("type", &kind.to_string());
    let mut response = client
        .get(url)
        .header("Accept", "application/dns-json")
        .send()
        .await
        .map_err(|_| failure())?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        || !matches!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim(),
            "application/json" | "application/dns-json"
        )
    {
        return Err(failure());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| failure())? {
        if chunk.len() > MAX_RESPONSE.saturating_sub(bytes.len()) {
            return Err(failure());
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_answer(&bytes, host, kind)
}

#[derive(Deserialize)]
struct Message {
    #[serde(rename = "Status")]
    status: u16,
    #[serde(rename = "TC", default)]
    truncated: bool,
    #[serde(rename = "Question")]
    question: Questions,
    #[serde(rename = "Answer", default)]
    answers: Vec<Record>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Questions {
    One(Question),
    Many(Vec<Question>),
}
#[derive(Deserialize)]
struct Question {
    name: String,
    #[serde(rename = "type")]
    kind: u16,
}
#[derive(Deserialize)]
struct Record {
    name: String,
    #[serde(rename = "type")]
    kind: u16,
    #[serde(rename = "TTL")]
    ttl: u32,
    data: String,
}

fn name(value: &str) -> Result<String, ContentError> {
    let value = value.strip_suffix('.').unwrap_or(value);
    if value.is_empty()
        || value.len() > 253
        || value
            .split('.')
            .any(|label| label.is_empty() || label.len() > 63)
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(ContentError("独立图片 DNS 名称无效".into()));
    }
    Ok(value.to_ascii_lowercase())
}

fn parse_answer(bytes: &[u8], host: &str, kind: u16) -> Result<Answer, ContentError> {
    let failure = || ContentError("独立图片 DNS 响应无效".into());
    if bytes.len() > MAX_RESPONSE {
        return Err(failure());
    }
    let message: Message = serde_json::from_slice(bytes).map_err(|_| failure())?;
    let questions = match message.question {
        Questions::One(q) => vec![q],
        Questions::Many(q) => q,
    };
    let mut current = name(host)?;
    if !matches!(kind, 1 | 28)
        || message.truncated
        || !matches!(message.status, 0 | 3)
        || questions.len() != 1
        || questions[0].kind != kind
        || name(&questions[0].name)? != current
        || message.answers.len() > 64
    {
        return Err(failure());
    }
    if message.status == 3 {
        if !message.answers.is_empty() {
            return Err(failure());
        }
        return Ok(Answer {
            ips: vec![],
            ttl: 0,
        });
    }
    let mut ttl = MAX_TTL;
    let mut seen = HashSet::new();
    for _ in 0..=8 {
        if !seen.insert(current.clone()) {
            return Err(failure());
        }
        let related = message
            .answers
            .iter()
            .filter(|r| name(&r.name).is_ok_and(|n| n == current))
            .collect::<Vec<_>>();
        let aliases = related.iter().filter(|r| r.kind == 5).collect::<Vec<_>>();
        if !aliases.is_empty() {
            let target = name(&aliases[0].data)?;
            if related.iter().any(|r| matches!(r.kind, 1 | 28))
                || aliases
                    .iter()
                    .any(|r| !name(&r.data).is_ok_and(|n| n == target))
            {
                return Err(failure());
            }
            for alias in aliases {
                ttl = ttl.min(alias.ttl);
            }
            current = target;
            continue;
        }
        let mut ips = vec![];
        for record in related.into_iter().filter(|r| r.kind == kind) {
            let ip: IpAddr = record.data.parse().map_err(|_| failure())?;
            if (kind == 1) != ip.is_ipv4() {
                return Err(failure());
            }
            // Validate the complete set before it can be cached or pinned to HTTP.
            check_addresses(host, &[SocketAddr::new(ip, 443)])?;
            ttl = ttl.min(record.ttl);
            ips.push(ip);
        }
        return Ok(Answer { ips, ttl });
    }
    Err(failure())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn record(owner: &str, kind: u16, data: &str, ttl: u32) -> Value {
        json!({"name": owner, "type": kind, "TTL": ttl, "data": data})
    }

    fn message(kind: u16, records: Vec<Value>) -> Value {
        json!({"Status": 0, "Question": {"name": "images.example.", "type": kind}, "Answer": records})
    }

    fn parse(value: &Value, kind: u16) -> Result<Answer, ContentError> {
        parse_answer(&serde_json::to_vec(value).unwrap(), "images.example", kind)
    }

    #[test]
    fn fallback_requires_synthetic_hostname_without_other_private_answers() {
        let fake = "198.18.0.77:443".parse().unwrap();
        let public = "1.1.1.1:443".parse().unwrap();
        assert!(needs_fallback("images.example", &[fake]));
        assert!(needs_fallback("images.example", &[fake, public]));
        for (host, addresses) in [
            ("images.example", vec![]),
            ("images.example", vec![public]),
            ("198.18.0.77", vec![fake]),
            (
                "images.example",
                vec![fake, "127.0.0.1:443".parse().unwrap()],
            ),
            ("images.example", vec!["10.0.0.1:443".parse().unwrap()]),
        ] {
            assert!(!needs_fallback(host, &addresses));
        }
    }

    #[test]
    fn provider_question_shapes_cname_ownership_and_ttl() {
        let mut response = message(
            1,
            vec![
                record("IMAGES.EXAMPLE.", 5, "cdn.example.", 25),
                record("cdn.example", 1, "1.1.1.1", 120),
                record("unrelated.example", 1, "127.0.0.1", 1),
            ],
        );
        let answer = parse(&response, 1).unwrap();
        assert_eq!(answer.ips, vec!["1.1.1.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(answer.ttl, 25);
        response["Question"] = json!([response["Question"].clone()]);
        assert_eq!(parse(&response, 1).unwrap().ips, answer.ips);
        let v6 = parse(
            &message(
                28,
                vec![record("images.example", 28, "2606:4700:4700::1111", 300)],
            ),
            28,
        )
        .unwrap();
        assert_eq!(v6.ttl, MAX_TTL);
        assert!(v6.ips[0].is_ipv6());
        assert!(parse(&message(28, vec![]), 28).unwrap().ips.is_empty());
    }

    #[test]
    fn rejects_unsafe_addresses_wrong_families_and_alias_chains() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "198.18.0.77",
            "169.254.169.254",
            "::ffff:1.1.1.1",
            "fc00::1",
        ] {
            let kind = if ip.contains(':') { 28 } else { 1 };
            assert!(
                parse(
                    &message(kind, vec![record("images.example", kind, ip, 60)]),
                    kind
                )
                .is_err(),
                "{ip}"
            );
        }
        for records in [
            vec![record("images.example", 1, "::1", 60)],
            vec![
                record("images.example", 1, "1.1.1.1", 60),
                record("images.example", 1, "127.0.0.1", 60),
            ],
            vec![
                record("images.example", 5, "cdn.example", 60),
                record("cdn.example", 5, "images.example", 60),
            ],
            vec![
                record("images.example", 5, "a.example", 60),
                record("images.example", 5, "b.example", 60),
            ],
            vec![
                record("images.example", 5, "cdn.example", 60),
                record("images.example", 1, "1.1.1.1", 60),
            ],
        ] {
            assert!(parse(&message(1, records), 1).is_err());
        }
    }

    #[test]
    fn rejects_mismatched_truncated_and_oversized_responses() {
        let original = message(1, vec![record("images.example", 1, "1.1.1.1", 60)]);
        for (key, value) in [
            ("Status", json!(2)),
            ("TC", json!(true)),
            ("Question", json!([])),
            ("Question", json!({"name":"other.example", "type":1})),
            ("Question", json!({"name":"images.example", "type":28})),
        ] {
            let mut response = original.clone();
            response[key] = value;
            assert!(parse(&response, 1).is_err());
        }
        assert!(parse_answer(&vec![b' '; MAX_RESPONSE + 1], "images.example", 1).is_err());
        assert!(parse_answer(b"{", "images.example", 1).is_err());
        assert!(
            parse(
                &message(1, vec![record("images.example", 1, "1.1.1.1", 60); 65]),
                1
            )
            .is_err()
        );
    }

    #[test]
    fn cached_real_addresses_preserve_requested_port_and_expire() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut dns = ImageDns::default();
        dns.real.insert(
            "images.example".into(),
            (
                Instant::now() + Duration::from_secs(60),
                vec!["1.1.1.1".parse().unwrap()],
            ),
        );
        assert_eq!(
            runtime
                .block_on(dns.resolve("images.example", 8443))
                .unwrap(),
            vec!["1.1.1.1:8443".parse::<SocketAddr>().unwrap()]
        );
        dns.real.insert(
            "127.0.0.1".into(),
            (
                Instant::now() - Duration::from_secs(1),
                vec!["1.1.1.1".parse().unwrap()],
            ),
        );
        assert!(runtime.block_on(dns.resolve("127.0.0.1", 443)).is_err());
    }

    #[test]
    fn shutdown_cancels_stalled_encrypted_dns_connection() {
        use std::{
            io::Read,
            net::TcpListener,
            sync::{
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let cancel = AtomicBool::new(false);
        let (entered, waiting) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = [0; 4096];
                assert!(stream.read(&mut bytes).unwrap() > 0);
                entered.send(()).unwrap();
                loop {
                    match stream.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
                        Err(e) => panic!("DNS connection did not close: {e}"),
                    }
                }
            });
            let signal = &cancel;
            scope.spawn(move || {
                waiting.recv_timeout(Duration::from_secs(2)).unwrap();
                signal.store(true, Ordering::Release);
            });
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let started = Instant::now();
            let result = runtime.block_on(async {
                let client = Client::builder()
                    .no_proxy()
                    .resolve_to_addrs(ENDPOINTS[0].host, &[address])
                    .build()
                    .unwrap();
                super::super::cancellable(
                    query(&client, &ENDPOINTS[0], "images.example", 1),
                    &cancel,
                )
                .await
            });
            runtime.shutdown_timeout(Duration::from_millis(25));
            assert!(matches!(result, Err(e) if e.0.contains("取消")));
            assert!(started.elapsed() < Duration::from_secs(1));
        });
    }
}
