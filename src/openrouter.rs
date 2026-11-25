//! Explicit, bounded model requests with content-addressed response caching.
//!
//! One client serves the built-in OpenRouter provider and user-configured
//! OpenAI-compatible providers. OpenRouter-only request fields (data policy
//! routing, usage accounting, reasoning toggles) and its JSON transcription
//! body go only to the built-in provider; other providers receive the standard
//! OpenAI wire format and never another provider's key.
use crate::providers::Provider;
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::blocking::Client as HttpClient;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_REQUEST_BYTES: usize = 24 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Body fields only OpenRouter understands; stripped for every other provider.
pub const OPENROUTER_ONLY_FIELDS: &[&str] = &[
    "provider",
    "usage",
    "reasoning",
    "transforms",
    "models",
    "route",
    "plugins",
];

/// A provider that could not be reached at all. No request was processed, so
/// nothing was billed; callers stop instead of repeating the request.
#[derive(Debug)]
pub struct Unreachable(pub String);
impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unreachable {}

/// Classifies a failed `send()`: `Some(detail)` only when reqwest reports a
/// connect failure. The connector (DNS, TCP, TLS handshake, proxy tunnel) runs
/// before hyper writes any request byte, rustls sends no early data, and a
/// reused pooled connection never reaches the connector, so such a request
/// provably never left this machine. Every other error, and any error while
/// reading a response body, may follow a sent request and is never classified
/// here. Clients that call this must neither follow redirects nor retry
/// (reqwest resends after an HTTP/2 GOAWAY by default).
pub(crate) fn connect_failure(error: &reqwest::Error) -> Option<String> {
    if !error.is_connect() {
        return None;
    }
    let mut detail = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !detail.contains(&text) {
            detail.push(text);
        }
        source = cause.source();
    }
    Some(if detail.is_empty() {
        error.to_string()
    } else {
        detail.join(": ")
    })
}

fn http_client(connect_timeout: Duration, request_timeout: Duration) -> Result<HttpClient> {
    HttpClient::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .redirect(reqwest::redirect::Policy::none())
        // See `connect_failure`: a client retry could hide an earlier send.
        .retry(reqwest::retry::never())
        .build()
        .context("initializing HTTPS client")
}
fn bounded_response(reader: impl Read, provider: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {provider} response"))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        bail!("{provider} response exceeded 8 MiB");
    }
    Ok(bytes)
}

pub struct Client {
    client: HttpClient,
    provider: Provider,
    cache_dir: Option<PathBuf>,
    /// Test fixtures supply a dummy key without touching the environment.
    key_override: Option<String>,
}
#[derive(Debug)]
pub struct ApiResponse {
    pub body: Value,
    pub cached: bool,
}
impl Client {
    pub fn new(provider: &Provider, cache_dir: Option<&Path>) -> Result<Self> {
        for warning in provider.warnings() {
            eprintln!("Warning: provider {}: {warning}", provider.name);
        }
        Ok(Self {
            client: http_client(CONNECT_TIMEOUT, provider.timeout)?,
            provider: provider.clone(),
            cache_dir: cache_dir.map(Path::to_owned),
            key_override: None,
        })
    }
    fn api_key(&self) -> Result<Option<String>> {
        match &self.key_override {
            Some(key) => Ok(Some(key.clone())),
            None => self.provider.api_key(),
        }
    }
    pub fn is_openrouter(&self) -> bool {
        self.provider.is_openrouter()
    }
    /// A built-in OpenRouter client against a loopback fixture.
    #[cfg(test)]
    pub(crate) fn for_test(
        base_url: String,
        cache_dir: Option<PathBuf>,
        timeout: Duration,
    ) -> Self {
        Self {
            client: http_client(timeout, timeout).unwrap(),
            provider: Provider::builtin_at(base_url),
            cache_dir,
            key_override: Some("DUMMY_TEST_ONLY".into()),
        }
    }

    fn send_error(&self, error: reqwest::Error, what: &str) -> anyhow::Error {
        if let Some(detail) = connect_failure(&error) {
            return anyhow::Error::new(Unreachable(format!(
                "provider {} at {} is unreachable ({detail}); start the server or fix its base URL (skywrite providers show {}). this request was not sent, and Skywrite does not retry",
                self.provider.name, self.provider.base_url, self.provider.name
            )));
        }
        let hint = if error.is_timeout() {
            self.provider.deadline_hint()
        } else {
            String::new()
        };
        anyhow::Error::new(error).context(format!("{what} failed{hint}"))
    }
    /// A body that stalls past the deadline fails while reading, after the
    /// headers arrived; it gets the same deadline guidance as a slow reply.
    fn read_body(&self, response: reqwest::blocking::Response) -> Result<Vec<u8>> {
        bounded_response(response, &self.provider.name).map_err(|error| {
            if format!("{error:#}").contains("timed out") {
                error.context(format!(
                    "reading the response timed out{}",
                    self.provider.deadline_hint()
                ))
            } else {
                error
            }
        })
    }

    /// Catalog GETs. OpenRouter's public catalogs never get a key; a custom
    /// provider's `/models` gets its configured key, since some servers
    /// require it even for listing. A custom server without `/models` (404)
    /// yields an empty catalog marked `skywrite_missing_catalog`.
    pub fn catalog(&self, endpoint: &str) -> Result<Value> {
        let name = &self.provider.name;
        let mut request = if self.provider.is_openrouter() {
            if !matches!(
                endpoint,
                "models"
                    | "models?output_modalities=image"
                    | "models?output_modalities=video"
                    | "models?output_modalities=transcription"
                    | "models?output_modalities=speech"
                    | "models?output_modalities=audio"
                    | "images/models"
                    | "videos/models"
            ) {
                bail!("unsupported OpenRouter catalog endpoint");
            }
            self.client
                .get(format!("{}/{endpoint}", self.provider.base_url))
        } else {
            if endpoint != "models" {
                bail!("unsupported provider catalog endpoint");
            }
            let mut request = self.provider.with_headers(
                self.client
                    .get(format!("{}/models", self.provider.base_url)),
            );
            // A configured key variable that is unset fails before any request.
            if let Some(token) = self.api_key()? {
                request = request.bearer_auth(token);
            }
            request
        };
        request = request.header("Accept", "application/json");
        let response = request
            .send()
            .map_err(|error| self.send_error(error, &format!("{name} catalog request")))?;
        let status = response.status();
        let bytes = self.read_body(response)?;
        if status.as_u16() == 404 && !self.provider.is_openrouter() {
            return Ok(serde_json::json!({"data":[],"skywrite_missing_catalog":true}));
        }
        if !status.is_success() {
            bail!(
                "{name} catalog HTTP {}: {}",
                status.as_u16(),
                self.redact(&String::from_utf8_lossy(&bytes))
            );
        }
        let body: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid {name} catalog JSON"))?;
        if !body["data"].is_array() {
            bail!("{name} catalog has no data array");
        }
        Ok(body)
    }

    fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        if let Ok(Some(key)) = self.api_key() {
            text = text.replace(&key, "<redacted API key>");
        }
        text.chars().take(2048).collect()
    }

    pub fn request(&self, endpoint: &str, body: &Value) -> Result<ApiResponse> {
        if !matches!(endpoint, "audio/transcriptions" | "chat/completions") {
            bail!("unsupported model endpoint");
        }
        let name = self.provider.name.clone();
        let openrouter = self.provider.is_openrouter();
        let mut body = body.clone();
        if !openrouter && let Some(object) = body.as_object_mut() {
            for field in OPENROUTER_ONLY_FIELDS {
                object.remove(*field);
            }
        }
        // Routing policy is not part of the content: keep it out of the cache key
        // so cached results survive policy changes and are never paid for twice.
        let encoded = serde_json::to_vec(&body)?;
        if encoded.len() > MAX_REQUEST_BYTES {
            bail!("model request exceeds 24 MiB; shorten the selected range or reduce frame count");
        }
        let key = cache_key(&[
            self.provider.identity().as_bytes(),
            endpoint.as_bytes(),
            &encoded,
        ]);
        let cache = self
            .cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("{key}.json")));
        if let Some(path) = cache.as_ref().filter(|path| path.is_file()) {
            // Cached provider data has the same byte budget as a live response.
            let file = std::fs::File::open(path)?;
            if !file.metadata()?.is_file() {
                bail!("response cache must be a regular file");
            }
            let cached: Value = serde_json::from_slice(&bounded_response(file, &name)?)
                .context("invalid response cache JSON")?;
            return Ok(ApiResponse {
                body: cached,
                cached: true,
            });
        }
        let token = self.api_key()?;
        // Validate routing before announcing a request.
        let policy = if openrouter { data_policy()? } else { None };
        eprintln!(
            "{name}: requesting {} from {}",
            endpoint,
            body["model"].as_str().unwrap_or("selected model")
        );
        let url = format!("{}/{endpoint}", self.provider.base_url);
        let mut request = self.client.post(&url);
        if let Some(token) = &token {
            request = request.bearer_auth(token);
        }
        request = self.provider.with_headers(request);
        request = if openrouter {
            let mut routed = body.clone();
            if let Some(provider) = policy {
                routed["provider"] = provider;
            }
            routed["usage"] = serde_json::json!({"include": true});
            request
                .header("Content-Type", "application/json")
                .header("X-Title", "Skywrite")
                .body(serde_json::to_vec(&routed)?)
        } else if endpoint == "audio/transcriptions" {
            let (boundary, form) = multipart_transcription(&body)?;
            request
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(form)
        } else {
            request
                .header("Content-Type", "application/json")
                .body(encoded)
        };
        let response = request
            .send()
            .map_err(|error| self.send_error(error, &format!("{name} request")))?;
        let status = response.status();
        let bytes = self.read_body(response)?;
        if !status.is_success() {
            let message = self.redact(&String::from_utf8_lossy(&bytes));
            if openrouter
                && status.as_u16() == 404
                && (message.contains("data policy") || message.contains("No endpoints"))
            {
                bail!(
                    "OpenRouter HTTP {}: {}\nNo provider of this model satisfies the data policy ({}). Choose another model, or set {DATA_POLICY_ENV}=no-training or provider-default to relax it.",
                    status.as_u16(),
                    message,
                    policy_name()
                );
            }
            bail!(
                "{name} HTTP {}: {}{}",
                status.as_u16(),
                message,
                self.media_hint(&body, &message)
            );
        }
        let parsed: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("{name} returned invalid JSON"))?;
        if parsed.get("error").is_some() {
            let message = self.redact(&parsed["error"].to_string());
            bail!(
                "{name} returned an error: {message}{}",
                self.media_hint(&body, &message)
            );
        }
        if let Some(path) = cache {
            match crate::io::atomic_write(&path, &serde_json::to_vec(&parsed)?, false) {
                Ok(()) => {}
                Err(_) if path.is_file() => {}
                Err(e) => return Err(e).context("saving response cache"),
            }
        }
        Ok(ApiResponse {
            body: parsed,
            cached: false,
        })
    }

    /// Name the likely cause when a model rejects image or video input.
    fn media_hint(&self, body: &Value, message: &str) -> String {
        let sent = |kind: &str| {
            body["messages"].as_array().into_iter().flatten().any(|m| {
                m["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|part| part["type"] == kind)
            })
        };
        let lower = message.to_lowercase();
        let mentions = |words: &[&str]| words.iter().any(|w| lower.contains(w));
        let kind = if sent("image_url") && mentions(&["image", "vision", "multimodal", "mmproj"]) {
            "image"
        } else if sent("video_url") && mentions(&["video", "multimodal", "content type"]) {
            "video"
        } else {
            return String::new();
        };
        format!(
            "\nModel {} on provider {} appears not to accept {kind} input; choose a model with {kind} input (llama.cpp's llama-server also needs --mmproj for images)",
            body["model"].as_str().unwrap_or("?"),
            self.provider.name
        )
    }
}

/// The standard OpenAI `/audio/transcriptions` form (whisper.cpp, speaches,
/// faster-whisper-server, OpenRouter) from Skywrite's JSON request.
fn multipart_transcription(body: &Value) -> Result<(String, Vec<u8>)> {
    let audio = STANDARD
        .decode(
            body["input_audio"]["data"]
                .as_str()
                .context("transcription request has no audio")?,
        )
        .context("transcription audio is not base64")?;
    let format = body["input_audio"]["format"].as_str().unwrap_or("wav");
    if !format.bytes().all(|b| b.is_ascii_alphanumeric()) {
        bail!("invalid audio format name");
    }
    let boundary = format!("skywrite-{}", &cache_key(&[&audio, b"boundary"])[..32]);
    let mut form = Vec::with_capacity(audio.len() + 1024);
    let mut field = |name: &str, value: &str| {
        form.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    };
    let text = |key: &str| -> Result<Option<&str>> {
        match body.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) if !value.contains(['\r', '\n']) => Ok(Some(value)),
            Some(_) => bail!("transcription field {key} must be a single-line string"),
        }
    };
    field(
        "model",
        text("model")?.context("transcription request has no model")?,
    );
    if let Some(value) = text("response_format")? {
        field("response_format", value);
    }
    if let Some(value) = text("language")? {
        field("language", value);
    }
    for granularity in body["timestamp_granularities"]
        .as_array()
        .into_iter()
        .flatten()
    {
        field(
            "timestamp_granularities[]",
            granularity
                .as_str()
                .context("timestamp granularity must be a string")?,
        );
    }
    form.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.{format}\"\r\nContent-Type: audio/{format}\r\n\r\n"
        )
        .as_bytes(),
    );
    form.extend_from_slice(&audio);
    form.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((boundary, form))
}

pub const DATA_POLICY_ENV: &str = "SKYWRITE_DATA_POLICY";

fn policy_name() -> String {
    std::env::var(DATA_POLICY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "zero-retention".into())
}

/// Provider routing for requests that carry the user's media or words.
/// Default: only providers that neither train on nor retain the data.
pub fn data_policy() -> Result<Option<Value>> {
    Ok(match policy_name().as_str() {
        "zero-retention" => Some(serde_json::json!({"data_collection": "deny", "zdr": true})),
        "no-training" => Some(serde_json::json!({"data_collection": "deny"})),
        "provider-default" => None,
        other => bail!(
            "{DATA_POLICY_ENV} must be zero-retention, no-training or provider-default, not {other:?}"
        ),
    })
}

pub fn cache_key(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn assistant_json(response: &Value) -> Result<Value> {
    let content = response["choices"][0]["message"]["content"]
        .as_str()
        .context("model returned no text content")?;
    let mut content = content.trim();
    // Local reasoning models may prefix their answer with a thinking block.
    if let Some(rest) = content.strip_prefix("<think>")
        && let Some((_, answer)) = rest.split_once("</think>")
    {
        content = answer.trim();
    }
    let content = content
        .strip_prefix("```json")
        .or_else(|| content.strip_prefix("```"))
        .and_then(|s| s.trim_end().strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(content);
    serde_json::from_str(content)
        .context("model did not return valid JSON; select a model supporting JSON output")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_separates_endpoint_model_and_payload() {
        assert_ne!(cache_key(&[b"a", b"bc"]), cache_key(&[b"ab", b"c"]));
        assert_ne!(
            cache_key(&[b"model-a", b"audio"]),
            cache_key(&[b"model-b", b"audio"])
        );
        assert_ne!(
            cache_key(&[b"frames", b"one"]),
            cache_key(&[b"frames", b"two"])
        );
    }
    #[test]
    fn extracts_json_without_inventing_model_output() {
        assert_eq!(assistant_json(&serde_json::json!({"choices":[{"message":{"content":"```json\n{\"summary\":\"hi\"}\n```"}}]})).unwrap()["summary"], "hi");
        assert!(assistant_json(&serde_json::json!({"choices":[]})).is_err());
        assert_eq!(assistant_json(&serde_json::json!({"choices":[{"message":{"content":"<think>\nhmm {\"x\":1}\n</think>\n{\"summary\":\"hi\"}"}}]})).unwrap()["summary"], "hi");
        assert!(assistant_json(&serde_json::json!({"choices":[{"message":{"content":"<think>unterminated {\"summary\":\"hi\"}"}}]})).is_err());
    }
    #[test]
    fn request_cache_uses_actual_content_and_needs_no_key() {
        let dir = tempfile::tempdir().unwrap();
        // A variable no environment sets: the built-in provider without a key.
        let mut provider = Provider::builtin();
        provider.key_env = Some("SKYWRITE_TEST_UNSET_OPENROUTER_KEY".into());
        let api = Client::new(&provider, Some(dir.path())).unwrap();
        let request = serde_json::json!({"model":"test", "messages":[]});
        let encoded = serde_json::to_vec(&request).unwrap();
        let hash = cache_key(&[
            provider.identity().as_bytes(),
            b"chat/completions",
            &encoded,
        ]);
        crate::io::write_json(
            &dir.path().join(format!("{hash}.json")),
            &serde_json::json!({"cached":"yes"}),
            false,
        )
        .unwrap();
        let response = api.request("chat/completions", &request).unwrap();
        assert!(response.cached);
        assert_eq!(response.body["cached"], "yes");
        assert!(
            api.request(
                "chat/completions",
                &serde_json::json!({"model":"different"})
            )
            .unwrap_err()
            .to_string()
            .contains("OPENROUTER_API_KEY")
        );
    }
    /// Where the fixture withholds the reply until the client gives up.
    #[derive(Clone, Copy, PartialEq)]
    enum Stall {
        None,
        Headers,
        Body,
    }
    fn hold_until_closed(stream: &mut std::net::TcpStream) {
        let mut buffer = [0; 1024];
        while matches!(stream.read(&mut buffer), Ok(n) if n > 0) {}
    }
    // A single owned loopback request, with real reqwest transport and bounded
    // server lifetime. No runtime credentials or remote endpoints are involved.
    fn fixture(reply: Vec<u8>, stall: Stall) -> (Client, std::thread::JoinHandle<()>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            let header = String::from_utf8(request).unwrap();
            assert!(
                header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer dummy_test_only")
            );
            let length: usize = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["model"],
                "fixture"
            );
            // A stall holds until the client's deadline drops the connection. A
            // fixed sleep let a descheduled client find the reply ready and
            // succeed; the read timeout only bounds a broken deadline.
            if stall == Stall::Headers {
                hold_until_closed(&mut stream);
            }
            let split = reply.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let _ = stream.write_all(&reply[..split]);
            if stall == Stall::Body {
                hold_until_closed(&mut stream);
            }
            let _ = stream.write_all(&reply[split..]);
        });
        (
            Client::for_test(base, None, Duration::from_millis(120)),
            handle,
        )
    }
    fn reply(status: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }
    #[test]
    fn actual_http_success_cache_and_non_success() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(15));
        assert_eq!(
            Provider::builtin().timeout,
            Duration::from_secs(crate::providers::BUILTIN_TIMEOUT_SECONDS)
        );
        assert_eq!(crate::providers::BUILTIN_TIMEOUT_SECONDS, 180);
        let dir = tempfile::tempdir().unwrap();
        let (mut api, server) = fixture(reply("200 OK", br#"{"ok":true}"#), Stall::None);
        api.cache_dir = Some(dir.path().to_owned());
        let request = serde_json::json!({"model":"fixture"});
        assert!(!api.request("chat/completions", &request).unwrap().cached);
        server.join().unwrap();
        // The one-shot listener is gone: this second result must come from cache.
        let second = api.request("chat/completions", &request).unwrap();
        assert!(second.cached);
        assert_eq!(second.body["ok"], true);
        let (api, server) = fixture(
            reply("422 Unprocessable Entity", b"fixture rejected"),
            Stall::None,
        );
        let error = api
            .request("chat/completions", &request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 422") && error.contains("fixture rejected"));
        server.join().unwrap();
    }
    #[test]
    fn paid_request_errors_redact_active_api_key() {
        for (status, body) in [
            ("422 Unprocessable Entity", "rejected DUMMY_TEST_ONLY"),
            (
                "404 Not Found",
                "No endpoints for DUMMY_TEST_ONLY data policy",
            ),
            (
                "200 OK",
                r#"{"error":{"message":"rejected DUMMY_TEST_ONLY"}}"#,
            ),
        ] {
            let (api, server) = fixture(reply(status, body.as_bytes()), Stall::None);
            let error = api
                .request("chat/completions", &serde_json::json!({"model":"fixture"}))
                .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("<redacted API key>"), "{message}");
            assert!(!message.contains("DUMMY_TEST_ONLY"), "{message}");
            server.join().unwrap();
        }
    }

    #[test]
    fn actual_http_does_not_follow_redirects() {
        let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let bytes = format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", destination.local_addr().unwrap()).into_bytes();
        let (api, server) = fixture(bytes, Stall::None);
        assert!(
            api.request("chat/completions", &serde_json::json!({"model":"fixture"}))
                .unwrap_err()
                .to_string()
                .contains("HTTP 307")
        );
        server.join().unwrap();
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn actual_http_exact_body_cap_and_first_excess_byte() {
        let mut body = b"{\"data\":\"".to_vec();
        body.resize(MAX_RESPONSE_BYTES as usize - 2, b'a');
        body.extend_from_slice(b"\"}");
        assert_eq!(body.len(), MAX_RESPONSE_BYTES as usize);
        for excess in [false, true] {
            let mut bytes = body.clone();
            if excess {
                bytes.push(b' ');
            }
            let (mut api, server) = fixture(reply("200 OK", &bytes), Stall::None);
            // Large local transfers need an ordinary deadline, not the deliberately
            // short deadline used to discriminate the stall tests below.
            api.client = http_client(CONNECT_TIMEOUT, Duration::from_secs(5)).unwrap();
            let result = api.request("chat/completions", &serde_json::json!({"model":"fixture"}));
            if excess {
                assert!(result.unwrap_err().to_string().contains("exceeded 8 MiB"));
            } else {
                assert_eq!(
                    result.unwrap().body["data"].as_str().unwrap().len(),
                    MAX_RESPONSE_BYTES as usize - 11
                );
            }
            server.join().unwrap();
        }
    }
    #[test]
    fn actual_http_deadline_covers_slow_headers_and_body() {
        for custom in [false, true] {
            for stall in [Stall::Headers, Stall::Body] {
                let (mut api, server) = fixture(reply("200 OK", br#"{"ok":true}"#), stall);
                if custom {
                    // Same loopback fixture, as a custom provider.
                    api.provider.name = "local".into();
                }
                let started = std::time::Instant::now();
                let error = api
                    .request("chat/completions", &serde_json::json!({"model":"fixture"}))
                    .unwrap_err();
                assert!(started.elapsed() >= Duration::from_millis(80));
                assert!(started.elapsed() < Duration::from_secs(2));
                let message = format!("{error:#}");
                assert!(message.contains("timed out"), "{message}");
                assert_eq!(message.contains("--timeout"), custom, "{message}");
                server.join().unwrap();
            }
        }
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn public_catalog_get_omits_auth_and_rejects_unlisted_routes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = Client::for_test(
            format!("http://{}", listener.local_addr().unwrap()),
            None,
            Duration::from_secs(2),
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            let text = String::from_utf8(request).unwrap().to_lowercase();
            assert!(text.starts_with("get /videos/models http/1.1"));
            assert!(!text.contains("authorization:"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}").unwrap();
        });
        assert_eq!(
            api.catalog("videos/models").unwrap()["data"],
            serde_json::json!([])
        );
        server.join().unwrap();
        assert!(api.catalog("https://example.invalid").is_err());
        assert!(api.catalog("../credits").is_err());
    }
    #[test]
    fn custom_catalog_body_stall_names_the_provider_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut api = Client::for_test(
            format!("http://{}", listener.local_addr().unwrap()),
            None,
            Duration::from_millis(150),
        );
        api.provider.name = "local".into();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            // Headers now, then hold the body until the client gives up.
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"da");
            let mut buffer = [0; 64];
            while matches!(stream.read(&mut buffer), Ok(n) if n > 0) {}
        });
        let message = format!("{:#}", api.catalog("models").unwrap_err());
        assert!(
            message.contains("timed out") && message.contains("--timeout"),
            "{message}"
        );
        server.join().unwrap();
    }
}
