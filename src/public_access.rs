//! Explicit anonymous, bounded diagnostics; this never certifies a provider fetch.
use anyhow::{Context, Result, bail, ensure};
use reqwest::{blocking::Client, header};
use serde::Serialize;
use std::{io::Read, time::Duration};
use url::Url;

pub const PREFIX_BYTES: usize = 1024;
const MAX_REDIRECTS: usize = 4;

#[derive(Debug)]
pub struct CheckFailed {
    pub report: serde_json::Value,
}
impl std::fmt::Display for CheckFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "anonymous public access check failed")
    }
}
impl std::error::Error for CheckFailed {}

pub(crate) fn checked_result(
    report: serde_json::Value,
    success: bool,
) -> Result<serde_json::Value> {
    if success {
        Ok(report)
    } else {
        Err(CheckFailed { report }.into())
    }
}

#[derive(Debug, Serialize)]
pub struct PublicAccessReport {
    pub http_status: u16,
    pub content_type: Option<String>,
    pub content_range: Option<String>,
    pub requested_range: &'static str,
    pub bytes_checked: usize,
    pub redirects: usize,
    pub body_limit_exceeded: bool,
    pub anonymous_read_succeeded: bool,
    pub prefix_matches: Option<bool>,
    pub full_sha256_verified: bool,
    pub provider_fetch_verified: bool,
}

/// No credentials, cookies, proxy, or provider requests. Every redirect gets a
/// fresh client from the existing HTTPS/global-address/IP-pinning boundary.
pub fn check(
    raw: &str,
    expected_prefix: Option<&[u8]>,
    expected_bytes: Option<u64>,
) -> Result<PublicAccessReport> {
    check_with(raw, expected_prefix, expected_bytes, |url| {
        public_download_client(url)
    })
}

fn range_length(raw: Option<&str>, expected_bytes: Option<u64>) -> Option<usize> {
    let (range, total) = raw?.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = end.parse().ok()?;
    let total: u64 = total.parse().ok()?;
    let length = end.checked_add(1)?;
    if start != 0
        || end >= PREFIX_BYTES as u64
        || end >= total
        || length != total.min(PREFIX_BYTES as u64)
        || expected_bytes.is_some_and(|expected| expected != total)
    {
        return None;
    }
    usize::try_from(length).ok()
}

fn check_with(
    raw: &str,
    expected_prefix: Option<&[u8]>,
    expected_bytes: Option<u64>,
    mut client_for: impl FnMut(&Url) -> Result<Client>,
) -> Result<PublicAccessReport> {
    ensure!(raw.len() <= 4096, "public URL exceeds 4096 bytes");
    ensure!(
        expected_prefix.is_none_or(|p| p.len() <= PREFIX_BYTES),
        "expected public prefix exceeds 1024 bytes"
    );
    let mut url = Url::parse(raw).context("invalid public URL")?;
    for redirects in 0..=MAX_REDIRECTS {
        let mut response = client_for(&url)?
            .get(url.clone())
            .timeout(Duration::from_secs(30))
            .header(
                header::USER_AGENT,
                concat!("skywrite/", env!("CARGO_PKG_VERSION")),
            )
            .header(header::RANGE, "bytes=0-1023")
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .map_err(|error| error.without_url())
            .context("anonymous public range request failed")?;
        if response.status().is_redirection() {
            ensure!(redirects < MAX_REDIRECTS, "too many public URL redirects");
            let location = response
                .headers()
                .get(header::LOCATION)
                .context("public redirect lacks Location")?
                .to_str()?;
            ensure!(
                location.len() <= 4096,
                "public redirect URL exceeds 4096 bytes"
            );
            url = url.join(location)?;
            ensure!(
                url.as_str().len() <= 4096,
                "public redirect URL exceeds 4096 bytes"
            );
            continue;
        }
        let status = response.status();
        let header_text = |name: header::HeaderName| -> Option<String> {
            response
                .headers()
                .get(name)?
                .to_str()
                .ok()
                .map(|s| s.chars().take(512).collect())
        };
        let content_type = header_text(header::CONTENT_TYPE);
        let content_range = header_text(header::CONTENT_RANGE);
        let content_encoding = header_text(header::CONTENT_ENCODING);
        let mut bytes = Vec::with_capacity(PREFIX_BYTES + 1);
        response
            .by_ref()
            .take((PREFIX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        let body_limit_exceeded = bytes.len() > PREFIX_BYTES;
        let range_valid = if status.as_u16() == 206 {
            range_length(content_range.as_deref(), expected_bytes) == Some(bytes.len())
        } else {
            status.as_u16() == 200 && expected_bytes.is_none_or(|n| n == bytes.len() as u64)
        };
        let prefix_matches = expected_prefix.map(|prefix| bytes == prefix);
        return Ok(PublicAccessReport {
            http_status: status.as_u16(),
            content_type,
            content_range,
            requested_range: "bytes=0-1023",
            bytes_checked: bytes.len().min(PREFIX_BYTES),
            redirects,
            body_limit_exceeded,
            anonymous_read_succeeded: range_valid
                && !body_limit_exceeded
                && content_encoding
                    .as_deref()
                    .is_none_or(|encoding| encoding == "identity")
                && prefix_matches != Some(false),
            prefix_matches,
            full_sha256_verified: false,
            provider_fetch_verified: false,
        });
    }
    unreachable!("redirect count is bounded")
}

pub(crate) fn validate_public_url(url: &url::Url) -> Result<()> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.fragment().is_some()
    {
        bail!("video downloads require public HTTPS URLs without userinfo");
    }
    let host = url.host_str().context("video content URL lacks host")?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        bail!("private video download host rejected");
    }
    Ok(())
}
// Conservative subset of globally reachable unicast. See the checked-in IANA
// registries (retrieved 2026-10-30) and endpoint table under tests/fixtures/network.
// Entire special-use parent ranges may be denied even when they contain globally
// reachable exceptions; mapped/NAT64/6to4/Teredo addresses remain denied.
fn public_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let o = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_unspecified()
                || o[0] == 0
                || o[0] >= 240
                || o[0] == 127
                || o[0] == 100 && (64..=127).contains(&o[1])
                || o[0] == 192 && (o[1] == 0 || o[1] == 88 && o[2] == 99)
                || o[0] == 198 && (o[1] == 18 || o[1] == 19)
                || o[0] == 198 && o[1] == 51 && o[2] == 100
                || o[0] == 203 && o[1] == 0 && o[2] == 113)
        }
        std::net::IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
        }
    }
}
pub(crate) fn public_download_client(url: &url::Url) -> Result<Client> {
    use std::net::ToSocketAddrs;
    validate_public_url(url)?;
    let host = url.host().context("missing content host")?;
    // URL's typed host avoids treating brackets around an IPv6 literal as DNS.
    let addresses = match host {
        url::Host::Ipv4(ip) => vec![std::net::SocketAddr::new(ip.into(), 443)],
        url::Host::Ipv6(ip) => vec![std::net::SocketAddr::new(ip.into(), 443)],
        url::Host::Domain(name) => (name, 443).to_socket_addrs()?.collect::<Vec<_>>(),
    };
    if addresses.is_empty() || addresses.iter().any(|a| !public_address(a.ip())) {
        bail!("nonpublic video download address rejected");
    }
    Ok(pinned_download_builder(&host.to_string(), &addresses).build()?)
}
// Production calls this only after the complete resolved set passes admission.
// Tests exercise this same socket/TLS boundary with owned loopback addresses and
// a test root certificate, independently of the pure global-address guard.
fn pinned_download_builder(
    host: &str,
    addresses: &[std::net::SocketAddr],
) -> reqwest::blocking::ClientBuilder {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs(host, addresses)
}

#[cfg(test)]
pub(crate) fn public_test_client(
    host: &str,
    addresses: &[std::net::SocketAddr],
) -> reqwest::blocking::ClientBuilder {
    pinned_download_builder(host, addresses)
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn public_address_registry_endpoints_and_inverse() {
        let table = include_str!("../tests/fixtures/network/endpoints.txt");
        let mut count = 0;
        for line in table.lines().filter(|line| !line.starts_with('#')) {
            let cells: Vec<_> = line.split_whitespace().collect();
            for address in &cells[1..=2] {
                assert_eq!(
                    public_address(address.parse().unwrap()),
                    cells[3] == "true",
                    "{} ({})",
                    address,
                    cells[0]
                );
            }
            count += 1;
        }
        assert_eq!(count, 51);
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "192.1.0.1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "3ffe:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "3fff:1000::",
        ] {
            assert!(public_address(address.parse().unwrap()), "{address}");
        }
        for address in [
            "224.0.0.1",
            "ff02::1",
            "::ffff:8.8.8.8",
            "2002:808:808::",
            "64:ff9b::808:808",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_address(address.parse().unwrap()), "{address}");
        }
        for raw in [
            "http://example.com/",
            "https://user:pass@example.com/",
            "https://example.com:444/",
            "https://example.com/#fragment",
            "https://localhost/",
            "https://x.localhost/",
        ] {
            assert!(
                validate_public_url(&url::Url::parse(raw).unwrap()).is_err(),
                "{raw}"
            );
        }
        for raw in [
            "https://127.0.0.1/",
            "https://2130706433/",
            "https://0x7f000001/",
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
        ] {
            let error = public_download_client(&url::Url::parse(raw).unwrap()).unwrap_err();
            assert!(error.to_string().contains("nonpublic"), "{raw}: {error}");
        }
        let v6 = url::Url::parse("https://[2606:4700:4700::1111]/").unwrap();
        assert!(matches!(v6.host(), Some(url::Host::Ipv6(_))));
        assert!(public_download_client(&v6).is_ok()); // Builds only; no request.
        assert!(!public_address("127.0.0.1".parse().unwrap()));
        assert!(!public_address("::ffff:127.0.0.1".parse().unwrap()));
        assert!(public_address("1.1.1.1".parse().unwrap()));
        assert!(
            validate_public_url(&url::Url::parse("http://example.com/asset").unwrap()).is_err()
        );
        assert!(
            validate_public_url(&url::Url::parse("https://user:secret@example.com/asset").unwrap())
                .is_err()
        );
    }

    // A bounded owned Python TLS server. Drop always kills/reaps the fixture.
    struct OwnedTls {
        child: std::process::Child,
        address: std::net::SocketAddr,
    }
    impl Drop for OwnedTls {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    impl OwnedTls {
        fn start(ip: &str, port: u16) -> Self {
            Self::start_with_certificate(ip, port, &crate::test_tls::chain().server)
        }
        fn start_with_certificate(ip: &str, port: u16, leaf: &crate::test_tls::Leaf) -> Self {
            use std::io::BufRead;
            let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/network");
            // The server loads the chain before printing its port, read below.
            let files = leaf.files();
            let mut child = std::process::Command::new("python3")
                .arg(base.join("tls_server.py"))
                .arg(&files.cert)
                .arg(&files.key)
                .arg(ip)
                .arg(port.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let address =
                std::net::SocketAddr::new(ip.parse().unwrap(), line.trim().parse().unwrap());
            Self { child, address }
        }
        fn client(&self, host: &str) -> Client {
            let cert = crate::test_tls::chain().ca();
            pinned_download_builder(host, &[self.address])
                .tls_certs_merge([cert])
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap()
        }
    }
    #[test]
    fn pinned_download_owned_tls_socket_and_certificate_boundary() {
        // localhost has a real fallback address. With trusted matching TLS,
        // a forbidden resolver fallback would turn the wrong-pin failure into
        // success against this live server.
        let fallback = OwnedTls::start("127.0.0.1", 0);
        let fallback_url = format!("https://localhost:{}/", fallback.address.port());
        assert!(
            fallback
                .client("localhost")
                .get(&fallback_url)
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        // Only 127.0.0.1 and ::1 exist on every supported loopback (macOS lo0 has
        // no other 127/8 address). The fallback server listens on IPv4 only.
        let unavailable =
            std::net::SocketAddr::new("::1".parse().unwrap(), fallback.address.port());
        let cert = crate::test_tls::chain().ca();
        let no_fallback = pinned_download_builder("localhost", &[unavailable])
            .tls_certs_merge([cert])
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(no_fallback.get(&fallback_url).send().is_err());
        let first = OwnedTls::start("127.0.0.1", 0);
        let port = first.address.port();
        let second = OwnedTls::start("::1", port);
        let domain_url = format!("https://pinned.invalid:{port}/");
        let client = first.client("pinned.invalid");
        assert_eq!(
            client.get(&domain_url).send().unwrap().text().unwrap(),
            "ok"
        );
        assert_eq!(
            first
                .client("127.0.0.1")
                .get(format!("https://127.0.0.1:{port}/"))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        // Rebuilding with the same origin but a different address reaches server 2.
        // Killing server 1 ensures an old pool or ignored new pin cannot pass.
        drop(first);
        assert!(client.get(&domain_url).send().is_err());
        assert_eq!(
            second
                .client("pinned.invalid")
                .get(&domain_url)
                .send()
                .unwrap()
                .text()
                .unwrap(),
            "ok"
        );
        let redirect = second
            .client("pinned.invalid")
            .get(format!("https://pinned.invalid:{port}/redirect"))
            .send()
            .unwrap();
        assert_eq!(redirect.status(), reqwest::StatusCode::FOUND);
        // A wrong pin must fail even while a valid same-port loopback server exists.
        let wrong: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let cert = crate::test_tls::chain().ca();
        let wrong_client = pinned_download_builder("pinned.invalid", &[wrong])
            .tls_certs_merge([cert])
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(wrong_client.get(&domain_url).send().is_err());
        assert!(
            second
                .client("mismatch.invalid")
                .get(format!("https://mismatch.invalid:{port}/"))
                .send()
                .is_err()
        );
        assert_eq!(
            second
                .client("::1")
                .get(format!("https://[::1]:{port}/"))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        let v6 = OwnedTls::start("::1", 0);
        assert_eq!(
            v6.client("::1")
                .get(format!("https://[::1]:{}/", v6.address.port()))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        for ip in ["127.0.0.1", "::1"] {
            let mismatched =
                OwnedTls::start_with_certificate(ip, 0, &crate::test_tls::chain().mismatch);
            let literal = if ip.contains(':') {
                format!("[{ip}]")
            } else {
                ip.to_owned()
            };
            assert!(
                mismatched
                    .client(ip)
                    .get(format!("https://{literal}:{}/", mismatched.address.port()))
                    .send()
                    .is_err()
            );
            // Same certificate and socket succeed for its DNS SAN: rejection
            // above is specifically the missing IP SAN, not broken trust/I/O.
            assert_eq!(
                mismatched
                    .client("mismatch.invalid")
                    .get(format!(
                        "https://mismatch.invalid:{}/",
                        mismatched.address.port()
                    ))
                    .send()
                    .unwrap()
                    .status(),
                reqwest::StatusCode::OK
            );
        }
        // Name mismatch remains rejected on an IPv6 connection.
        assert!(
            v6.client("mismatch.invalid")
                .get(format!("https://mismatch.invalid:{}/", v6.address.port()))
                .send()
                .is_err()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::BufRead,
        path::Path,
        process::{Child, Command, Stdio},
    };

    struct OwnedTls {
        child: Child,
        address: std::net::SocketAddr,
    }
    impl Drop for OwnedTls {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    impl OwnedTls {
        fn start() -> Self {
            let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/network");
            // The server loads the chain before printing its port, read below.
            let files = crate::test_tls::chain().server.files();
            let mut child = Command::new("python3")
                .arg(base.join("public_server.py"))
                .arg(&files.cert)
                .arg(&files.key)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            Self {
                child,
                address: format!("127.0.0.1:{}", line.trim()).parse().unwrap(),
            }
        }
        fn client(&self, url: &Url) -> Result<Client> {
            validate_public_url(url)?;
            if url.host_str() != Some("pinned.invalid") {
                return public_download_client(url);
            }
            let cert = crate::test_tls::chain().ca();
            Ok(public_test_client("pinned.invalid", &[self.address])
                .tls_certs_merge([cert])
                .timeout(Duration::from_secs(2))
                .build()?)
        }
        fn check(
            &self,
            path: &str,
            expected: Option<&[u8]>,
            bytes: Option<u64>,
        ) -> Result<PublicAccessReport> {
            check_with(
                &format!("https://pinned.invalid{path}"),
                expected,
                bytes,
                |url| self.client(url),
            )
        }
    }

    #[test]
    fn failed_signed_redirect_request_omits_url_from_error_chain() {
        use std::io::Write;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for redirect in [true, false] {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                if redirect {
                    reader.get_mut().write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /private-target?token=private-signature\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    ).unwrap();
                } else {
                    reader.get_mut().write_all(b"invalid HTTP\r\n\r\n").unwrap();
                }
            }
        });
        let error = check_with(
            &format!("http://{address}/base?token=original-signature"),
            None,
            None,
            |_| {
                Ok(Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?)
            },
        )
        .unwrap_err();
        server.join().unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("anonymous public range request failed"));
        assert!(!message.contains("private-target"), "{message}");
        assert!(!message.contains("signature"), "{message}");
        assert!(!message.contains("for url"), "{message}");
    }

    #[test]
    fn content_ranges_reject_extreme_and_invalid_values_without_panicking() {
        assert_eq!(range_length(None, None), None);
        for raw in [
            "",
            "bytes */8192",
            "bytes 0-0/*",
            "bytes 0-0/0",
            "bytes 1-1023/8192",
            "bytes 0-1024/8192",
            "bytes 0-1022/8192",
            "bytes 0-2/2",
            "bytes 0-18446744073709551615/18446744073709551615",
            "bytes 18446744073709551615-18446744073709551615/18446744073709551615",
            "bytes 0-18446744073709551616/18446744073709551615",
            "bytes 0-1023/18446744073709551616",
        ] {
            assert_eq!(range_length(Some(raw), None), None, "{raw}");
        }
        assert_eq!(range_length(Some("bytes 0-1023/8192"), Some(9000)), None);
    }

    #[test]
    fn content_ranges_accept_full_prefix_and_complete_short_objects() {
        for (raw, expected_total, length) in [
            ("bytes 0-1023/8192", 8192, PREFIX_BYTES),
            ("bytes 0-1023/1024", 1024, PREFIX_BYTES),
            ("bytes 0-1023/18446744073709551615", u64::MAX, PREFIX_BYTES),
            ("bytes 0-1/2", 2, 2),
            ("bytes 0-0/1", 1, 1),
        ] {
            assert_eq!(range_length(Some(raw), None), Some(length), "{raw}");
            assert_eq!(
                range_length(Some(raw), Some(expected_total)),
                Some(length),
                "{raw}"
            );
        }
    }

    #[test]
    fn anonymous_owned_tls_success_range_prefix_and_no_credentials() {
        let server = OwnedTls::start();
        let prefix = vec![b'x'; PREFIX_BYTES];
        let report = server.check("/success", Some(&prefix), Some(8192)).unwrap();
        assert_eq!(report.http_status, 206);
        assert!(report.anonymous_read_succeeded);
        assert_eq!(report.prefix_matches, Some(true));
        assert_eq!(report.content_type.as_deref(), Some("video/mp4"));
        assert_eq!(report.bytes_checked, 1024);
        assert!(!report.full_sha256_verified && !report.provider_fetch_verified);
        // Legitimate small objects can ignore Range and return a complete 200.
        let small = server.check("/small", Some(b"ok"), Some(2)).unwrap();
        assert!(small.anonymous_read_succeeded);
        assert_eq!(small.http_status, 200);
        let mismatch = server
            .check("/success", Some(&vec![b'y'; PREFIX_BYTES]), Some(8192))
            .unwrap();
        assert_eq!(mismatch.prefix_matches, Some(false));
        assert!(!mismatch.anonymous_read_succeeded);
        assert!(
            !server
                .check("/success", Some(&prefix), Some(9000))
                .unwrap()
                .anonymous_read_succeeded
        );
    }

    #[test]
    fn anonymous_owned_tls_denied_oversize_truncated_and_invalid_ranges() {
        let server = OwnedTls::start();
        let denied = server.check("/denied", None, None).unwrap();
        assert_eq!(denied.http_status, 403);
        assert!(!denied.anonymous_read_succeeded);
        for path in ["/oversized", "/ignored-range"] {
            let report = server.check(path, None, None).unwrap();
            assert!(report.body_limit_exceeded, "{path}");
            assert_eq!(report.bytes_checked, PREFIX_BYTES);
            assert!(!report.anonymous_read_succeeded);
        }
        for path in ["/short", "/wrong-range"] {
            assert!(
                !server
                    .check(path, None, None)
                    .unwrap()
                    .anonymous_read_succeeded,
                "{path}"
            );
        }
        let failure =
            checked_result(serde_json::json!({"public_access":denied}), false).unwrap_err();
        assert_eq!(
            failure.downcast_ref::<CheckFailed>().unwrap().report["public_access"]["http_status"],
            403
        );
    }

    #[test]
    fn anonymous_owned_tls_redirects_revalidate_every_hop_without_cookie_forwarding() {
        let server = OwnedTls::start();
        let redirected = server.check("/redirect", None, None).unwrap();
        assert!(redirected.anonymous_read_succeeded);
        assert_eq!(redirected.redirects, 1);
        for (path, reason) in [
            ("/private", "nonpublic"),
            ("/http", "public HTTPS"),
            ("/userinfo", "public HTTPS"),
            ("/loop", "too many"),
        ] {
            let error = server.check(path, None, None).unwrap_err();
            assert!(error.to_string().contains(reason), "{path}: {error:#}");
        }
        for raw in [
            "http://example.com/",
            "https://localhost/",
            "https://127.0.0.1/",
            "https://user:secret@example.com/",
        ] {
            assert!(check(raw, None, None).is_err(), "{raw}");
        }
    }
}
