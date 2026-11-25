//! Named model providers: the built-in OpenRouter plus user-configured
//! OpenAI-compatible servers (local llama.cpp, Ollama, LM Studio, other hosts).
//!
//! Model references are `PROVIDER:MODEL`. The first `:` separates the provider
//! only when no `/` comes before it, so OpenRouter IDs (`vendor/model`,
//! `vendor/model:free`) stay bare and always mean the built-in provider. Only
//! the NAME of an environment variable holding a key is stored, never a key.
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

pub const BUILTIN: &str = "openrouter";
pub const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
pub const OPENROUTER_KEY_ENV: &str = "OPENROUTER_API_KEY";
/// OpenRouter answers fast; its request deadline is fixed.
pub const BUILTIN_TIMEOUT_SECONDS: u64 = 180;
/// Small local models on a CPU can take minutes per request.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 600;
pub const MAX_TIMEOUT_SECONDS: u64 = 3600;
const MAX_PROVIDERS: usize = 32;
const MAX_HEADERS: usize = 16;
/// The built-in providers, plus words OpenRouter uses as model variant
/// suffixes (`vendor/model:free`), reserved so `free:x` can never be mistaken
/// for a variant.
const RESERVED: &[&str] = &[
    BUILTIN,
    crate::ondevice::PROVIDER,
    "free",
    "thinking",
    "nitro",
    "floor",
    "online",
    "extended",
    "exacto",
];
/// Tasks whose wire format is standard OpenAI chat/completions or
/// /audio/transcriptions, and so may use any provider.
pub const PORTABLE_TASKS: &[&str] = &[
    "analysis",
    "frame-description",
    "video-description",
    "transcription",
    "editing",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub key_env: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub timeout: Duration,
}

impl Provider {
    pub fn builtin() -> Self {
        Self {
            name: BUILTIN.into(),
            base_url: OPENROUTER_BASE.into(),
            key_env: Some(OPENROUTER_KEY_ENV.into()),
            headers: BTreeMap::new(),
            timeout: Duration::from_secs(BUILTIN_TIMEOUT_SECONDS),
        }
    }
    /// The in-process on-device engine (`ondevice:MODEL`). It has no URL or
    /// key and serves only transcription; see `crate::ondevice`.
    pub fn ondevice() -> Self {
        Self {
            name: crate::ondevice::PROVIDER.into(),
            base_url: String::new(),
            key_env: None,
            headers: BTreeMap::new(),
            timeout: Duration::ZERO,
        }
    }
    pub fn is_ondevice(&self) -> bool {
        self.name == crate::ondevice::PROVIDER
    }
    fn from_config(name: &str, config: &ProviderConfig) -> Result<Self> {
        let config = validate_config(name, config)?;
        Ok(Self {
            name: name.into(),
            base_url: config.base_url,
            key_env: config.key_env,
            headers: config.headers,
            timeout: Duration::from_secs(config.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS)),
        })
    }
    /// The same provider with another per-request deadline.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self { timeout, ..self }
    }
    /// The built-in provider at another base URL (a loopback fixture).
    #[cfg(test)]
    pub(crate) fn builtin_at(base_url: String) -> Self {
        Self {
            base_url,
            ..Self::builtin()
        }
    }
    /// `request` with this provider's configured extra headers.
    pub(crate) fn with_headers(
        &self,
        mut request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        for (header, value) in &self.headers {
            request = request.header(header, value);
        }
        request
    }
    /// Advice for a request that ran out of this provider's deadline: only
    /// custom providers have one to raise.
    pub(crate) fn deadline_hint(&self) -> String {
        if self.is_openrouter() {
            return String::new();
        }
        format!(
            " (provider {} deadline {} s; slow local models may need skywrite providers add ... --timeout SECONDS)",
            self.name,
            self.timeout.as_secs()
        )
    }
    /// The built-in provider is the only one that receives OpenRouter-specific
    /// request fields (data policy routing, usage accounting, reasoning toggles).
    pub fn is_openrouter(&self) -> bool {
        self.name == BUILTIN
    }
    /// Everything that decides which backend answers. Part of every response
    /// cache key, so two providers never share an entry even for the same
    /// base URL and model name.
    pub fn identity(&self) -> String {
        let mut identity = format!("provider-v1\n{}\n{}", self.name, self.base_url);
        for (name, value) in &self.headers {
            identity.push_str(&format!("\n{name}: {value}"));
        }
        identity
    }
    /// `Ok(None)` when no key is configured (local servers); an error when a
    /// key variable is configured but empty in this process.
    pub fn api_key(&self) -> Result<Option<String>> {
        let Some(variable) = &self.key_env else {
            return Ok(None);
        };
        match std::env::var(variable)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            Some(key) => Ok(Some(key)),
            None if self.is_openrouter() => bail!(
                "set {OPENROUTER_KEY_ENV} to enable this paid OpenRouter request (local inspection and cached results need no key)"
            ),
            None => bail!(
                "provider {} reads its API key from {variable}, which is unset or empty in this process; set it, or re-add the provider without --key-env if the server needs no key",
                self.name
            ),
        }
    }
    pub fn key_present(&self) -> bool {
        self.key_env.as_ref().is_some_and(|variable| {
            std::env::var(variable).is_ok_and(|value| !value.trim().is_empty())
        })
    }
    pub fn warnings(&self) -> Vec<String> {
        insecure_http_warning(&self.base_url).into_iter().collect()
    }
    pub fn summary(&self) -> Value {
        if self.is_ondevice() {
            return crate::ondevice::provider_summary();
        }
        json!({
            "name": self.name,
            "builtin": self.is_openrouter(),
            "base_url": self.base_url,
            "key_env": self.key_env,
            "key_present": self.key_present(),
            // Header values may be private; only names are shown.
            "header_names": self.headers.keys().collect::<Vec<_>>(),
            "timeout_seconds": self.timeout.as_secs(),
            "tasks": if self.is_openrouter() { crate::models::TASKS.to_vec() } else { PORTABLE_TASKS.to_vec() },
            "openrouter_only_features": if self.is_openrouter() { json!("data policy routing, usage cost accounting, reasoning toggle") } else { json!("not sent: data policy routing (SKYWRITE_DATA_POLICY), usage accounting and reasoning fields are OpenRouter-only; this server's own retention applies") },
            "warnings": self.warnings(),
        })
    }
}

/// A resolved model reference: the provider that serves it and the model ID
/// sent to that provider verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRef {
    pub provider: Provider,
    pub model: String,
}
impl ModelRef {
    /// Canonical spelling: bare for OpenRouter, `NAME:MODEL` otherwise.
    pub fn reference(&self) -> String {
        if self.provider.is_openrouter() {
            self.model.clone()
        } else {
            format!("{}:{}", self.provider.name, self.model)
        }
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let valid = name.len() <= 32
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !valid {
        bail!(
            "provider name {name:?} must be 1-32 lowercase ASCII letters, digits, '_' or '-', starting with a letter or digit"
        );
    }
    Ok(())
}

/// Syntax only: `(None, id)` for a bare OpenRouter ID, `(Some(name), model)`
/// for an explicit provider. Provider existence is checked by [`resolve`].
pub fn split_reference(reference: &str) -> Result<(Option<&str>, &str)> {
    let Some((prefix, model)) = reference.split_once(':') else {
        return Ok((None, reference));
    };
    if prefix.contains('/') {
        return Ok((None, reference));
    }
    if validate_name(prefix).is_err() {
        bail!(
            "{reference:?} is not a model reference: {prefix:?} before the first ':' is not a provider name (lowercase letters, digits, '_' or '-'). OpenRouter IDs contain vendor/ (vendor/model or vendor/model:free); other providers use PROVIDER:MODEL"
        );
    }
    if model.is_empty() {
        bail!("model reference {reference:?} names provider {prefix} but no model");
    }
    Ok((Some(prefix), model))
}

/// Resolve a reference against the configured providers. An unknown provider
/// is an error naming the fix, never a silent fallback to OpenRouter.
pub fn resolve(reference: &str, configured: &BTreeMap<String, ProviderConfig>) -> Result<ModelRef> {
    let (name, model) = split_reference(reference)?;
    let provider = match name {
        None | Some(BUILTIN) => Provider::builtin(),
        Some(crate::ondevice::PROVIDER) => {
            crate::ondevice::spec(model)?;
            Provider::ondevice()
        }
        Some(name) => match configured.get(name) {
            Some(config) => Provider::from_config(name, config)?,
            None => {
                let hint = match configured.keys().collect::<Vec<_>>().as_slice() {
                    [only] => format!(" If {reference:?} is a model on provider {only}, write {only}:{reference}."),
                    [] => " No providers are configured; add one with skywrite providers add NAME --base-url URL.".into(),
                    names => format!(" Configured providers: {}.", names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ")),
                };
                bail!(
                    "unknown provider {name:?} in model reference {reference:?}. Bare OpenRouter IDs contain vendor/ (vendor/model or vendor/model:free).{hint}"
                )
            }
        },
    };
    Ok(ModelRef {
        provider,
        model: model.to_owned(),
    })
}

/// Generation tasks use OpenRouter-specific APIs and durable jobs.
pub fn check_task(task: &str, provider: &Provider) -> Result<()> {
    if provider.is_ondevice() && task != "transcription" {
        bail!(
            "provider {} runs on-device transcription models only, not {task}",
            provider.name
        );
    }
    if !provider.is_openrouter() && !PORTABLE_TASKS.contains(&task) {
        bail!(
            "{task} uses OpenRouter-specific generation APIs, so it supports only provider openrouter, not {}. Custom providers serve {}",
            provider.name,
            PORTABLE_TASKS.join(", ")
        );
    }
    Ok(())
}

pub fn validate_key_env(variable: &str) -> Result<()> {
    let mut bytes = variable.bytes();
    if variable.len() > 64
        || !bytes
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        || !bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        bail!(
            "--key-env takes the NAME of an environment variable (uppercase letters, digits, '_'), never the key itself"
        );
    }
    Ok(())
}

fn validate_header(name: &str, value: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    {
        bail!("header name {name:?} is not a valid HTTP token");
    }
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "host"
            | "content-type"
            | "content-length"
    ) || ["key", "token", "secret", "password"]
        .iter()
        .any(|word| lower.contains(word))
    {
        bail!(
            "header {name} looks like a credential or is set by Skywrite; secrets are never stored in config. Use --key-env VAR to send the key in VAR as a bearer token"
        );
    }
    if value.len() > 1024 || value.bytes().any(|b| b.is_ascii_control()) {
        bail!("header {name} value must be at most 1024 bytes without control characters");
    }
    Ok(())
}

/// Normalise and check one stored provider.
pub fn validate_config(name: &str, config: &ProviderConfig) -> Result<ProviderConfig> {
    validate_name(name)?;
    if RESERVED.contains(&name) {
        bail!(
            "provider name {name} is reserved (built-in provider or an OpenRouter model variant)"
        );
    }
    // The URL is not echoed: an unparsable one may still carry credentials.
    let url = url::Url::parse(&config.base_url).map_err(|error| {
        anyhow::anyhow!("provider {name}: invalid base URL ({error}); use http(s)://HOST[:PORT]/PATH without credentials")
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        bail!("provider {name}: base URL must be http:// or https:// with a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("provider {name}: base URL must not contain credentials; use --key-env");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("provider {name}: base URL must not have a query or fragment");
    }
    if config.base_url.len() > 512 {
        bail!("provider {name}: base URL exceeds 512 bytes");
    }
    if let Some(variable) = &config.key_env {
        validate_key_env(variable)?;
        if variable == OPENROUTER_KEY_ENV {
            bail!(
                "provider {name}: {OPENROUTER_KEY_ENV} belongs to the built-in openrouter provider and is never sent elsewhere; put this provider's key in its own variable"
            );
        }
    }
    if config.headers.len() > MAX_HEADERS {
        bail!("provider {name}: at most {MAX_HEADERS} headers");
    }
    let mut seen = std::collections::BTreeSet::new();
    for (header, value) in &config.headers {
        validate_header(header, value)?;
        if !seen.insert(header.to_ascii_lowercase()) {
            bail!("provider {name}: duplicate header {header}");
        }
    }
    if let Some(seconds) = config.timeout_seconds
        && !(1..=MAX_TIMEOUT_SECONDS).contains(&seconds)
    {
        bail!("provider {name}: timeout must be 1..{MAX_TIMEOUT_SECONDS} seconds");
    }
    let mut normalized = config.clone();
    normalized.base_url = url.as_str().trim_end_matches('/').to_owned();
    Ok(normalized)
}

pub fn validate_all(configs: &BTreeMap<String, ProviderConfig>) -> Result<()> {
    if configs.len() > MAX_PROVIDERS {
        bail!("at most {MAX_PROVIDERS} providers can be configured");
    }
    for (name, config) in configs {
        validate_config(name, config)?;
    }
    Ok(())
}

fn private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                // Carrier-grade NAT range, used by Tailscale and similar overlays.
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| private_address(IpAddr::V4(v4)))
        }
    }
}

/// Plain HTTP is expected for loopback and private-network servers only; a
/// public plain-HTTP URL sends media and any key unencrypted.
pub fn insecure_http_warning(base_url: &str) -> Option<String> {
    let url = url::Url::parse(base_url).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let private = match url.host()? {
        url::Host::Ipv4(v4) => private_address(IpAddr::V4(v4)),
        url::Host::Ipv6(v6) => private_address(IpAddr::V6(v6)),
        url::Host::Domain(domain) => {
            let domain = domain.to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost")
        }
    };
    (!private).then(|| {
        format!(
            "{base_url} uses plain http:// to an address that is not loopback or a private network literal; media, prompts and any key travel unencrypted. Use https:// unless this host is on a trusted network"
        )
    })
}

/// Every configured provider plus the built-in one, by name.
pub fn all() -> Result<Vec<Provider>> {
    let mut providers = vec![Provider::builtin(), Provider::ondevice()];
    for (name, config) in crate::workspace::provider_configs()? {
        providers.push(Provider::from_config(&name, &config)?);
    }
    Ok(providers)
}

pub fn get(name: &str) -> Result<Provider> {
    if name == BUILTIN {
        return Ok(Provider::builtin());
    }
    if name == crate::ondevice::PROVIDER {
        return Ok(Provider::ondevice());
    }
    validate_name(name)?;
    let configs = crate::workspace::provider_configs()?;
    let config = configs
        .get(name)
        .with_context(|| format!("no provider named {name}; see skywrite providers list"))?;
    Provider::from_config(name, config)
}

/// Resolve a reference, reading the personal config only when it names a
/// custom provider (OpenRouter users never depend on the providers section).
pub fn resolve_reference(reference: &str) -> Result<ModelRef> {
    match split_reference(reference)? {
        (None | Some(BUILTIN | crate::ondevice::PROVIDER), _) => {
            resolve(reference, &BTreeMap::new())
        }
        (Some(_), _) => resolve(reference, &crate::workspace::provider_configs()?),
    }
}

#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    /// Register an OpenAI-compatible server (stores the key's variable NAME, never a key).
    Add {
        name: String,
        /// API base, e.g. http://127.0.0.1:8080/v1 (requests go to BASE/chat/completions).
        #[arg(long)]
        base_url: String,
        /// Environment variable holding the API key, sent as a bearer token. Omit for keyless local servers.
        #[arg(long, value_name = "VAR")]
        key_env: Option<String>,
        /// Extra non-secret request header, NAME=VALUE (repeatable).
        #[arg(long = "header", value_name = "NAME=VALUE")]
        headers: Vec<String>,
        /// Per-request deadline in seconds (default 600; small CPU models are slow).
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
    },
    /// List the built-in OpenRouter provider and configured providers (no network).
    List,
    /// Show one provider (no network; key presence only, never its value).
    Show { name: String },
    /// Remove a configured provider (refused while a saved default uses it).
    Remove { name: String },
}

pub fn execute(command: ProvidersCommand) -> Result<Value> {
    let mut result = match command {
        ProvidersCommand::Add {
            name,
            base_url,
            key_env,
            headers,
            timeout,
        } => {
            let mut parsed = BTreeMap::new();
            for header in headers {
                let (header, value) = header
                    .split_once('=')
                    .context("--header takes NAME=VALUE")?;
                if parsed.insert(header.to_owned(), value.to_owned()).is_some() {
                    bail!("duplicate --header {header}");
                }
            }
            let config = ProviderConfig {
                base_url,
                key_env,
                headers: parsed,
                timeout_seconds: timeout,
            };
            let stored = crate::workspace::add_provider(&name, &config)?;
            let provider = Provider::from_config(&name, &stored)?;
            for warning in provider.warnings() {
                eprintln!("Warning: {warning}");
            }
            json!({"added": provider.summary(), "example": format!("skywrite describe frames SOURCE --every 30 --model {name}:MODEL"), "models_command": format!("skywrite models list --provider {name}")})
        }
        ProvidersCommand::List => {
            json!({"providers": all()?.iter().map(Provider::summary).collect::<Vec<_>>()})
        }
        ProvidersCommand::Show { name } => json!({"provider": get(&name)?.summary()}),
        ProvidersCommand::Remove { name } => {
            if name == BUILTIN || name == crate::ondevice::PROVIDER {
                bail!(
                    "the built-in provider {name} cannot be removed{}",
                    if name == BUILTIN {
                        ""
                    } else {
                        "; delete a downloaded model with skywrite models remove ondevice:MODEL"
                    }
                );
            }
            validate_name(&name)?;
            crate::workspace::remove_provider(&name)?;
            json!({"removed": name})
        }
    };
    result["config_path"] = json!(crate::workspace::personal_home()?.join("config.json"));
    result["syntax"] = json!(
        "Model references are PROVIDER:MODEL (local:qwen3.5-0.8b, local:qwen3:0.8b). A bare ID, or one with '/' before its first ':' (vendor/model:free), is an OpenRouter model."
    );
    result["network_requests"] = json!(false);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(name: &str, base: &str) -> BTreeMap<String, ProviderConfig> {
        BTreeMap::from([(
            name.to_owned(),
            ProviderConfig {
                base_url: base.into(),
                key_env: None,
                headers: BTreeMap::new(),
                timeout_seconds: None,
            },
        )])
    }

    #[test]
    fn openrouter_ids_and_variants_stay_bare() {
        for id in [
            "openai/gpt-6-luna",
            "meta-llama/llama-3.3-70b:free",
            "vendor/model:thinking",
            "gpt-4o",
            "openrouter/auto",
        ] {
            assert_eq!(split_reference(id).unwrap(), (None, id));
            let resolved = resolve(id, &BTreeMap::new()).unwrap();
            assert!(resolved.provider.is_openrouter());
            assert_eq!(resolved.model, id);
            assert_eq!(resolved.reference(), id);
        }
        let explicit = resolve("openrouter:vendor/model:free", &BTreeMap::new()).unwrap();
        assert!(explicit.provider.is_openrouter());
        assert_eq!(explicit.reference(), "vendor/model:free");
    }

    #[test]
    fn provider_prefix_splits_at_first_colon_and_keeps_the_rest_verbatim() {
        let providers = configured("local", "http://127.0.0.1:8080/v1/");
        let resolved = resolve("local:qwen3:0.8b", &providers).unwrap();
        assert_eq!(resolved.provider.name, "local");
        assert_eq!(resolved.provider.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(resolved.model, "qwen3:0.8b");
        assert_eq!(resolved.reference(), "local:qwen3:0.8b");
        let slashed = resolve("local:lmstudio-community/Qwen3-VL-2B-GGUF", &providers).unwrap();
        assert_eq!(slashed.model, "lmstudio-community/Qwen3-VL-2B-GGUF");
        assert_eq!(
            resolved.provider.timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECONDS)
        );
    }

    #[test]
    fn unknown_or_malformed_prefixes_fail_with_the_fix() {
        let providers = configured("local", "http://127.0.0.1:8080/v1");
        let error = resolve("qwen3:0.8b", &providers).unwrap_err().to_string();
        assert!(error.contains("unknown provider \"qwen3\""), "{error}");
        assert!(error.contains("local:qwen3:0.8b"), "{error}");
        let error = resolve("qwen3:0.8b", &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("providers add"), "{error}");
        assert!(split_reference("Local:model").is_err());
        assert!(split_reference("local:").is_err());
        assert!(split_reference(":model").is_err());
    }

    #[test]
    fn names_urls_keys_and_headers_are_checked() {
        let config = |base: &str| ProviderConfig {
            base_url: base.into(),
            key_env: None,
            headers: BTreeMap::new(),
            timeout_seconds: None,
        };
        for name in ["openrouter", "free", "thinking"] {
            assert!(validate_config(name, &config("http://127.0.0.1/v1")).is_err());
        }
        for name in ["Local", "a/b", "a:b", "", &"x".repeat(33)] {
            assert!(validate_config(name, &config("http://127.0.0.1/v1")).is_err());
        }
        for base in [
            "ftp://host/v1",
            "http://user:pw@127.0.0.1/v1",
            "http://127.0.0.1/v1?x=1",
            "not a url",
        ] {
            assert!(validate_config("local", &config(base)).is_err(), "{base}");
        }
        let mut keyed = config("https://api.example.com/v1");
        keyed.key_env = Some("sk-live-abc".into());
        assert!(validate_config("x", &keyed).is_err());
        keyed.key_env = Some("EXAMPLE_API_KEY".into());
        assert!(validate_config("x", &keyed).is_ok());
        // Even OpenRouter registered as a custom provider uses its own variable.
        keyed.key_env = Some(OPENROUTER_KEY_ENV.into());
        assert!(validate_config("x", &keyed).is_err());
        // An unparsable URL is never echoed (it may carry credentials).
        let error = validate_config("x", &config("https://user:SECRET@host:bad/v1"))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("SECRET"), "{error}");
        for header in ["Authorization", "X-Api-Key", "x-auth-token", "Cookie"] {
            let mut headed = config("https://api.example.com/v1");
            headed.headers.insert(header.into(), "v".into());
            assert!(validate_config("x", &headed).is_err(), "{header}");
        }
        let mut headed = config("https://api.example.com/v1");
        headed
            .headers
            .insert("X-Org".into(), "team\r\nInjected: 1".into());
        assert!(validate_config("x", &headed).is_err());
        let mut timed = config("http://127.0.0.1/v1");
        timed.timeout_seconds = Some(0);
        assert!(validate_config("x", &timed).is_err());
        timed.timeout_seconds = Some(MAX_TIMEOUT_SECONDS + 1);
        assert!(validate_config("x", &timed).is_err());
    }

    #[test]
    fn plain_http_is_quiet_only_for_loopback_and_private_networks() {
        for quiet in [
            "http://127.0.0.1:8080/v1",
            "http://localhost:11434/v1",
            "http://10.0.0.5/v1",
            "http://192.168.1.20:1234/v1",
            "http://172.16.0.1/v1",
            "http://100.101.1.2/v1",
            "http://[::1]:8080/v1",
            "http://[fd00::1]/v1",
            "https://api.example.com/v1",
        ] {
            assert!(insecure_http_warning(quiet).is_none(), "{quiet}");
        }
        for loud in [
            "http://api.example.com/v1",
            "http://8.8.8.8/v1",
            "http://mac-mini.local:8080/v1",
        ] {
            assert!(insecure_http_warning(loud).is_some(), "{loud}");
        }
    }

    #[test]
    fn identity_separates_providers_sharing_a_base_url() {
        let builtin = Provider::builtin();
        let custom = resolve(
            "ortest:vendor/model",
            &configured("ortest", OPENROUTER_BASE),
        )
        .unwrap()
        .provider;
        assert_eq!(custom.base_url, builtin.base_url);
        assert_ne!(custom.identity(), builtin.identity());
        let mut headed = custom.clone();
        headed.headers.insert("X-Org".into(), "a".into());
        assert_ne!(headed.identity(), custom.identity());
    }

    #[test]
    fn generation_tasks_refuse_custom_providers() {
        let custom = resolve("local:m", &configured("local", "http://127.0.0.1/v1"))
            .unwrap()
            .provider;
        for task in PORTABLE_TASKS {
            check_task(task, &custom).unwrap();
        }
        let error = check_task("image-generation", &custom)
            .unwrap_err()
            .to_string();
        assert!(error.contains("only provider openrouter"), "{error}");
        check_task("image-generation", &Provider::builtin()).unwrap();
    }
}
